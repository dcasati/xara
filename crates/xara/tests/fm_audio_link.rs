mod common;

use common::air::{self, Failure, payload};
use xara::station::fm::Station;
use xara::station::{Modem, Options, Report};
use xara_arq::compression::Compression;
use xara_arq::fm;
use xara_arq::host::HostEvent;
use xara_arq::kiss::Command;
use xara_phy::fm::level::FmBandwidth;

#[derive(Clone, Copy, Default)]
struct Lead {
    ms: [i64; 2],
    told: bool,
}

type Air = air::Air<Station, 2>;

fn fm_air(bw: [FmBandwidth; 2], registered: bool, noise: f64, lead: Lead) -> Air {
    let cfg = |bw, call: &str, seed| {
        let mut c = fm::Config { seed, ..fm::Config::new(bw, call) };
        (c.host.registered, c.host.compression) = (registered, Compression::Off);
        c
    };
    let opts = |i: usize| Options { tx_latency: if lead.told { lead.ms[i] * 48 } else { 0 }, ..Options::default() };
    Air::pair(
        [Station::new(cfg(bw[0], "N0CALL", 3), opts(0)), Station::new(cfg(bw[1], "N1CALL", 5), opts(1))],
        [0, 1].map(|i| 2400 + lead.ms[i] as usize * 48),
        noise,
        |r| matches!(r, Report::Host(HostEvent::Buffer(_)) | Report::Busy(_)),
    )
}

fn host_lines(a: &Air, i: usize) -> Vec<String> {
    a.events[i]
        .iter()
        .filter_map(|(_, r)| match r {
            Report::Line(l) => Some(l.clone()),
            Report::Host(HostEvent::Disconnected) => Some("DISCONNECTED".into()),
            _ => None,
        })
        .collect()
}

fn session(bw: [FmBandwidth; 2], registered: bool, noise: f64, pa: Vec<u8>, pb: Vec<u8>) -> Air {
    session_led(bw, registered, noise, pa, pb, Lead::default()).unwrap()
}

fn session_led(
    bw: [FmBandwidth; 2],
    registered: bool,
    noise: f64,
    pa: Vec<u8>,
    pb: Vec<u8>,
    lead: Lead,
) -> Result<Air, Failure> {
    let mut air = fm_air(bw, registered, noise, lead);
    air.st[1].send(&pb);
    assert!(air.st[0].connect("N0CALL", "N1CALL", &[]));
    let link = if bw == [FmBandwidth::Wide; 2] { FmBandwidth::Wide } else { FmBandwidth::Narrow };
    let line = Report::Host(HostEvent::Connected {
        caller: "N0CALL".into(),
        answerer: "N1CALL".into(),
        vias: Vec::new(),
        bw: link.into(),
    });
    if !air.run_until(30.0, |a| a.has(0, &line) && a.has(1, &line)) {
        return Err(Failure::NoLink(air.trace()));
    }
    air.st[0].send(&pa);
    let secs = 60.0 + (pa.len() + pb.len()) as f64 / 60.0;
    if !air.run_until(secs, |a| a.got[1].len() >= pa.len() && a.got[0].len() >= pb.len()) {
        let (a, b) = (air.got[1].len(), air.got[0].len());
        return Err(Failure::Stalled(format!("{a}/{} and {b}/{}:\n{}", pa.len(), pb.len(), air.trace())));
    }
    assert_eq!(air.got[1], pa);
    assert_eq!(air.got[0], pb);
    air.st[0].disconnect();
    if !air.run_until(30.0, |a| a.st.iter().all(Station::idle)) {
        return Err(Failure::NoDisconnect(air.trace()));
    }
    assert_eq!(host_lines(&air, 0).last().map(String::as_str), Some("DISCONNECTED"), "{}", air.trace());
    assert_eq!(host_lines(&air, 1).last().map(String::as_str), Some("DISCONNECTED"), "{}", air.trace());
    assert_eq!(air.st[1].state(), fm::State::Listen);
    Ok(air)
}

#[test]
fn narrow_unregistered_over_audio() {
    let air = session([FmBandwidth::Narrow; 2], false, 0.002, payload(2500, 1), payload(900, 2));
    assert_eq!(air.top_tx(0), 2, "{}", air.trace());
    assert!(air.has(1, &Report::Host(HostEvent::Registered(false))));
}

#[test]
fn wide_unregistered_over_audio() {
    let air = session([FmBandwidth::Wide; 2], false, 0.002, payload(2500, 3), payload(600, 4));
    assert_eq!(air.top_tx(0), 2, "{}", air.trace());
}

#[test]
fn wide_caller_narrow_target_links_narrow() {
    session([FmBandwidth::Wide, FmBandwidth::Narrow], false, 0.002, payload(400, 5), Vec::new());
}

#[test]
fn registered_links_climb_over_audio() {
    for (bw, min) in [(FmBandwidth::Narrow, 6), (FmBandwidth::Wide, 7)] {
        let air = session([bw; 2], true, 0.001, payload(40_000, 6), payload(1500, 7));
        assert!(air.top_tx(0) >= min, "{bw:?}\n{}", air.trace());
    }
}

#[test]
fn noisy_links_over_audio() {
    for (bw, noise, min, max) in [
        (FmBandwidth::Narrow, 0.02, 6, 40.0),
        (FmBandwidth::Wide, 0.02, 6, 40.0),
        (FmBandwidth::Narrow, 0.05, 6, 45.0),
        (FmBandwidth::Narrow, 0.3, 2, 120.0),
    ] {
        let air = session([bw; 2], true, noise, payload(8000, 8), payload(500, 9));
        assert!(air.top_tx(0) >= min && air.secs() < max, "{bw:?} noise {noise}: {:.1} s\n{}", air.secs(), air.trace());
    }
}

