mod common;

use common::air::{self, Failure, payload};
use xara::station::hf::Station;
use xara::station::{Modem, Options, Report};
use xara_arq::compression::Compression;
use xara_arq::hf::session::{Config, State};
use xara_arq::host::HostEvent;
use xara_phy::hf::level::Bandwidth;

#[derive(Clone, Copy, Default)]
struct Lead {
    ms: [i64; 2],
    told: bool,
}

type Air = air::Air<Station, 2>;

fn pair(bw: Bandwidth, registered: bool, noise: f64, lead: Lead) -> Air {
    let cfg = |call: &str, seed| {
        let mut c = Config { seed, ..Config::new(bw, call) };
        c.host.registered = registered;
        c
    };
    let opts = |i: usize| Options { tx_latency: if lead.told { lead.ms[i] * 48 } else { 0 }, ..Options::default() };
    Air::pair(
        [Station::new(cfg("N0CALL", 3), opts(0)), Station::new(cfg("N1CALL", 5), opts(1))],
        [0, 1].map(|i| 2400 + lead.ms[i] as usize * 48),
        noise,
        |r| matches!(r, Report::Host(HostEvent::Buffer(_) | HostEvent::Sn(_))),
    )
}

fn session(bw: Bandwidth, registered: bool, noise: f64, a_to_b: usize, b_to_a: usize) -> Air {
    session_with(bw, registered, noise, payload(a_to_b, 77), payload(b_to_a, 78), Compression::Text)
}

fn session_with(bw: Bandwidth, registered: bool, noise: f64, pa: Vec<u8>, pb: Vec<u8>, comp: Compression) -> Air {
    session_led(bw, registered, noise, pa, pb, comp, Lead::default()).unwrap()
}

fn session_led(
    bw: Bandwidth,
    registered: bool,
    noise: f64,
    pa: Vec<u8>,
    pb: Vec<u8>,
    comp: Compression,
    lead: Lead,
) -> Result<Air, Failure> {
    let mut air = pair(bw, registered, noise, lead);
    let mut cfg = air.st[0].config().clone();
    cfg.host.compression = comp;
    air.st[0].reconfigure(cfg);
    air.st[0].connect("N0CALL", "N1CALL", &[]);
    let line = Report::Host(HostEvent::Connected {
        caller: "N0CALL".into(),
        answerer: "N1CALL".into(),
        vias: Vec::new(),
        bw: bw.into(),
    });
    if !air.run_until(40.0, |a| a.has(0, &line) && a.has(1, &line)) {
        return Err(Failure::NoLink(air.trace()));
    }
    air.st[0].send(&pa);
    air.st[1].send(&pb);
    let secs = 30.0 + (pa.len() + pb.len()) as f64 / 8.0;
    if !air.run_until(secs, |a| a.got[1].len() >= pa.len() && a.got[0].len() >= pb.len()) {
        let (a, b) = (air.got[1].len(), air.got[0].len());
        return Err(Failure::Stalled(format!("{a}/{} and {b}/{}:\n{}", pa.len(), pb.len(), air.trace())));
    }
    assert_eq!(air.got[1], pa);
    assert_eq!(air.got[0], pb);
    air.st[0].disconnect();
    if !air.run_until(40.0, |a| a.st.iter().all(Station::idle)) {
        return Err(Failure::NoDisconnect(air.trace()));
    }
    let gone = Report::Host(HostEvent::Disconnected);
    assert!(air.has(0, &gone) && air.has(1, &gone), "{}", air.trace());
    assert_eq!(air.st[1].state(), State::Listen);
    Ok(air)
}

#[test]
fn unregistered_2300_over_audio() {
    let air = session(Bandwidth::Hz2300, false, 0.002, 400, 120);
    assert_eq!(air.top_tx(0), 4, "{}", air.trace());
}

