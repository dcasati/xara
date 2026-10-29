use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use xara::audio::TxAudio;
use xara::ptt::{Cm108, Event, Ptt, PttCtl, PttSpec, PttTiming, Rigctld, Serial, cm108_report};

#[test]
fn ptt_spec_parse() {
    assert_eq!(PttSpec::parse("vox"), Ok(PttSpec::Vox));
    assert_eq!(PttSpec::parse("rts:/dev/ttyACM0"), Ok(PttSpec::Rts("/dev/ttyACM0".into())));
    assert_eq!(PttSpec::parse("dtr:/dev/ttyUSB1"), Ok(PttSpec::Dtr("/dev/ttyUSB1".into())));
    assert_eq!(PttSpec::parse("cm108:/dev/hidraw2"), Ok(PttSpec::Cm108 { path: "/dev/hidraw2".into(), gpio: 3 }));
    assert_eq!(PttSpec::parse("cm108:/dev/hidraw2:4"), Ok(PttSpec::Cm108 { path: "/dev/hidraw2".into(), gpio: 4 }));
    assert_eq!(PttSpec::parse("rigctld"), Ok(PttSpec::Rigctld("127.0.0.1:4532".into())));
    assert_eq!(PttSpec::parse("rigctld:radio.lan:4533"), Ok(PttSpec::Rigctld("radio.lan:4533".into())));
    for bad in ["", "rts", "rts:ttyACM0", "cm108:/dev/hidraw0:9", "cm108:/dev/hidraw0:0", "vox:x", "gpio:4"] {
        assert!(PttSpec::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn cm108_reports() {
    assert_eq!(cm108_report(3, true), [0, 0, 4, 4, 0]);
    assert_eq!(cm108_report(3, false), [0, 0, 0, 4, 0]);
    assert_eq!(cm108_report(1, true), [0, 0, 1, 1, 0]);
    let f = Scratch::new("cm108");
    let mut c = Cm108::open(f.path(), 3).unwrap();
    c.set(true).unwrap();
    c.set(false).unwrap();
    assert_eq!(std::fs::read(&f.0).unwrap(), [[0, 0, 0, 4, 0], [0, 0, 4, 4, 0], [0, 0, 0, 4, 0]].concat());
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let p = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ptt-{name}-{}", std::process::id()));
        std::fs::write(&p, b"").unwrap();
        Self(p)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn serial_refuses_non_serial_devices() {
    assert!(Serial::open("/dev/null", libc::TIOCM_RTS).is_err());
}

// FreeBSD ptys accept the modem-line ioctls, so only Linux rejects them.
#[cfg(target_os = "linux")]
#[test]
fn serial_refuses_ptys() {
    let (mut m, mut s) = (0, 0);
    let mut name = [0 as libc::c_char; 64];
    assert_eq!(
        unsafe { libc::openpty(&mut m, &mut s, name.as_mut_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) },
        0
    );
    let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_str().unwrap().to_string();
    let e = Serial::open(&path, libc::TIOCM_RTS).err().expect("a pty has no modem lines");
    assert_eq!(e.raw_os_error(), Some(libc::ENOTTY));
    unsafe {
        libc::close(m);
        libc::close(s);
    }
}

fn fake_rigctld(reply: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    thread::spawn(move || {
        let (s, _) = l.accept().unwrap();
        let mut w = s.try_clone().unwrap();
        for line in BufReader::new(s).lines() {
            let line = line.unwrap();
            log2.lock().unwrap().push(line.clone());
            let r = if line == "T 1" { reply } else { "RPRT 0" };
            if w.write_all(format!("{r}\n").as_bytes()).is_err() {
                break;
            }
        }
    });
    (addr, log)
}

#[test]
fn rigctld_protocol() {
    let (addr, log) = fake_rigctld("RPRT 0");
    let mut r = Rigctld::open(&addr).unwrap();
    r.set(true).unwrap();
    r.set(false).unwrap();
    assert_eq!(*log.lock().unwrap(), ["T 0", "T 1", "T 0"]);

    let (addr, _) = fake_rigctld("RPRT -11");
    let mut r = Rigctld::open(&addr).unwrap();
    assert!(r.set(true).is_err());

    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    assert!(Rigctld::open(&addr).is_err());
}

#[derive(Clone, Default)]
struct Rec(Arc<Mutex<Vec<(Instant, bool)>>>);

impl Ptt for Rec {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.0.lock().unwrap().push((Instant::now(), on));
        Ok(())
    }

    fn describe(&self) -> String {
        "test".into()
    }
}

fn ctl(t: PttTiming) -> (PttCtl, Rec, Arc<Mutex<Vec<&'static str>>>) {
    let rec = Rec::default();
    let ev = Arc::new(Mutex::new(Vec::new()));
    let ev2 = ev.clone();
    let c = PttCtl::spawn_with(Box::new(rec.clone()), t, Arc::default(), move |e| {
        ev2.lock().unwrap().push(match e {
            Event::On => "on",
            Event::Off => "off",
            Event::Watchdog => "watchdog",
            Event::Stalled => "stalled",
        })
    });
    (c, rec, ev)
}

fn feed(c: &mut PttCtl, on: bool, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        c.set(on).unwrap();
        thread::sleep(Duration::from_millis(10));
    }
}

fn states(r: &Rec) -> Vec<bool> {
    r.0.lock().unwrap().iter().map(|e| e.1).collect()
}

fn feed_until(c: &mut PttCtl, on: bool, done: impl Fn(&PttCtl) -> bool) {
    let end = Instant::now() + Duration::from_secs(3);
    while !done(c) {
        assert!(Instant::now() < end, "condition not reached within 3 s");
        c.set(on).unwrap();
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_until(done: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(3);
    while !done() {
        assert!(Instant::now() < end, "condition not reached within 3 s");
        thread::sleep(Duration::from_millis(5));
    }
}

const T: PttTiming = PttTiming {
    lead: Duration::ZERO,
    tail: Duration::from_millis(80),
    max_on: Duration::from_secs(5),
    stall: Duration::from_millis(300),
};

#[test]
fn ptt_keys_and_holds_the_tail() {
    let (mut c, rec, _) = ctl(T);
    feed(&mut c, false, 50);
    assert!(states(&rec).is_empty());
    let t0 = Instant::now();
    feed(&mut c, true, 200);
    let t1 = Instant::now();
    feed_until(&mut c, false, |_| states(&rec).len() >= 2);
    feed(&mut c, false, 100);
    let v = rec.0.lock().unwrap().clone();
    assert_eq!(v.iter().map(|e| e.1).collect::<Vec<_>>(), [true, false]);
    assert!(v[0].0 - t0 < Duration::from_millis(200));
    let tail = v[1].0 - t1;
    assert!(tail >= Duration::from_millis(70) && tail < Duration::from_millis(500), "{tail:?}");
}

#[test]
fn ptt_gap_shorter_than_tail_stays_keyed() {
    let (mut c, rec, _) = ctl(PttTiming { tail: Duration::from_millis(400), ..T });
    feed(&mut c, true, 100);
    feed(&mut c, false, 40);
    feed(&mut c, true, 100);
    feed_until(&mut c, false, |_| states(&rec).len() >= 2);
    feed(&mut c, false, 100);
    assert_eq!(states(&rec), [true, false]);
}

#[test]
fn ptt_watchdog_releases_and_stays_released() {
    let (mut c, rec, ev) = ctl(PttTiming { max_on: Duration::from_millis(200), ..T });
    let t0 = Instant::now();
    feed_until(&mut c, true, |_| states(&rec).len() >= 2);
    feed(&mut c, true, 200);
    let v = rec.0.lock().unwrap().clone();
    assert_eq!(v.iter().map(|e| e.1).collect::<Vec<_>>(), [true, false]);
    let held = v[1].0 - t0;
    assert!(held >= Duration::from_millis(200) && held < Duration::from_millis(800), "{held:?}");
    feed(&mut c, false, 150);
    feed_until(&mut c, true, |_| states(&rec).len() >= 3);
    feed_until(&mut c, false, |_| states(&rec).len() >= 4);
    assert_eq!(states(&rec), [true, false, true, false]);
    assert_eq!(ev.lock().unwrap()[..3], ["on", "watchdog", "off"]);
}

#[test]
fn ptt_released_when_the_audio_loop_stalls() {
    let (mut c, rec, ev) = ctl(T);
    feed(&mut c, true, 100);
    let t0 = Instant::now();
    wait_until(|| states(&rec).len() >= 2);
    thread::sleep(Duration::from_millis(100));
    let v = rec.0.lock().unwrap().clone();
    assert_eq!(v.iter().map(|e| e.1).collect::<Vec<_>>(), [true, false]);
    assert!(v[1].0 - t0 < Duration::from_millis(1000));
    assert!(ev.lock().unwrap().contains(&"stalled"));
    feed_until(&mut c, true, |_| states(&rec).len() >= 3);
    assert_eq!(states(&rec), [true, false, true]);
    wait_until(|| states(&rec).len() >= 4);
    feed(&mut c, false, 150);
    assert_eq!(states(&rec), [true, false, true, false]);
}

#[test]
fn ptt_released_on_drop() {
    let (mut c, rec, _) = ctl(T);
    feed(&mut c, true, 100);
    drop(c);
    assert_eq!(states(&rec), [true, false]);
}

fn run(bin: &str, args: &[&str]) -> (i32, String, String) {
    let mut c = Command::new(bin)
        .args(args)
        .env("PULSE_SERVER", "unix:/nonexistent/xara-test")
        .env_remove("STATE_DIRECTORY")
        .env("XDG_CONFIG_HOME", "/nonexistent/xara-test")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let drain = |mut r: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            String::from_utf8_lossy(&b).into_owned()
        })
    };
    let (out, err) = (drain(Box::new(c.stdout.take().unwrap())), drain(Box::new(c.stderr.take().unwrap())));
    let end = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = c.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > end {
            let _ = c.kill();
            let _ = c.wait();
            panic!("{bin} {args:?}: still running after 20 s");
        }
        thread::sleep(Duration::from_millis(10));
    };
    (status.code().unwrap_or(-1), out.join().unwrap(), err.join().unwrap())
}

