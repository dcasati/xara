#![allow(dead_code)]

pub mod air;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use xara::monitor::record::{Body, Record};
use xara::station::hf::Station;
use xara::station::{Modem, Options};
use xara::tnc::Tnc;
use xara_arq::hf::session::Config;
use xara_host::server::Host;
use xara_phy::hf::ctrl::Role;
use xara_phy::hf::level::Bandwidth;
use xara_testkit::json::Value;

pub fn vector(name: &str) -> Value {
    let p = format!("{}/../../vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    xara_testkit::json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"))).unwrap()
}

pub fn captures_dir() -> std::path::PathBuf {
    std::env::var_os("XARA_CAPTURES").expect("set XARA_CAPTURES to the VARA lab captures directory").into()
}

pub fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

pub type Lines = Arc<Mutex<Vec<String>>>;

const MARK: &str = "\u{1}sync";
const SYNC_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct Probe {
    lines: Lines,
    crs: Arc<AtomicUsize>,
    marks: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

impl Probe {
    fn open(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }

    fn unanswered(&self) -> bool {
        let replies = self.lines.lock().unwrap().iter().filter(|l| is_reply(l)).count();
        self.open() && replies < self.crs.load(Ordering::SeqCst)
    }
}

pub struct Client {
    pub cmd: TcpStream,
    pub data: TcpStream,
    pub lines: Lines,
    pub bytes: Arc<Mutex<Vec<u8>>>,
    probe: Probe,
}

impl Client {
    fn attach(h: &Host) -> Self {
        let cmd = TcpStream::connect(h.cmd_addr).unwrap();
        let data = TcpStream::connect(h.data_addr).unwrap();
        let probe = Probe::default();
        let bytes: Arc<Mutex<Vec<u8>>> = Default::default();
        let (p, mut r) = (probe.clone(), cmd.try_clone().unwrap());
        thread::spawn(move || {
            let (mut buf, mut cur) = ([0u8; 4096], Vec::new());
            while let Ok(n @ 1..) = r.read(&mut buf) {
                for &b in &buf[..n] {
                    if b != b'\r' {
                        cur.push(b);
                        continue;
                    }
                    let l = String::from_utf8_lossy(&cur).into_owned();
                    cur.clear();
                    if l == MARK {
                        p.marks.fetch_add(1, Ordering::SeqCst);
                    } else {
                        p.lines.lock().unwrap().push(l);
                    }
                }
            }
            p.closed.store(true, Ordering::SeqCst);
        });
        let (b, mut d) = (bytes.clone(), data.try_clone().unwrap());
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = d.read(&mut buf) {
                b.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self { cmd, data, lines: probe.lines.clone(), bytes, probe }
    }

    pub fn send(&mut self, s: &str) {
        self.raw(format!("{s}\r").as_bytes());
    }

    pub fn raw(&mut self, b: &[u8]) {
        self.probe.crs.fetch_add(b.iter().filter(|&&c| c == b'\r').count(), Ordering::SeqCst);
        self.cmd.write_all(b).unwrap();
    }

    pub fn write(&mut self, b: &[u8]) {
        self.data.write_all(b).unwrap();
    }

    pub fn close(&self) {
        let _ = self.cmd.shutdown(Shutdown::Both);
        let _ = self.data.shutdown(Shutdown::Both);
    }

    pub fn saw(&self, s: &str) -> bool {
        self.lines.lock().unwrap().iter().any(|l| l == s)
    }

    pub fn count(&self, s: &str) -> usize {
        self.lines.lock().unwrap().iter().filter(|l| *l == s).count()
    }

    pub fn len(&self) -> usize {
        self.lines.lock().unwrap().len()
    }

    pub fn since(&self, i: usize) -> Vec<String> {
        self.lines.lock().unwrap()[i..].to_vec()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn is_reply(l: &str) -> bool {
    l == "OK" || l == "WRONG" || ["VERSION ", "TUNE ", "CLEANTXBUFFER "].iter().any(|p| l.starts_with(p))
}

pub fn options() -> Options {
    Options { drive_db: -5.0, ..Options::default() }
}

pub fn tnc(bw: Bandwidth, call: &str) -> Tnc<Station> {
    Tnc::new(Station::new(Config::new(bw, call), options()), Host::bind("127.0.0.1", 0).unwrap())
}

pub struct Lab<M: Modem = Station> {
    pub tnc: Vec<Tnc<M>>,
    wire: Vec<Vec<f64>>,
    pub deaf: Vec<bool>,
    probes: Vec<(usize, Probe, usize)>,
}

impl Lab {
    pub fn new(n: usize) -> Self {
        let calls = ["XX0A", "XX0B"];
        Lab::with((0..n).map(|i| tnc(Bandwidth::Hz500, calls[i])).collect())
    }
}

impl<M: Modem> Lab<M> {
    pub fn with(tnc: Vec<Tnc<M>>) -> Self {
        let n = tnc.len();
        Self { tnc, wire: vec![vec![0.0; 960]; n], deaf: vec![false; n], probes: Vec::new() }
    }

    pub fn attach(&mut self, i: usize) -> Client {
        let c = Client::attach(self.tnc[i].host());
        let h = self.tnc[i].host();
        assert!(wait(|| h.has_cmd_client() && h.has_data_client()), "client {i} was not accepted");
        let lines = c.lines.clone();
        let announced = move |_: &Lab<M>| lines.lock().unwrap().iter().filter(|l| l.starts_with("BUSY ")).count() >= 2;
        assert!(self.wait_for(announced), "client {i}: connections not announced");
        self.probes.push((i, c.probe.clone(), 0));
        c
    }

    pub fn now(&self) -> f64 {
        self.tnc[0].station().now() as f64 / 48000.0
    }

    pub fn wait_for(&mut self, done: impl Fn(&Lab<M>) -> bool) -> bool {
        let start = Instant::now();
        while !done(self) {
            if start.elapsed() > SYNC_LIMIT {
                return false;
            }
            for t in &mut self.tnc {
                t.service();
            }
            thread::yield_now();
        }
        true
    }

    fn sync(&mut self) {
        let probes: Vec<Probe> = self.probes.iter().map(|p| p.1.clone()).collect();
        assert!(self.wait_for(|_| !probes.iter().any(Probe::unanswered)), "commands left unanswered");
        for (i, p, sent) in &mut self.probes {
            if p.open() {
                *sent += 1;
                self.tnc[*i].host().line(MARK);
            }
        }
        let probes = &self.probes;
        assert!(
            wait(|| probes.iter().all(|(_, p, sent)| !p.open() || p.marks.load(Ordering::SeqCst) >= *sent)),
            "host lines not delivered"
        );
    }

    pub fn step(&mut self) {
        let n = self.tnc.len();
        for i in 0..n {
            let input: Vec<f64> = if n == 2 && !self.deaf[i] {
                self.wire[1 - i].drain(..480).collect()
            } else {
                if n == 2 {
                    self.wire[1 - i].drain(..480);
                }
                vec![0.0; 480]
            };
            let mut out = vec![0.0; 480];
            self.tnc[i].process(&input, &mut out);
            self.wire[i].extend(out);
        }
    }

    pub fn run_until(&mut self, audio_secs: f64, done: impl Fn(&Lab<M>) -> bool) -> bool {
        let end = self.tnc[0].station().now() + (audio_secs * 48000.0) as i64;
        loop {
            self.sync();
            if done(self) {
                return true;
            }
            if self.tnc[0].station().now() >= end {
                return false;
            }
            self.step();
        }
    }

    pub fn settle(&mut self) {
        self.run_until(0.1, |_| false);
    }
}

fn wait(done: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > SYNC_LIMIT {
            return false;
        }
        thread::yield_now();
    }
    true
}

pub fn ask<M: Modem>(lab: &mut Lab<M>, c: &mut Client, raw: &[u8], n: usize) -> Vec<String> {
    let start = c.len();
    c.raw(raw);
    let lines = c.lines.clone();
    let replies = move || lines.lock().unwrap()[start..].iter().filter(|l| is_reply(l)).cloned().collect::<Vec<_>>();
    let r = replies.clone();
    lab.run_until(2.0, move |_| r().len() >= n);
    lab.settle();
    replies()
}

fn token_matches(want: &str, got: &str) -> bool {
    want == got
        || (want.starts_with('{') && want.ends_with('}') && !got.is_empty())
        || (want == "(l)" && got.starts_with('(') && got.ends_with(')'))
        || (want == "b" && !got.is_empty() && got.bytes().all(|b| b.is_ascii_digit()))
}

pub fn line_matches(want: &str, got: &str) -> bool {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.split(' ').collect(), got.split(' ').collect());
    w.len() == g.len() && w.iter().zip(&g).all(|(a, b)| token_matches(a, b))
}

pub fn quiet(l: &str) -> bool {
    l.starts_with("BUSY ") || ["IAMALIVE", "REGISTERED", "ENCRYPTION DISABLED"].contains(&l)
}

pub fn check_sequence(name: &str, events: &[(String, Vec<String>)], lines: &[String], optional: &[&str]) {
    let got: Vec<&String> = lines.iter().filter(|l| !quiet(l)).collect();
    let mut pos = 0;
    for (ev, want) in events {
        if want.is_empty() || want.iter().any(|w| quiet(w)) {
            continue;
        }
        let at = (pos..=got.len().saturating_sub(want.len()))
            .find(|&i| want.iter().zip(&got[i..]).all(|(w, g)| line_matches(w, g)) && got.len() >= i + want.len());
        match at {
            Some(i) => pos = i + want.len(),
            None if optional.iter().any(|o| ev.contains(o)) => {}
            None => panic!("{name}: event {ev:?} {want:?} not found after line {pos}\n{got:#?}"),
        }
    }
}

pub fn events(v: &Value) -> Vec<(String, Vec<String>)> {
    v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let lines = e["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect();
            (e["event"].as_str().unwrap().to_string(), lines)
        })
        .collect()
}

#[derive(Default)]
pub struct Seen {
    pub delivered: [Vec<u8>; 3],
    pub ctrl: Vec<(Role, u32)>,
    pub callers: Vec<String>,
}

impl Seen {
    pub fn add(&mut self, rec: &Record) {
        match rec {
            Record::Ctrl { sender, code, .. } => self.ctrl.push((*sender, *code)),
            Record::Header { call, .. } => self.callers.push(call.clone()),
            Record::Payload { sender, body: Body::Data { data, .. }, .. } => {
                let i = match sender {
                    Some(Role::Caller) => 0,
                    Some(Role::Answerer) => 1,
                    None => 2,
                };
                self.delivered[i].extend_from_slice(data);
            }
            _ => {}
        }
    }

    pub fn has(&self, sender: Option<Role>, codes: &[u32]) -> bool {
        self.ctrl.iter().any(|&(s, c)| sender.is_none_or(|r| r == s) && codes.contains(&c))
    }
}
