use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, Once, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub trait Ptt: Send {
    fn set(&mut self, on: bool) -> io::Result<()>;
    fn describe(&self) -> String;
}

#[derive(Clone, Debug, PartialEq)]
pub enum PttSpec {
    Vox,
    Rts(String),
    Dtr(String),
    Cm108 { path: String, gpio: u8 },
    Rigctld(String),
}

pub const RIGCTLD_DEFAULT: &str = "127.0.0.1:4532";

impl PttSpec {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (kind, rest) = s.split_once(':').unwrap_or((s, ""));
        let path = |what: &str| {
            if rest.starts_with('/') {
                Ok(rest.to_string())
            } else {
                Err(format!("--ptt {what}: needs a device path, e.g. {what}:/dev/ttyACM0"))
            }
        };
        match kind {
            "vox" if rest.is_empty() => Ok(Self::Vox),
            "rts" => path("rts").map(Self::Rts),
            "dtr" => path("dtr").map(Self::Dtr),
            "cm108" => {
                let (p, g) = match rest.rsplit_once(':') {
                    Some((p, g)) => (p, g.parse::<u8>().map_err(|e| format!("--ptt cm108 gpio: {e}"))?),
                    None => (rest, 3),
                };
                if !p.starts_with('/') || !(1..=8).contains(&g) {
                    return Err("--ptt cm108:/dev/hidrawN[:GPIO 1-8]".into());
                }
                Ok(Self::Cm108 { path: p.into(), gpio: g })
            }
            "rigctld" => Ok(Self::Rigctld(if rest.is_empty() { RIGCTLD_DEFAULT.into() } else { rest.into() })),
            _ => Err(format!("--ptt {s:?}: expected vox, rts:DEV, dtr:DEV, cm108:DEV[:GPIO] or rigctld[:HOST:PORT]")),
        }
    }

    pub fn open(&self) -> io::Result<Box<dyn Ptt>> {
        Ok(match self {
            Self::Vox => Box::new(Vox),
            Self::Rts(p) => Box::new(Serial::open(p, libc::TIOCM_RTS)?),
            Self::Dtr(p) => Box::new(Serial::open(p, libc::TIOCM_DTR)?),
            Self::Cm108 { path, gpio } => Box::new(Cm108::open(path, *gpio)?),
            Self::Rigctld(a) => Box::new(Rigctld::open(a)?),
        })
    }
}

pub struct Vox;

impl Ptt for Vox {
    fn set(&mut self, _: bool) -> io::Result<()> {
        Ok(())
    }