fn xara(args: &[&str]) -> (i32, String) {
    let (c, _, e) = run(env!("CARGO_BIN_EXE_xara"), args);
    (c, e)
}

const OPT_IN_BASE: [&str; 8] =
    ["--call", "N2CALL", "--capture", "nothing.monitor", "--port", "0", "--kiss-port", "off"];

fn opt_in(extra: &[&str]) -> (i32, String) {
    xara(&[&OPT_IN_BASE[..], extra].concat())
}

#[test]
fn radio_mode_needs_explicit_opt_in() {
    let (c, e) = opt_in(&["--playback", "nothing", "--ptt", "vox"]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("--ptt needs --radio"));
    let (c, e) = opt_in(&["--playback", "nothing", "--radio"]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("--radio needs --ptt"));
    let (c, _) = opt_in(&["--playback", "nothing", "--radio", "--ptt", "vox", "--ptt-max-s", "0"]);
    assert_eq!(c, 2);
    let (c, e) = xara(&["--call", "N2CALL", "--capture", "x", "--playback", "y", "--port", "65535"]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("--port must be below 65535"), "{e}");
}

#[cfg(feature = "pulse")]
#[test]
fn radio_mode_refuses_a_default_sink_before_contacting_the_sound_server() {
    for sink in ["@DEFAULT_SINK@", "default"] {
        let (c, e) = opt_in(&["--playback", sink, "--radio", "--ptt", "vox"]);
        assert_eq!(c, 1, "{e}");
        assert!(e.contains("named explicitly") && !e.contains("sound server"), "{e}");
    }
    for extra in [&["--radio", "--ptt", "vox"][..], &[]] {
        let (c, e) = opt_in(&[&["--playback", "xara-no-such-sink"][..], extra].concat());
        assert_eq!(c, 1, "{e}");
        assert!(e.contains("sound server"), "{e}");
    }
}

