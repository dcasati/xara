use crate::command::{Command, parse};
use crate::writer::Writer;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub const MAX_LINE: usize = 1024;
pub const DATA_CHUNK: usize = 65536;
pub const INPUT_QUEUE: usize = 64;
pub(crate) const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const WAKE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub enum Input {
    Command(Command),
    Wrong(String),
    Data(Vec<u8>),
    Connected,
    Closed,
}

#[derive(Clone, Copy)]
enum Port {
    Cmd,
    Data,
}

#[derive(Default)]
struct Clients {
    cmd: Option<Writer>,
    data: Option<Writer>,
}

impl Clients {
    fn slot(&mut self, p: Port) -> &mut Option<Writer> {
        match p {
            Port::Cmd => &mut self.cmd,
            Port::Data => &mut self.data,
        }
    }
}

type Shared = Arc<Mutex<Clients>>;

#[derive(Default)]
struct Hold {
    on: Mutex<bool>,
    cv: Condvar,
}

impl Hold {
    fn set(&self, on: bool) {
        *self.on.lock().unwrap() = on;
        self.cv.notify_all();
    }

    fn wait(&self) {
        drop(self.cv.wait_while(self.on.lock().unwrap(), |on| *on).unwrap());
    }
}

pub struct Host {
    pub input: Receiver<Input>,
    clients: Shared,
    hold: Arc<Hold>,
    closed: Arc<AtomicBool>,
    pub cmd_addr: SocketAddr,
    pub data_addr: SocketAddr,
}

impl Host {
    pub fn bind(addr: &str, port: u16) -> io::Result<Self> {
        let cl = bind_fixed(addr, port)?;
        let data_port = match port {
            0 => 0,
            p => p.checked_add(1).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "port must be < 65535"))?,
        };
        let dl = bind_fixed(addr, data_port)?;
        let (tx, input) = sync_channel(INPUT_QUEUE);
        let clients = Shared::default();
        let hold = Arc::<Hold>::default();
        let closed = Arc::<AtomicBool>::default();
        let h = Self {
            input,
            clients: clients.clone(),
            hold: hold.clone(),
            closed: closed.clone(),
            cmd_addr: cl.local_addr()?,
            data_addr: dl.local_addr()?,
        };
        serve(cl, Port::Cmd, clients.clone(), tx.clone(), hold.clone(), closed.clone())?;
        serve(dl, Port::Data, clients, tx, hold, closed)?;
        Ok(h)
    }

    pub fn hold_data(&self, on: bool) {
        self.hold.set(on);
    }

    pub fn line(&self, s: &str) {
        self.send(Port::Cmd, format!("{s}\r").as_bytes());
    }

    pub fn data(&self, b: &[u8]) {
        self.send(Port::Data, b);
    }

    fn send(&self, p: Port, b: &[u8]) {
        if let Some(w) = self.clients.lock().unwrap().slot(p) {
            w.send(b);
        }
    }

    pub fn has_cmd_client(&self) -> bool {
        self.clients.lock().unwrap().cmd.is_some()
    }

    pub fn has_data_client(&self) -> bool {
        self.clients.lock().unwrap().data.is_some()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.hold.set(false);
        self.closed.store(true, Ordering::SeqCst);
        wake(self.cmd_addr);
        wake(self.data_addr);
    }
}

fn wake(mut a: SocketAddr) {
    if a.ip().is_unspecified() {
        a.set_ip(match a.ip() {
            IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
            IpAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
        });
    }
    let _ = TcpStream::connect_timeout(&a, WAKE_TIMEOUT);
}

