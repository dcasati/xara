#[cfg(feature = "alsa")]
pub mod alsa;
#[cfg(feature = "pulse")]
pub mod pulse;
#[cfg(feature = "pulse")]
mod pulse_ffi;
pub mod ratematch;

#[cfg(not(any(feature = "pulse", feature = "alsa")))]
compile_error!("xara needs an audio backend: enable the pulse or alsa feature");

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use ratematch::{Queue, TxBuffer};

pub const RATE: u32 = 48000;
pub const BLOCK: usize = 480;
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(1);
const BEHIND: usize = RATE as usize / 10;

pub type DeviceList = Vec<(String, String)>;

fn capture_only() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "write to a capture-only audio device")
}

fn remaining(until: Instant, what: &str) -> io::Result<Duration> {
    until
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, format!("{what}: timed out")))
}

pub trait Audio {
    fn read(&mut self, x: &mut [i16]) -> io::Result<()>;
    fn write(&mut self, x: &[i16]) -> io::Result<()>;
    fn queued(&mut self) -> io::Result<Option<Queue>>;
    fn backlog(&mut self) -> io::Result<Option<usize>>;
}

pub struct Behind {
    report: usize,
    start: Option<usize>,
    armed: bool,
}

impl Default for Behind {
    fn default() -> Self {
        Self::new()
    }
}

impl Behind {
    pub fn new() -> Self {
        Self { report: BEHIND, start: None, armed: false }
    }

    pub fn tick(&mut self, backlog: Option<usize>) {
        if let Some(n) = self.over(backlog) {
            say!(
                "audio: {} ms of capture waiting to be read (xara is falling behind real time: CPU overload?)",
                n * 1000 / RATE as usize
            );
        }
    }

    fn over(&mut self, backlog: Option<usize>) -> Option<usize> {
        let n = backlog?;
        let start = *self.start.get_or_insert(n);
        self.armed |= n < BEHIND || n >= start + BEHIND;
        if !self.armed || n < self.report {
            return None;
        }
        while self.report <= n {
            self.report *= 2;
        }
        Some(n)
    }
}

pub fn sample(v: i16) -> f64 {
    f64::from(v) / 32768.0
}