struct Flaky(Rec);

impl Ptt for Flaky {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.0.set(on)?;
        if on { Err(io::Error::other("line stuck")) } else { Ok(()) }
    }

    fn describe(&self) -> String {
        "flaky".into()
    }
}

#[test]
fn ptt_fault_is_returned_after_release() {
    let rec = Rec::default();
    let mut c = PttCtl::spawn_with(Box::new(Flaky(rec.clone())), T, Arc::default(), |_| {});
    let t0 = Instant::now();
    let e = loop {
        if let Err(e) = c.set(true) {
            break e;
        }
        assert!(t0.elapsed() < Duration::from_secs(1));
        thread::sleep(Duration::from_millis(5));
    };
    assert!(e.to_string().contains("PTT flaky failed: line stuck; released"), "{e}");
    assert_eq!(states(&rec), [true, false]);
}

#[test]
fn ptt_released_on_shutdown_flag() {
    let rec = Rec::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut c = PttCtl::spawn_with(Box::new(rec.clone()), T, stop.clone(), |_| {});
    feed_until(&mut c, true, |_| !states(&rec).is_empty());
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_until(|| states(&rec).len() >= 2);
    assert_eq!(states(&rec), [true, false]);
    assert!(c.set(true).is_ok());
    assert!(c.finish().is_ok());
    assert_eq!(states(&rec), [true, false]);
}