fn set_opt(s: &impl AsRawFd, opt: libc::c_int, on: bool) -> io::Result<()> {
    let v: libc::c_int = on.into();
    let len = std::mem::size_of_val(&v) as libc::socklen_t;
    let r = unsafe { libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, opt, (&raw const v).cast(), len) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn pause(l: &TcpListener, _: SocketAddr) -> io::Result<()> {
    if unsafe { libc::shutdown(l.as_raw_fd(), libc::SHUT_RD) } != 0 {
        return Err(io::Error::last_os_error());
    }
    set_opt(l, libc::SO_REUSEADDR, false)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn resume(l: &TcpListener, _: SocketAddr) -> io::Result<()> {
    set_opt(l, libc::SO_REUSEADDR, true)?;
    if unsafe { libc::listen(l.as_raw_fd(), 128) } != 0 {
        let e = io::Error::last_os_error();
        let _ = set_opt(l, libc::SO_REUSEADDR, false);
        return Err(e);
    }
    Ok(())
}

// Elsewhere shutdown() on a listening socket fails, so keep the port reserved by binding an
// idle twin (via SO_REUSEPORT) and swapping it onto the listener's fd, which closes the
// listener and refuses new connections. Listening on the twin resumes service.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn pause(l: &TcpListener, addr: SocketAddr) -> io::Result<()> {
    set_opt(l, libc::SO_REUSEPORT, true)?;
    let r = twin(addr).and_then(|t| {
        let fd = l.as_raw_fd();
        if unsafe { libc::dup2(t.as_raw_fd(), fd) } < 0
            || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    });
    let _ = set_opt(l, libc::SO_REUSEPORT, false);
    r
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn resume(l: &TcpListener, _: SocketAddr) -> io::Result<()> {
    if unsafe { libc::listen(l.as_raw_fd(), 128) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn twin(addr: SocketAddr) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let family = if addr.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let s = unsafe { OwnedFd::from_raw_fd(fd) };
    set_opt(&s, libc::SO_REUSEADDR, true)?;
    set_opt(&s, libc::SO_REUSEPORT, true)?;
    let r = match addr {
        SocketAddr::V4(a) => {
            let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sa.sin_len = size_of::<libc::sockaddr_in>() as u8;
            sa.sin_family = libc::AF_INET as libc::sa_family_t;
            sa.sin_port = a.port().to_be();
            sa.sin_addr.s_addr = u32::from(*a.ip()).to_be();
            unsafe { libc::bind(fd, (&raw const sa).cast(), size_of_val(&sa) as libc::socklen_t) }
        }
        SocketAddr::V6(a) => {
            let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sa.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
            sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sa.sin6_port = a.port().to_be();
            sa.sin6_flowinfo = a.flowinfo();
            sa.sin6_addr.s6_addr = a.ip().octets();
            sa.sin6_scope_id = a.scope_id();
            unsafe { libc::bind(fd, (&raw const sa).cast(), size_of_val(&sa) as libc::socklen_t) }
        }
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    set_opt(&s, libc::SO_REUSEPORT, false)?;
    Ok(s)
}

fn bind_fixed(addr: &str, port: u16) -> io::Result<TcpListener> {
    if port != 0 {
        return TcpListener::bind((addr, port));
    }
    for _ in 0..16 {
        let p = TcpListener::bind((addr, 0))?.local_addr()?.port();
        if let Ok(l) = TcpListener::bind((addr, p)) {
            return Ok(l);
        }
    }
    Err(io::Error::new(io::ErrorKind::AddrInUse, "no free port"))
}

fn serve(
    l: TcpListener,
    port: Port,
    clients: Shared,
    tx: SyncSender<Input>,
    hold: Arc<Hold>,
    closed: Arc<AtomicBool>,
) -> io::Result<()> {
    let name = match port {
        Port::Cmd => "host-cmd-accept",
        Port::Data => "host-data-accept",
    };
    let closed = move || closed.load(Ordering::SeqCst);
    let addr = l.local_addr()?;
    thread::Builder::new().name(name.into()).spawn(move || {
        while !closed() {
            let s = match l.accept() {
                Ok((s, _)) => s,
                Err(_) => {
                    thread::sleep(ACCEPT_BACKOFF);
                    continue;
                }
            };
            if closed() {
                return;
            }
            if let Err(e) = pause(&l, addr) {
                crate::say(format_args!("{name} cannot refuse a second client: {e}"));
            }
            if attach(s, port, &clients, &tx, &hold).is_err() || closed() {
                return;
            }
            if let Err(e) = resume(&l, addr) {
                crate::say(format_args!("{name} cannot listen again, retrying: {e}"));
                while !closed() && resume(&l, addr).is_err() {
                    thread::sleep(ACCEPT_BACKOFF);
                }
            }
        }
    })?;
    Ok(())
}

fn attach(s: TcpStream, port: Port, clients: &Shared, tx: &SyncSender<Input>, hold: &Hold) -> Result<(), ()> {
    let _ = s.set_nodelay(true);
    let Ok(w) = Writer::spawn(&s) else { return Ok(()) };
    *clients.lock().unwrap().slot(port) = Some(w);
    tx.send(Input::Connected).map_err(drop)?;
    match port {
        Port::Cmd => read_commands(s, tx),
        Port::Data => read_data(s, tx, hold),
    }
    let mut c = clients.lock().unwrap();
    if c.slot(port).is_some() {
        for w in [c.cmd.take(), c.data.take()].into_iter().flatten() {
            w.close();
        }
        drop(c);
        tx.send(Input::Closed).map_err(drop)?;
    }
    Ok(())
}

fn read_commands(mut s: TcpStream, tx: &SyncSender<Input>) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let mut long = false;
    while let Ok(n @ 1..) = s.read(&mut buf) {
        for &b in &buf[..n] {
            match b {
                b'\r' => {
                    let l = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    if std::mem::take(&mut long) {
                        continue;
                    }
                    let msg = parse(&l).map_or(Input::Wrong(l), Input::Command);
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                b'\n' => {}
                _ if line.len() >= MAX_LINE => {
                    line.clear();
                    long = true;
                }
                _ if long => {}
                _ => line.push(b),
            }
        }
    }
}

fn read_data(mut s: TcpStream, tx: &SyncSender<Input>, hold: &Hold) {
    let mut buf = vec![0; DATA_CHUNK];
    loop {
        hold.wait();
        let Ok(n @ 1..) = s.read(&mut buf) else { return };
        if tx.send(Input::Data(buf[..n].to_vec())).is_err() {
            return;
        }
    }
}