#[test]
fn lead_in_adapts_after_the_first_report() {
    let air = session([FmBandwidth::Narrow; 2], false, 0.002, payload(1000, 10), payload(300, 11));
    let leads: Vec<String> = air.events[0]
        .iter()
        .filter_map(|(_, r)| match r {
            Report::Trace(t) if t.starts_with("tx c") => Some(t.split(' ').take(3).collect::<Vec<_>>().join(" ")),
            _ => None,
        })
        .collect();
    assert_eq!(leads[0], "tx c14 lead", "{leads:?}");
    assert!(
        air.events[0]
            .iter()
            .any(|(_, r)| matches!(r, Report::Trace(t) if t.starts_with("tx c") && t.contains("lead 1 "))),
        "{leads:?}"
    );
}

fn kiss_lines(a: &Air, i: usize) -> Vec<String> {
    a.events[i]
        .iter()
        .filter_map(|(_, r)| match r {
            Report::Line(l) => Some(l.clone()),
            Report::Kiss(c, d) => Some(format!("KISS {} {}", c.byte(), String::from_utf8_lossy(d))),
            Report::Host(HostEvent::Pending) => Some("PENDING".into()),
            Report::Host(HostEvent::CancelPending) => Some("CANCELPENDING".into()),
            Report::Ptt(on) => Some(format!("PTT {on}")),
            Report::Bitrate { level, bps, tx } => Some(format!("BITRATE {level} {bps} {tx}")),
            _ => None,
        })
        .collect()
}

#[test]
fn record16_kiss_and_cq_over_audio() {
    let mut a = fm_air([FmBandwidth::Narrow; 2], false, 0.0, Lead::default());
    a.st[1].set_kiss_rx(true);
    a.st[1].refresh_listen();
    a.st[1].update(|c| c.host.chat = true);
    assert!(a.st[0].kiss(Command::Generic, b"Hello".to_vec()));
    assert!(!a.st[0].cq_frame("N0CALL", None));
    assert!(a.run_until(6.0, |a| kiss_lines(a, 1).contains(&"KISS 2 Hello".to_string())));
    assert!(a.run_until(2.0, |a| a.st[0].idle()));
    assert_eq!(kiss_lines(&a, 0), ["PENDING", "BITRATE 1 566 true", "PTT true", "PTT false", "CANCELPENDING"]);
    assert!(a.st[0].cq_frame("N0CALL-5", Some("NARROW")));
    assert!(a.run_until(6.0, |a| kiss_lines(a, 1).iter().any(|l| l.starts_with("CQFRAME"))));
    assert!(kiss_lines(&a, 1).contains(&"CQFRAME N0CALL-5".to_string()));
    a.st[1].set_kiss_rx(false);
    a.st[1].refresh_listen();
    assert!(a.run_until(2.0, |a| a.st[0].idle()));
    let n = kiss_lines(&a, 1).len();
    assert!(a.st[0].cq_frame("N0CALL", None));
    a.run_until(5.0, |_| false);
    assert_eq!(kiss_lines(&a, 1).len(), n);
    assert!(a.st[0].idle());
    assert_eq!(a.st[1].state(), fm::State::Listen);
}

fn led(ms: [i64; 2], told: bool) -> Result<Air, Failure> {
    let lead = Lead { ms, told };
    session_led([FmBandwidth::Narrow; 2], true, 0.002, payload(8000, 12), payload(1500, 13), lead)
}

#[test]
fn ptt_lead_500_ms_at_both_ends() {
    let a = led([500, 500], true).unwrap();
    assert!(a.top_tx(0) >= 6 && a.secs() < 45.0, "{:.1} s\n{}", a.secs(), a.trace());
}

#[test]
fn ptt_lead_700_ms_at_both_ends_no_longer_slows_the_link() {
    let (before, after) = (led([700, 700], false).unwrap(), led([700, 700], true).unwrap());
    assert!(after.secs() + 5.0 < before.secs(), "unaware {:.1} s, aware {:.1} s", before.secs(), after.secs());
}

#[test]
fn ptt_lead_1000_ms_at_both_ends() {
    let after = led([1000, 1000], true).unwrap();
    assert!(after.secs() < 50.0, "aware {:.1} s", after.secs());
    let before = led([1000, 1000], false);
    let slow = match &before {
        Err(Failure::Stalled(_)) => true,
        Ok(b) => b.secs() > 2.0 * after.secs(),
        Err(_) => false,
    };
    assert!(slow, "unaware: {:?}, aware {:.1} s", before.map(|a| a.secs()), after.secs());
}

fn ptt_off_at(a: &Air, i: usize) -> f64 {
    a.events[i].iter().rev().find(|(_, r)| *r == Report::Ptt(false)).map(|(t, _)| *t).unwrap()
}

#[test]
fn record16_ptt_report_waits_for_the_delayed_audio() {
    let mut off = Vec::new();
    for ms in [0, 300] {
        let mut a = fm_air([FmBandwidth::Narrow; 2], false, 0.0, Lead { ms: [ms, 0], told: true });
        a.st[1].set_kiss_rx(true);
        a.st[1].refresh_listen();
        assert!(a.st[0].kiss(Command::Generic, b"Hello".to_vec()));
        assert!(a.run_until(6.0, |a| kiss_lines(a, 1).contains(&"KISS 2 Hello".to_string())));
        assert!(a.run_until(2.0, |a| a.st[0].idle()));
        assert_eq!(kiss_lines(&a, 0), ["PENDING", "BITRATE 1 566 true", "PTT true", "PTT false", "CANCELPENDING"]);
        off.push(ptt_off_at(&a, 0));
    }
    assert!((off[1] - off[0] - 0.3).abs() < 0.015, "{off:?}");
}