struct Stuck(Rec);

impl Ptt for Stuck {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.0.set(on)?;
        if on { Ok(()) } else { Err(io::Error::other("line stuck")) }
    }

    fn describe(&self) -> String {
        "stuck".into()
    }
}

#[test]
fn a_failed_release_is_reported_after_shutdown() {
    let rec = Rec::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut c = PttCtl::spawn_with(Box::new(Stuck(rec.clone())), T, stop.clone(), |_| {});
    feed_until(&mut c, true, |_| !states(&rec).is_empty());
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_until(|| states(&rec).len() >= 4);
    let e = c.set(true).expect_err("a release failure must not be swallowed");
    assert!(e.to_string().contains("COULD NOT RELEASE PTT on stuck"), "{e}");
    assert_eq!(states(&rec), [true, false, false, false]);
}

struct Recovers(Rec, usize);

impl Ptt for Recovers {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.0.set(on)?;
        match on {
            true => Err(io::Error::other("link dropped")),
            false if self.1 > 0 => {
                self.1 -= 1;
                Err(io::Error::other("link dropped"))
            }
            false => Ok(()),
        }
    }

    fn describe(&self) -> String {
        "recovers".into()
    }

    fn release_window(&self) -> Duration {
        Duration::from_secs(10)
    }
}

#[test]
fn release_keeps_retrying_through_a_link_dropout() {
    let rec = Rec::default();
    let mut c = PttCtl::spawn_with(Box::new(Recovers(rec.clone(), 5)), T, Arc::default(), |_| {});
    let t0 = Instant::now();
    let e = loop {
        if let Err(e) = c.set(true) {
            break e;
        }
        assert!(t0.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(5));
    };
    assert!(e.to_string().contains("PTT recovers failed: link dropped; released"), "{e}");
    assert_eq!(states(&rec), [true, false, false, false, false, false, false]);
}

fn blocked_log(t: PttTiming, on: &'static str) -> (PttCtl, Rec, std::sync::mpsc::Sender<()>) {
    let rec = Rec::default();
    let (unblock, wait) = std::sync::mpsc::channel::<()>();
    let c = PttCtl::spawn_with(Box::new(rec.clone()), t, Arc::default(), move |e| {
        let name = match e {
            Event::Watchdog => "watchdog",
            Event::Stalled => "stalled",
            _ => "",
        };
        if name == on {
            let _ = wait.recv();
        }
    });
    (c, rec, unblock)
}

#[test]
fn ptt_released_before_a_blocked_watchdog_log() {
    let (mut c, rec, unblock) = blocked_log(PttTiming { max_on: Duration::from_millis(100), ..T }, "watchdog");
    let t0 = Instant::now();
    while states(&rec) != [true, false] {
        let _ = c.set(true);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", states(&rec));
        thread::sleep(Duration::from_millis(10));
    }
    drop(unblock);
}

#[test]
fn ptt_released_before_a_blocked_stall_log() {
    let (mut c, rec, unblock) = blocked_log(T, "stalled");
    feed(&mut c, true, 50);
    let t0 = Instant::now();
    while states(&rec) != [true, false] {
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", states(&rec));
        thread::sleep(Duration::from_millis(10));
    }
    drop(unblock);
}

#[test]
fn ptt_released_when_the_ptt_thread_panics() {
    let rec = Rec::default();
    let mut c = PttCtl::spawn_with(Box::new(rec.clone()), T, Arc::default(), |e| {
        if matches!(e, Event::On) {
            panic!("log failed");
        }
    });
    let t0 = Instant::now();
    let e = loop {
        if let Err(e) = c.set(true) {
            break e;
        }
        assert!(t0.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(5));
    };
    assert!(e.to_string().contains("panicked"), "{e}");
    assert_eq!(states(&rec), [true, false]);
}

#[test]
fn a_watchdog_trip_reports_forced_off_until_the_modem_lets_go() {
    let (mut c, rec, _) = ctl(PttTiming { max_on: Duration::from_millis(300), ..T });
    feed_until(&mut c, true, |_| !states(&rec).is_empty());
    assert!(!c.forced_off());
    feed_until(&mut c, true, PttCtl::forced_off);
    feed_until(&mut c, false, |c| !c.forced_off());
}