    fn describe(&self) -> String {
        "VOX (no PTT line)".into()
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
type Ioctl = libc::Ioctl;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
type Ioctl = libc::c_ulong;

pub struct Serial {
    f: File,
    path: String,
    line: libc::c_int,
}

impl Serial {
    pub fn open(path: &str, line: libc::c_int) -> io::Result<Self> {
        let f = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(path)?;
        if !f.is_terminal() {
            return Err(io::Error::other(format!("{path} is not a serial port")));
        }
        let s = Self { f, path: path.into(), line };
        s.modem(libc::TIOCMBIC, libc::TIOCM_RTS | libc::TIOCM_DTR)?;
        Ok(s)
    }

    fn modem(&self, req: Ioctl, bits: libc::c_int) -> io::Result<()> {
        if unsafe { libc::ioctl(self.f.as_raw_fd(), req, &bits) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn lines(&self) -> io::Result<libc::c_int> {
        let mut bits: libc::c_int = 0;
        if unsafe { libc::ioctl(self.f.as_raw_fd(), libc::TIOCMGET, &mut bits) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(bits)
    }
}

impl Ptt for Serial {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.modem(if on { libc::TIOCMBIS } else { libc::TIOCMBIC }, self.line)?;
        if (self.lines()? & self.line != 0) != on {
            return Err(io::Error::other(format!("{}: modem line did not follow", self.path)));
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} on {} (other line held low)", if self.line == libc::TIOCM_RTS { "RTS" } else { "DTR" }, self.path)
    }
}

pub struct Cm108 {
    f: File,
    path: String,
    gpio: u8,
}

pub fn cm108_report(gpio: u8, on: bool) -> [u8; 5] {
    let m = 1u8 << (gpio - 1);
    [0, 0, if on { m } else { 0 }, m, 0]
}

#[cfg(not(any(target_os = "freebsd", target_os = "dragonfly")))]
pub fn is_hidraw(path: &str) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(m) = std::fs::metadata(path) else { return false };
    if !m.file_type().is_char_device() {
        return false;
    }
    let (major, minor) = (libc::major(m.rdev()), libc::minor(m.rdev()));
    std::fs::read_link(format!("/sys/dev/char/{major}:{minor}/subsystem"))
        .is_ok_and(|l| l.file_name().is_some_and(|n| n == "hidraw"))
}

// FreeBSD's hidraw(4) speaks the Linux hidraw protocol; uhid(4) does not, so only accept hidraw.
#[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
pub fn is_hidraw(path: &str) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(m) = std::fs::metadata(path) else { return false };
    if !m.file_type().is_char_device() {
        return false;
    }
    let mut buf = [0 as libc::c_char; 64];
    // SAFETY: buf is valid for its length and devname_r NUL-terminates on success.
    let p =
        unsafe { libc::devname_r(m.rdev() as libc::dev_t, libc::S_IFCHR, buf.as_mut_ptr(), buf.len() as libc::c_int) };
    !p.is_null() && unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes().starts_with(b"hidraw")
}

impl Cm108 {
    pub fn open(path: &str, gpio: u8) -> io::Result<Self> {
        let f = OpenOptions::new().write(true).open(path)?;
        if !is_hidraw(path) {
            say!("warning: {path} is not a hidraw device; CM108 PTT may not work");
        }
        let mut c = Self { f, path: path.into(), gpio };
        c.set(false)?;
        Ok(c)
    }
}

impl Ptt for Cm108 {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.f.write_all(&cm108_report(self.gpio, on))
    }

    fn describe(&self) -> String {
        format!("CM108 GPIO{} on {}", self.gpio, self.path)
    }
}

pub struct Rigctld {
    link: Option<(TcpStream, BufReader<TcpStream>)>,
    addr: String,
}

impl Rigctld {
    pub fn open(addr: &str) -> io::Result<Self> {
        let mut c = Self { link: Some(Self::connect(addr)?), addr: addr.into() };
        c.set(false)?;
        Ok(c)
    }

    fn connect(addr: &str) -> io::Result<(TcpStream, BufReader<TcpStream>)> {
        let a = addr.to_socket_addrs()?.next().ok_or_else(|| io::Error::other(format!("{addr}: no address")))?;
        let w = TcpStream::connect_timeout(&a, Duration::from_secs(2))?;
        w.set_read_timeout(Some(Duration::from_secs(1)))?;
        w.set_write_timeout(Some(Duration::from_secs(1)))?;
        w.set_nodelay(true)?;
        Ok((w.try_clone()?, BufReader::new(w)))
    }

    fn exchange(&mut self, cmd: &[u8]) -> io::Result<String> {
        let (w, r) = match &mut self.link {
            Some(l) => l,
            None => self.link.insert(Self::connect(&self.addr)?),
        };
        w.write_all(cmd)?;
        let mut l = String::new();
        if r.read_line(&mut l)? == 0 || !l.ends_with('\n') {
            return Err(io::Error::other(format!("rigctld {} closed", self.addr)));
        }
        Ok(l)
    }
}

impl Ptt for Rigctld {
    fn set(&mut self, on: bool) -> io::Result<()> {
        let l = self.exchange(if on { b"T 1\n" } else { b"T 0\n" }).inspect_err(|_| self.link = None)?;
        match l.trim() {
            "RPRT 0" => Ok(()),
            r => Err(io::Error::other(format!("rigctld {}: {r}", self.addr))),
        }
    }

