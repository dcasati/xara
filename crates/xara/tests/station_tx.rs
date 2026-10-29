use xara::station::hf::Station;
use xara::station::{Modem, Options, Report, TX_RING_BYTES};
use xara_arq::hf::session::{Config, State};
use xara_arq::kiss::Command;
use xara_phy::dsp::quantise;
use xara_phy::hf::ctrl::{self, Fmt};
use xara_phy::hf::level::Bandwidth;

fn station(drive_db: f64) -> Station {
    Station::new(Config::new(Bandwidth::Hz2300, "N0CALL"), Options { drive_db, ..Options::default() })
}

fn run(st: &mut Station, n: usize) -> (Vec<f64>, Vec<Report>) {
    let mut out = vec![0.0; n];
    st.process(&vec![0.0; n], &mut out);
    (out, std::iter::from_fn(|| st.pop()).collect())
}

#[test]
fn tune_tone_level_and_ptt() {
    let mut st = station(-10.0);
    assert!(st.tune(true));
    let (x, rep) = run(&mut st, 4800);
    assert!(rep.contains(&Report::Ptt(true)));
    let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    assert_eq!(peak, quantise(std::f64::consts::SQRT_2 * 10f64.powf((-10.0 - 6.4) / 20.0)));
    assert_eq!((peak * 32767.0).round(), 7014.0);
    assert!(st.tune(false));
    let (x, rep) = run(&mut st, 480);
    assert!(rep.contains(&Report::Ptt(false)));
    assert!(x.iter().all(|&v| v == 0.0));
}

#[test]
fn a_ptt_lead_holds_the_ptt_report_until_the_audio_is_out() {
    for (start, now) in [(4800, None), (0, Some(4800)), (1000, Some(4800))] {
        let opts = Options { tx_latency: start, ..Options::default() };
        let mut st = Station::new(Config::new(Bandwidth::Hz2300, "N0CALL"), opts);
        if let Some(t) = now {
            st.set_tx_latency(t);
        }
        assert_eq!(st.options().tx_latency, 4800);
        assert!(st.tune(true));
        assert!(run(&mut st, 4800).1.contains(&Report::Ptt(true)));
        assert!(st.tune(false));
        let (x, rep) = run(&mut st, 4800);
        assert!(x.iter().all(|&v| v == 0.0));
        assert!(!rep.contains(&Report::Ptt(false)) && st.ptt() && !st.idle());
        assert_eq!(run(&mut st, 1).1, [Report::Ptt(false)]);
        assert!(!st.ptt() && st.idle());
    }
}

#[test]
fn tune_refused_outside_listen() {
    let mut st = station(-5.0);
    assert!(st.connect("N0CALL", "N1CALL", &[]));
    assert_eq!(st.state(), State::Calling);
    assert!(!st.tune(true));
}

#[test]
fn control_frame_follows_level_law() {
    let mut st = station(-5.0);
    st.connect("N0CALL", "N1CALL", &[]);
    let mut burst = Vec::new();
    let mut fmt = None;
    for _ in 0..200 {
        let (x, rep) = run(&mut st, 4800);
        burst.extend(x.into_iter().filter(|&v| v != 0.0));
        for r in rep {
            if let Report::Trace(t) = &r {
                fmt = fmt.or_else(|| {
                    t.strip_prefix("tx c")?.split(" f").nth(1)?[..1].parse().ok().and_then(Fmt::from_index)
                });
            }
            if r == Report::Ptt(false) && !burst.is_empty() {
                let fmt = fmt.unwrap();
                let rms = (burst.iter().map(|v: &f64| v * v).sum::<f64>() / burst.len() as f64).sqrt();
                let want = ctrl::gain(fmt, -5.0);
                assert!((rms - want).abs() < 5e-3 * want, "fmt{fmt}: {rms} vs {want}");
                return;
            }
        }
    }
    panic!("no control frame");
}

#[test]
fn control_gain_at_minus_5_db() {
    assert!((ctrl::gain(Fmt::F1, -5.0) * std::f64::consts::SQRT_2 - 0.38064).abs() < 1e-5);
}

#[test]
fn data_before_a_link_is_bounded_by_the_tx_ring() {
    let mut st = station(-5.0);
    st.send(&vec![0x55; TX_RING_BYTES - 10]);
    st.send(&[0x55; 30]);
    assert_eq!(st.buffered(), TX_RING_BYTES);
}

#[test]
fn fm_kiss_queue_is_bounded_like_hf() {
    let mut fm = xara::station::fm::Station::new(
        xara_arq::fm::Config::new(xara_phy::fm::level::FmBandwidth::Narrow, "N0CALL"),
        Options::default(),
    );
    assert!(fm.kiss(Command::Generic, vec![b'a'; 200]));
    assert!(fm.kiss(Command::Generic, b"queued next".to_vec()));
    assert!(!fm.kiss(Command::Generic, vec![b'a'; xara_arq::kiss::MAX_QUEUED_BYTES]));
    let mut hf = station(-5.0);
    assert!(hf.kiss(Command::Generic, vec![b'a'; 200]));
    assert!(hf.kiss(Command::Generic, b"queued next".to_vec()));
    assert!(!hf.kiss(Command::Generic, vec![b'a'; xara_arq::kiss::MAX_QUEUED_BYTES]));
}