#[test]
fn the_vox_watchdog_mutes_tx_audio() {
    let mut c = PttCtl::spawn_with(
        Box::new(xara::ptt::Vox),
        PttTiming { max_on: Duration::from_millis(100), ..T },
        Arc::default(),
        |_| {},
    );
    let mut tx = TxAudio::new(0);
    let t0 = Instant::now();
    let (mut loud, mut muted) = (Vec::new(), None);
    while t0.elapsed() < Duration::from_millis(600) {
        c.set(true).unwrap();
        let mut out = vec![0.5; 480];
        tx.process(&mut out, c.forced_off());
        match out.iter().any(|&v| v != 0.0) {
            true => loud.push(t0.elapsed()),
            false => muted = muted.or(Some(t0.elapsed())),
        }
        thread::sleep(Duration::from_millis(10));
    }
    let muted = muted.expect("the watchdog muted the audio");
    assert!(!loud.is_empty() && loud.iter().all(|&t| t < muted), "{loud:?} {muted:?}");
    assert!(muted >= Duration::from_millis(100) && muted < Duration::from_millis(400), "{muted:?}");
    assert!(c.forced_off());
}

#[test]
fn muting_also_clears_the_lead_delay() {
    let mut tx = TxAudio::new(4);
    let mut out = [1.0, 2.0, 3.0];
    tx.process(&mut out, false);
    assert_eq!(out, [0.0, 0.0, 0.0]);
    let mut out = [4.0, 5.0, 6.0];
    tx.process(&mut out, false);
    assert_eq!(out, [0.0, 1.0, 2.0]);
    let mut out = [7.0, 8.0, 9.0];
    tx.process(&mut out, true);
    assert_eq!(out, [0.0, 0.0, 0.0]);
    let mut out = [10.0, 11.0, 12.0];
    tx.process(&mut out, false);
    assert_eq!(out, [0.0, 0.0, 0.0]);
    let mut out = [13.0, 14.0, 15.0];
    tx.process(&mut out, false);
    assert_eq!(out, [0.0, 10.0, 11.0]);
}

fn monitor(args: &[&str]) -> (i32, String, String) {
    run(env!("CARGO_BIN_EXE_xara-monitor"), args)
}

#[test]
fn help_and_version_exit_cleanly() {
    for (bin, name) in [(env!("CARGO_BIN_EXE_xara"), "xara"), (env!("CARGO_BIN_EXE_xara-monitor"), "xara-monitor")] {
        let (c, out, _) = run(bin, &["--help"]);
        assert_eq!(c, 0);
        assert!(out.starts_with(&format!("usage: {name} ")));
        let (c, out, _) = run(bin, &["--version"]);
        assert_eq!(c, 0);
        assert_eq!(out.trim(), format!("{name} {}", env!("CARGO_PKG_VERSION")));
    }
}

fn listed(help: &str) -> Vec<String> {
    let mut v: Vec<String> = help
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| w.len() > 2 && w.starts_with("--"))
        .map(String::from)
        .collect();
    v.sort();
    v.dedup();
    v
}

fn parsed(src: &str) -> Vec<String> {
    let (mut v, mut gated) = (Vec::new(), false);
    for l in src.lines().map(str::trim) {
        if !gated && l.starts_with("\"--") && l.contains(" => ") {
            let arm = l.split(" => ").next().unwrap();
            v.extend(arm.split(" | ").map(|f| f.trim_matches('"')).filter(|f| f.starts_with("--")).map(String::from));
        }
        gated = l.starts_with("#[cfg(feature");
    }
    v
}

#[test]
fn help_lists_exactly_the_parsed_options() {
    let ui = parsed(include_str!("../src/cli.rs"));
    for (bin, src) in [
        (env!("CARGO_BIN_EXE_xara"), include_str!("../src/main.rs")),
        (env!("CARGO_BIN_EXE_xara-monitor"), include_str!("../src/bin/xara-monitor.rs")),
    ] {
        let (_, help, _) = run(bin, &["--help"]);
        let shown = listed(&help);
        for f in parsed(src).iter().chain(&ui) {
            assert!(shown.contains(f), "{bin}: {f} is missing from --help");
        }
        for f in &shown {
            let (_, _, e) = run(bin, &[f]);
            assert!(!e.contains("unknown option"), "{bin}: --help shows {f}: {e}");
        }
    }
}