#[test]
fn registered_links_climb_over_audio() {
    for (bw, n, min) in [(Bandwidth::Hz2300, 30000, 12), (Bandwidth::Hz500, 6000, 9), (Bandwidth::Hz2750, 30000, 12)] {
        let air = session(bw, true, 0.002, n, 3000);
        assert!(air.top_tx(0) >= min, "{bw:?}\n{}", air.trace());
    }
}

#[test]
fn noisy_link_over_audio() {
    for noise in [0.03, 0.08] {
        let air = session(Bandwidth::Hz2300, true, noise, 8000, 500);
        assert!(air.top_tx(0) >= 10 && air.secs() < 90.0, "noise {noise}: {:.1} s\n{}", air.secs(), air.trace());
    }
}

fn text(n: usize) -> Vec<u8> {
    let line =
        b"Subject: net report\r\nThe weather station at N0CALL reports clear skies; check-in tonight 73 de N0CALL\r\n";
    line.iter().copied().cycle().take(n).collect()
}

fn zipped(air: &Air, i: usize) -> usize {
    air.events[i]
        .iter()
        .filter(|(_, r)| matches!(r, Report::Trace(t) if t.starts_with("rx L") && t.split(' ').nth(2).is_some_and(|f| f.contains('z'))))
        .count()
}

#[test]
fn compressed_sessions_over_audio() {
    let run = |comp| session_with(Bandwidth::Hz2300, true, 0.002, text(20_000), text(3_000), comp);
    let raw = run(Compression::Off);
    assert_eq!((zipped(&raw, 0), zipped(&raw, 1)), (0, 0));
    let text = run(Compression::Text);
    assert!(zipped(&text, 0) > 0 && zipped(&text, 1) > 0, "{}", text.trace());
    let files = run(Compression::Files);
    let (r, t, f) = (raw.secs(), text.secs(), files.secs());
    assert!(t + 5.0 < r && f + 5.0 < r, "raw {r:.1} s, TEXT {t:.1} s, FILES {f:.1} s");
}

#[test]
fn ptt_lead_is_absorbed() {
    for ms in [[300, 0], [500, 0], [0, 100], [100, 100]] {
        let led = |told| {
            let lead = Lead { ms, told };
            session_led(Bandwidth::Hz2300, true, 0.002, payload(8000, 77), payload(1500, 77), Compression::Text, lead)
        };
        let before = led(false);
        assert!(matches!(before, Err(Failure::NoLink(_))), "lead {ms:?} unaware: {:?}", before.map(|a| a.secs()));
        let after = led(true).unwrap_or_else(|e| panic!("lead {ms:?}: {e:?}"));
        assert!(after.top_tx(0) >= 10 && after.secs() < 100.0, "lead {ms:?}: {:.1} s\n{}", after.secs(), after.trace());
    }
}

#[test]
fn ssid_calls_link_over_audio() {
    let cfg = |call: &str, seed| Config { seed, ..Config::new(Bandwidth::Hz500, call) };
    let mut air = Air::pair(
        [Station::new(cfg("N0CALL-1", 3), Options::default()), Station::new(cfg("N1CALL-T", 5), Options::default())],
        [2400, 2400],
        0.002,
        |r| matches!(r, Report::Host(HostEvent::Buffer(_) | HostEvent::Sn(_))),
    );
    air.st[0].connect("N0CALL-1", "N1CALL-T", &[]);
    let line = Report::Host(HostEvent::Connected {
        caller: "N0CALL-1".into(),
        answerer: "N1CALL-T".into(),
        vias: Vec::new(),
        bw: Bandwidth::Hz500.into(),
    });
    assert!(air.run_until(40.0, |a| a.has(0, &line) && a.has(1, &line)), "{}", air.trace());
    let (pa, pb) = (payload(300, 77), payload(100, 78));
    air.st[0].send(&pa);
    air.st[1].send(&pb);
    assert!(air.run_until(90.0, |a| a.got[1].len() >= pa.len() && a.got[0].len() >= pb.len()), "{}", air.trace());
    assert_eq!((&air.got[1], &air.got[0]), (&pa, &pb));
}
