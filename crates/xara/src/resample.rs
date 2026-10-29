use std::f64::consts::PI;

const HALF: f64 = 24.0;
const PHASES: f64 = 512.0;

const BETA: f64 = 8.0;

fn bessel(v: f64) -> f64 {
    let (mut s, mut term) = (1.0, 1.0);
    for k in 1..30 {
        term *= (v / 2.0) / k as f64;
        s += term * term;
    }
    s
}

fn kaiser(x: f64, norm: f64) -> f64 {
    if x.abs() >= 1.0 { 0.0 } else { bessel(BETA * (1.0 - x * x).sqrt()) / norm }
}

pub(crate) struct Kernel {
    half: usize,
    phases: usize,
    table: Vec<f64>,
}

impl Kernel {
    pub(crate) fn new(half: usize, phases: usize, cutoff: f64) -> Self {
        let (n, norm) = (2 * half, bessel(BETA));
        let mut table = vec![0.0; (phases + 1) * n];
        for (p, h) in table.chunks_exact_mut(n).enumerate() {
            let frac = p as f64 / phases as f64;
            for (k, c) in h.iter_mut().enumerate() {
                let u = frac + (half - 1) as f64 - k as f64;
                let a = PI * cutoff * u;
                let s = if a.abs() < 1e-12 { 1.0 } else { a.sin() / a };
                *c = s * kaiser(u / half as f64, norm);
            }
            let sum: f64 = h.iter().sum();
            h.iter_mut().for_each(|c| *c /= sum);
        }
        Self { half, phases, table }
    }

    fn apply(&self, x: &[f64], frac: f64) -> f64 {
        let n = 2 * self.half;
        debug_assert_eq!(x.len(), n);
        let pos = frac * self.phases as f64;
        let p = (pos as usize).min(self.phases - 1);
        let w = pos - p as f64;
        let (a, b) = self.table[p * n..(p + 2) * n].split_at(n);
        x.iter().zip(a.iter().zip(b)).map(|(v, (c0, c1))| v * (c0 + w * (c1 - c0))).sum()
    }
}

pub(crate) struct Stream {
    kernel: Kernel,
    buf: Vec<f64>,
    i: usize,
    frac: f64,
}

impl Stream {
    pub(crate) fn new(kernel: Kernel, lag: usize) -> Self {
        let i = kernel.half - 1;
        Self { kernel, buf: vec![0.0; i + lag], i, frac: 0.0 }
    }

    pub(crate) fn half(&self) -> usize {
        self.kernel.half
    }

    pub(crate) fn silent(&self) -> bool {
        self.buf.iter().all(|&v| v == 0.0)
    }

    pub(crate) fn run(&mut self, x: &[f64], whole: usize, part: f64, out: &mut Vec<f64>) {
        debug_assert!((0.0..1.0).contains(&part));
        self.buf.extend_from_slice(x);
        let half = self.kernel.half;
        while self.i + half < self.buf.len() {
            out.push(self.kernel.apply(&self.buf[self.i + 1 - half..=self.i + half], self.frac));
            self.i += whole;
            self.frac += part;
            if self.frac >= 1.0 {
                self.frac -= 1.0;
                self.i += 1;
            }
        }
        let used = self.i + 1 - half;
        self.buf.drain(..used);
        self.i -= used;
    }
}

pub struct Resampler {
    whole: usize,
    part: f64,
    stream: Stream,
}

pub const MIN_RATE: f64 = 1000.0;
pub const MAX_RATE: f64 = 1_000_000.0;

pub fn check_rate(rate: f64) -> Result<f64, String> {
    if (MIN_RATE..=MAX_RATE).contains(&rate) {
        Ok(rate)
    } else {
        Err(format!("sample rate {rate} Hz: must be {MIN_RATE}-{MAX_RATE} Hz"))
    }
}

impl Resampler {
    pub fn new(rate_in: f64, rate_out: f64) -> Result<Self, String> {
        let (rate_in, rate_out) = (check_rate(rate_in)?, check_rate(rate_out)?);
        let cutoff = 0.95 * (rate_out / rate_in).min(1.0);
        let half = (HALF / cutoff).ceil() as usize;
        let kernel = Kernel::new(half, (PHASES * cutoff).ceil() as usize, cutoff);
        let step = rate_in / rate_out;
        let whole = step.floor();
        Ok(Self { whole: whole as usize, part: step - whole, stream: Stream::new(kernel, 0) })
    }

    pub fn identity(&self) -> bool {
        self.whole == 1 && self.part == 0.0
    }

    pub fn process(&mut self, x: &[f64]) -> Vec<f64> {
        if self.identity() {
            return x.to_vec();
        }
        let mut out = Vec::new();
        self.stream.run(x, self.whole, self.part, &mut out);
        out
    }

    pub fn flush(&mut self) -> Vec<f64> {
        if self.identity() {
            return Vec::new();
        }
        self.process(&vec![0.0; self.stream.half() + 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, fs: f64, n: usize) -> Vec<f64> {
        (0..n).map(|i| (2.0 * PI * hz * i as f64 / fs).sin()).collect()
    }

    fn run(r: &mut Resampler, x: &[f64], chunk: usize) -> Vec<f64> {
        let mut y: Vec<f64> = x.chunks(chunk).flat_map(|c| r.process(c)).collect();
        y.extend(r.flush());
        y
    }

    fn check(rate_in: f64, rate_out: f64, hz: f64) {
        let mut r = Resampler::new(rate_in, rate_out).unwrap();
        let y: Vec<f64> = tone(hz, rate_in, rate_in as usize).chunks(4096).flat_map(|c| r.process(c)).collect();
        let want = tone(hz, rate_out, y.len());
        let err: f64 = y[400..y.len() - 400].iter().zip(&want[400..]).map(|(a, b)| (a - b).powi(2)).sum::<f64>()
            / (y.len() - 800) as f64;
        let at = format!("{rate_in}->{rate_out} {hz} Hz");
        assert!((y.len() as f64 - rate_out).abs() < 200.0, "{at}: {}", y.len());
        assert!(err < 1e-6, "{at}: {err}");
    }

    #[test]
    fn upsampled_tone_keeps_frequency_and_level() {
        check(12000.0, 48000.0, 1500.0);
        check(8000.0, 48000.0, 3000.0);
    }

    #[test]
    fn downsampled_tone_keeps_frequency_and_level() {
        for (rate_in, hz) in
            [(96000.0, 1500.0), (96000.0, 20000.0), (44100.0, 1500.0), (44100.0, 15000.0), (1e6, 1500.0)]
        {
            check(rate_in, 48000.0, hz);
        }
    }

    #[test]
    fn chunking_does_not_change_output() {
        for (rate_in, rate_out) in [(12000.0, 48000.0), (44100.0, 48000.0), (96000.0, 48000.0)] {
            let x = tone(1500.0, rate_in, 5000);
            let whole = run(&mut Resampler::new(rate_in, rate_out).unwrap(), &x, x.len());
            for chunk in [1, 7, 1000] {
                assert_eq!(run(&mut Resampler::new(rate_in, rate_out).unwrap(), &x, chunk), whole, "{rate_in} {chunk}");
            }
        }
    }

    #[test]
    fn untrusted_rates_are_refused() {
        for r in [0.0, -48000.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 999.0, 1e6 + 1.0, f64::MIN_POSITIVE] {
            assert!(Resampler::new(r, 48000.0).is_err(), "{r}");
            assert!(Resampler::new(48000.0, r).is_err(), "{r}");
        }
        assert!(Resampler::new(1000.0, 1e6).is_ok());
    }
}