    fn describe(&self) -> String {
        format!("rigctld at {}", self.addr)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PttTiming {
    pub lead: Duration,
    pub tail: Duration,
    pub max_on: Duration,
    pub stall: Duration,
}

impl Default for PttTiming {
    fn default() -> Self {
        Self {
            lead: Duration::ZERO,
            tail: Duration::from_millis(100),
            max_on: Duration::from_secs(30),
            stall: Duration::from_millis(500),
        }
    }
}

struct Watched {
    id: u64,
    shutdown: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

static WATCHED: Mutex<Vec<Watched>> = Mutex::new(Vec::new());
static RELEASED: Condvar = Condvar::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static PANICKED: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

thread_local! {
    static PTT_THREAD: Cell<bool> = const { Cell::new(false) };
}

fn watched() -> std::sync::MutexGuard<'static, Vec<Watched>> {
    WATCHED.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn panicked() -> bool {
    PANICKED.load(Ordering::SeqCst)
}

pub fn wait_released(timeout: Duration) -> bool {
    let held = |w: &mut Vec<Watched>| !w.iter().all(|w| w.released.load(Ordering::SeqCst));
    !RELEASED.wait_timeout_while(watched(), timeout, held).unwrap_or_else(PoisonError::into_inner).1.timed_out()
}

fn mark_released(released: &AtomicBool, on: bool) {
    released.store(on, Ordering::SeqCst);
    if on {
        drop(watched());
        RELEASED.notify_all();
    }
}

fn guard_panics(shutdown: Arc<AtomicBool>, released: Arc<AtomicBool>) -> u64 {
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICKED.store(true, Ordering::SeqCst);
            for w in watched().iter() {
                w.shutdown.store(true, Ordering::SeqCst);
            }
            if !PTT_THREAD.get() {
                wait_released(Duration::from_millis(500));
            }
            prev(info);
        }));
    });
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    watched().push(Watched { id, shutdown, released });
    id
}

pub enum Event {
    On,
    Off,
    Watchdog,
    Stalled,
}

pub struct PttCtl {
    tx: Option<Sender<bool>>,
    thread: Option<JoinHandle<io::Result<()>>>,
    shutdown: Arc<AtomicBool>,
    forced: Arc<AtomicBool>,
    watch: Option<u64>,
}

struct Keyer {
    dev: Box<dyn Ptt>,
    keyed: bool,
    released: Arc<AtomicBool>,
}

impl Keyer {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.released.store(false, Ordering::SeqCst);
        let r = self.dev.set(on);
        self.keyed = on || r.is_err();
        mark_released(&self.released, !self.keyed);
        r
    }

    fn release(&mut self) -> bool {
        for _ in 0..3 {
            if self.dev.set(false).is_ok() {
                self.keyed = false;
                mark_released(&self.released, true);
                return true;
            }
        }
        false
    }
}

impl Drop for Keyer {
    fn drop(&mut self) {
        if self.keyed {
            self.release();
        }
    }
}

impl PttCtl {
    pub fn spawn(dev: Box<dyn Ptt>, timing: PttTiming, shutdown: Arc<AtomicBool>) -> Self {
        let released = Arc::new(AtomicBool::new(true));
        let watch = guard_panics(shutdown.clone(), released.clone());
        let (ltx, lrx) = mpsc::channel::<String>();
        spawn("ptt-log", move || {
            for m in lrx {
                say!("{m}");
            }
        });
        let mut c = Self::run(dev, timing, shutdown, released, move |e| {
            let m = match e {
                Event::Watchdog => format!("PTT watchdog: keyed longer than {:?}, released", timing.max_on),
                Event::Stalled => "PTT released: audio loop stalled".into(),
                _ => return,
            };
            let _ = ltx.send(m);
        });
        c.watch = Some(watch);
        c
    }

    pub fn spawn_with(
        dev: Box<dyn Ptt>,
        t: PttTiming,
        shutdown: Arc<AtomicBool>,
        log: impl FnMut(Event) + Send + 'static,
    ) -> Self {
        Self::run(dev, t, shutdown, Arc::new(AtomicBool::new(true)), log)
    }