#[test]
fn bad_option_values_are_refused() {
    let base = ["--call", "N2CALL", "--capture", "x", "--playback", "y"];
    let run = |extra: &[&str]| xara(&[&base[..], extra].concat());
    for (extra, want) in [
        (&["--drive-db", "NaN"][..], "--drive-db: NaN is not a finite number"),
        (&["--drive-db", "-inf"], "--drive-db: -inf is not a finite number"),
        (&["--persistence", "nan"], "--persistence: nan is not a finite number"),
        (&["--persistence", "inf"], "--persistence: inf is not a finite number"),
        (&["--call", "N0/CALL"], "bad call N0/CALL"),
        (&["--call", "TOOLONGCALL"], "bad call TOOLONGCALL"),
        (&["--call", "N0CALL"], "--call: N0CALL is not a callsign"),
        (&["--hf", "1000"], "bad bandwidth 1000"),
        (&["--hf", "500", "--fm", "narrow"], "--hf and --fm cannot both be given"),
        (&["--fm", "wide", "--hf", "2300"], "--hf and --fm cannot both be given"),
        (&["--tx-buffer-ms", "29"], "--tx-buffer-ms must be 30..500"),
        (&["--tx-buffer-ms", "501"], "--tx-buffer-ms must be 30..500"),
        (&["--tx-buffer-ms", "-5"], "--tx-buffer-ms: invalid digit"),
    ] {
        let (c, e) = run(extra);
        assert_eq!(c, 2, "{extra:?}: {e}");
        assert!(e.contains(want), "{extra:?}: {e}");
    }
    let (c, _, e) = monitor(&["--stdin", "12000", "--hf", "1000"]);
    assert_eq!(c, 2);
    assert!(e.contains("bad bandwidth 1000"), "{e}");
    let (c, _, e) = monitor(&["--stdin", "12000", "--call", "N0CALL\r"]);
    assert_eq!(c, 2);
    assert!(e.contains("bad call"), "{e}");
    for rate in ["0", "-1", "NaN", "inf", "100", "2e6"] {
        let (c, _, e) = monitor(&["--stdin", rate]);
        assert_eq!(c, 2, "{rate}: {e}");
        assert!(e.contains("--stdin: sample rate"), "{rate}: {e}");
    }
}

fn slow_rigctld(first_reply: &'static [&'static [u8]]) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    thread::spawn(move || {
        for (n, s) in l.incoming().enumerate() {
            let s = s.unwrap();
            let log = log2.clone();
            thread::spawn(move || {
                let mut w = s.try_clone().unwrap();
                for line in BufReader::new(s).lines() {
                    let Ok(line) = line else { break };
                    log.lock().unwrap().push(format!("{n}:{line}"));
                    if n == 0 && line == "T 1" {
                        for part in first_reply {
                            if w.write_all(part).is_err() {
                                return;
                            }
                            thread::sleep(Duration::from_millis(700));
                        }
                    } else if w.write_all(b"RPRT 0\n").is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, log)
}

#[test]
fn rigctld_reconnects_after_a_late_reply() {
    let (addr, log) = slow_rigctld(&[b"", b"", b"RPRT -1\n"]);
    let mut r = Rigctld::open(&addr).unwrap();
    assert!(r.set(true).is_err());
    r.set(false).unwrap();
    r.set(true).unwrap();
    r.set(false).unwrap();
    assert_eq!(*log.lock().unwrap(), ["0:T 0", "0:T 1", "1:T 0", "1:T 1", "1:T 0"]);
}

#[test]
fn rigctld_reconnects_after_a_partial_reply() {
    let (addr, log) = slow_rigctld(&[b"RPR", b"", b"T -1\n"]);
    let mut r = Rigctld::open(&addr).unwrap();
    assert!(r.set(true).is_err());
    r.set(false).unwrap();
    assert_eq!(*log.lock().unwrap(), ["0:T 0", "0:T 1", "1:T 0"]);
}

#[test]
fn is_hidraw_accepts_only_hidraw_char_devices() {
    assert!(!xara::ptt::is_hidraw("/dev/null"));
    assert!(!xara::ptt::is_hidraw("/nonexistent"));
    assert!(!xara::ptt::is_hidraw(Scratch::new("plain").path()));
    if let Some(h) =
        std::fs::read_dir("/dev").unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("hidraw"))
    {
        assert!(xara::ptt::is_hidraw(h.path().to_str().unwrap()));
    }
}