pub fn quantize(v: f64) -> i16 {
    (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

pub fn skip_backlog(io: &mut dyn Audio) -> io::Result<()> {
    let mut x = [0; BLOCK];
    for _ in 0..io.backlog()?.unwrap_or(0) / BLOCK {
        io.read(&mut x)?;
    }
    Ok(())
}

pub struct TxAudio {
    delay: VecDeque<f64>,
}

impl TxAudio {
    pub fn new(lead_samples: usize) -> Self {
        Self { delay: std::iter::repeat_n(0.0, lead_samples).collect() }
    }

    pub fn process(&mut self, out: &mut [f64], mute: bool) {
        if mute {
            out.fill(0.0);
            self.delay.iter_mut().for_each(|v| *v = 0.0);
        }
        if !self.delay.is_empty() {
            self.delay.extend(out.iter().copied());
            for o in out.iter_mut() {
                *o = self.delay.pop_front().unwrap();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Pulse,
    Alsa,
}

impl Default for Backend {
    fn default() -> Self {
        if cfg!(feature = "pulse") { Self::Pulse } else { Self::Alsa }
    }
}

impl std::str::FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let b = match s {
            "pulse" => Self::Pulse,
            "alsa" => Self::Alsa,
            _ => return Err(format!("--audio {s:?}: expected pulse or alsa")),
        };
        if b.built() { Ok(b) } else { Err(format!("--audio {s}: this xara was built without the {s} feature")) }
    }
}

impl Backend {
    pub fn built(self) -> bool {
        match self {
            Self::Pulse => cfg!(feature = "pulse"),
            Self::Alsa => cfg!(feature = "alsa"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::Alsa => "alsa",
        }
    }
}

#[cfg(not(all(feature = "pulse", feature = "alsa")))]
fn unbuilt(b: Backend) -> io::Error {
    io::Error::other(format!("this xara was built without the {} feature", b.name()))
}

pub fn open(b: Backend, capture: &str, playback: &str, radio: bool, tx: TxBuffer) -> io::Result<Box<dyn Audio>> {
    match b {
        #[cfg(feature = "pulse")]
        Backend::Pulse => Ok(Box::new(pulse::PulseIo::open(capture, playback, radio, tx)?)),
        #[cfg(feature = "alsa")]
        Backend::Alsa => Ok(Box::new(alsa::AlsaIo::open(capture, playback, radio, tx)?)),
        #[cfg(not(all(feature = "pulse", feature = "alsa")))]
        _ => Err(unbuilt(b)),
    }
}

pub fn check_playback(b: Backend, playback: &str, radio: bool) -> io::Result<()> {
    match b {
        #[cfg(feature = "pulse")]
        Backend::Pulse => pulse::check(playback, radio),
        #[cfg(feature = "alsa")]
        Backend::Alsa => alsa::resolve_playback(playback, radio).map(drop),
        #[cfg(not(all(feature = "pulse", feature = "alsa")))]
        _ => Err(unbuilt(b)),
    }
}

pub fn list(b: Backend) -> io::Result<(DeviceList, DeviceList)> {
    match b {
        #[cfg(feature = "pulse")]
        Backend::Pulse => pulse::list(),
        #[cfg(feature = "alsa")]
        Backend::Alsa => alsa::list(),
        #[cfg(not(all(feature = "pulse", feature = "alsa")))]
        _ => Err(unbuilt(b)),
    }
}

pub fn open_capture(b: Backend, capture: &str) -> io::Result<Box<dyn Audio>> {
    match b {
        #[cfg(feature = "pulse")]
        Backend::Pulse => Ok(Box::new(pulse::PulseIo::open_capture(capture)?)),
        #[cfg(feature = "alsa")]
        Backend::Alsa => Ok(Box::new(alsa::AlsaIo::open_capture(capture)?)),
        #[cfg(not(all(feature = "pulse", feature = "alsa")))]
        _ => Err(unbuilt(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_convert_at_the_device_boundary() {
        for s in [i16::MIN, -1, 0, 1, 12345, i16::MAX] {
            assert_eq!(quantize(sample(s)), s);
        }
        assert_eq!((quantize(1.0), quantize(-1.0), quantize(4.0), quantize(-4.0)), (32767, -32768, 32767, -32768));
        assert_eq!((quantize(0.5 / 32768.0), quantize(-0.4 / 32768.0), quantize(f64::NAN)), (1, 0, 0));
        assert_eq!(sample(-32768), -1.0);
    }

    fn reports(ms: &[usize]) -> Vec<usize> {
        let (mut b, k) = (Behind::new(), RATE as usize / 1000);
        ms.iter().filter_map(|&m| b.over(Some(m * k))).map(|n| n / k).collect()
    }

    #[test]
    fn capture_backlog_reports_at_doubling_thresholds() {
        assert_eq!(reports(&[0, 10, 21, 99, 0]), Vec::<usize>::new());
        assert_eq!(
            reports(&[0, 100, 150, 199, 0, 100, 200, 390, 400, 0, 1000, 1500, 1600]),
            [100, 200, 400, 1000, 1600]
        );
        assert_eq!(reports(&[10, 180, 199]), [180]);
        let mut b = Behind::new();
        assert_eq!(
            (b.over(None), b.over(Some(0)), b.over(None), b.over(Some(BEHIND))),
            (None, None, None, Some(BEHIND))
        );
    }

    #[test]
    fn a_startup_backlog_is_ignored_until_it_drains_or_grows() {
        assert_eq!(reports(&[300, 250, 150, 100, 60, 0, 120]), [120]);
        assert_eq!(reports(&[300, 250, 150, 99, 100]), [100]);
        assert_eq!(reports(&[300, 320, 390, 400, 420, 800]), [400, 800]);
        assert_eq!(reports(&[2000, 1000, 500]), Vec::<usize>::new());
    }

    struct Backlogged {
        waiting: Option<usize>,
        reads: usize,
        drains: bool,
    }

    impl Audio for Backlogged {
        fn read(&mut self, _: &mut [i16]) -> io::Result<()> {
            self.reads += 1;
            if self.drains {
                self.waiting = self.waiting.map(|n| n - BLOCK);
            }
            Ok(())
        }

        fn write(&mut self, _: &[i16]) -> io::Result<()> {
            Ok(())
        }

        fn queued(&mut self) -> io::Result<Option<Queue>> {
            Ok(None)
        }

        fn backlog(&mut self) -> io::Result<Option<usize>> {
            Ok(self.waiting)
        }
    }

    #[test]
    fn startup_capture_is_skipped_in_whole_blocks() {
        for (waiting, drains, reads, left) in [
            (None, true, 0, None),
            (Some(BLOCK - 1), true, 0, Some(BLOCK - 1)),
            (Some(14 * BLOCK + 100), true, 14, Some(100)),
            (Some(3 * BLOCK), false, 3, Some(3 * BLOCK)),
        ] {
            let mut io = Backlogged { waiting, reads: 0, drains };
            skip_backlog(&mut io).unwrap();
            assert_eq!((io.reads, io.waiting), (reads, left), "{waiting:?} {drains}");
        }
    }
}