    fn run(
        dev: Box<dyn Ptt>,
        t: PttTiming,
        shutdown: Arc<AtomicBool>,
        released: Arc<AtomicBool>,
        mut log: impl FnMut(Event) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<bool>();
        let (stop, forced) = (shutdown.clone(), Arc::new(AtomicBool::new(false)));
        let forced_off = forced.clone();
        let thread = spawn("ptt", move || {
            PTT_THREAD.set(true);
            let mut k = Keyer { dev, keyed: false, released };
            let (mut want, mut tripped, mut stalled) = (false, false, false);
            let (mut off_at, mut on_since, mut last) = (None::<Instant>, Instant::now(), Instant::now());
            let mut fault = None;
            loop {
                let msg = match k.keyed {
                    true => rx.recv_timeout(Duration::from_millis(2)),
                    false => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                };
                match msg {
                    Ok(w) => {
                        last = Instant::now();
                        stalled = false;
                        if w != want {
                            want = w;
                            off_at = (!w && k.keyed).then(|| last + t.tail);
                            tripped &= w;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let now = Instant::now();
                let stall = k.keyed && !stalled && now - last > t.stall;
                let trip = k.keyed && !tripped && now - on_since > t.max_on;
                stalled |= stall;
                tripped |= trip;
                forced_off.store(tripped || stalled, Ordering::SeqCst);
                let target = !tripped && !stalled && (want || off_at.is_some_and(|o| now < o));
                let change = target != k.keyed;
                if change {
                    if let Err(e) = k.set(target) {
                        fault = Some(e);
                        break;
                    }
                    if k.keyed {
                        on_since = now;
                    }
                }
                if stall {
                    log(Event::Stalled);
                }
                if trip {
                    log(Event::Watchdog);
                }
                if change {
                    log(if k.keyed { Event::On } else { Event::Off });
                }
            }
            drop(rx);
            let keyed = k.keyed;
            let released = !keyed || k.release();
            k.keyed = false;
            if keyed {
                log(Event::Off);
            }
            let state = if released { "released" } else { "COULD NOT RELEASE PTT" };
            match fault {
                Some(e) => Err(io::Error::new(e.kind(), format!("PTT {} failed: {e}; {state}", k.dev.describe()))),
                None if !released => Err(io::Error::other(format!("COULD NOT RELEASE PTT on {}", k.dev.describe()))),
                None => Ok(()),
            }
        });
        Self { tx: Some(tx), thread: Some(thread), shutdown, forced, watch: None }
    }

    pub fn set(&mut self, on: bool) -> io::Result<()> {
        if self.tx.as_ref().is_some_and(|tx| tx.send(on).is_ok()) {
            return Ok(());
        }
        self.finish()?;
        if self.shutdown.load(Ordering::SeqCst) { Ok(()) } else { Err(io::Error::other("PTT thread stopped")) }
    }

    pub fn forced_off(&self) -> bool {
        self.forced.load(Ordering::SeqCst)
    }

    pub fn finish(&mut self) -> io::Result<()> {
        self.tx.take();
        let r = match self.thread.take().map(JoinHandle::join) {
            Some(Ok(r)) => r,
            Some(Err(_)) => Err(io::Error::other("PTT thread panicked")),
            None => Ok(()),
        };
        if let Some(id) = self.watch.take() {
            watched().retain(|w| w.id != id);
        }
        r
    }
}

fn spawn<T: Send + 'static>(name: &str, f: impl FnOnce() -> T + Send + 'static) -> JoinHandle<T> {
    thread::Builder::new().name(name.into()).spawn(f).expect("spawn PTT thread")
}

impl Drop for PttCtl {
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            say!("{e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stuck;

    impl Ptt for Stuck {
        fn set(&mut self, on: bool) -> io::Result<()> {
            if on { Ok(()) } else { Err(io::Error::other("stuck")) }
        }

        fn describe(&self) -> String {
            "stuck".into()
        }
    }

    #[test]
    fn the_panic_hook_is_installed_once_and_releases_every_controller() {
        let stops: Vec<Arc<AtomicBool>> = (0..4).map(|_| Arc::default()).collect();
        let mut c: Vec<PttCtl> =
            stops.iter().map(|s| PttCtl::spawn(Box::new(Stuck), PttTiming::default(), s.clone())).collect();
        for p in &mut c {
            p.set(true).unwrap();
        }
        thread::sleep(Duration::from_millis(30));
        let t0 = Instant::now();
        assert!(thread::spawn(|| panic!("test panic")).join().is_err());
        let took = t0.elapsed();
        assert!(panicked());
        assert!(stops.iter().all(|s| s.load(Ordering::SeqCst)));
        assert!(took >= Duration::from_millis(450) && took < Duration::from_millis(1500), "{took:?}");
        for mut p in c {
            assert!(p.finish().unwrap_err().to_string().contains("COULD NOT RELEASE"));
        }
        assert!(watched().is_empty());
    }
}
