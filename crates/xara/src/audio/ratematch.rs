use super::{BLOCK, RATE};
use crate::resample::{Kernel, Stream};
use std::time::Duration;

pub const DEFAULT_MS: u32 = 40;
pub const MIN_MS: u32 = 30;
pub const MAX_MS: u32 = 500;
const HALF: usize = 8;
const PHASES: usize = 512;
const CUTOFF: f64 = 0.9;
const SMOOTH_BLOCKS: f64 = 200.0;
const KP_PER_FRAME: f64 = 2.5e-6;
const KP_FAR_PER_FRAME: f64 = 5e-6;
const KI_PER_FRAME_BLOCK: f64 = 1e-9;
const P_CAP: f64 = 5e-4;
const KEYED_CAP: f64 = 1e-3;
const FAR_FRAMES: f64 = 240.0;
const MIN_FLOOR_FRAMES: f64 = 960.0;
pub const MAX_SKEW: f64 = 0.01;
const SKEW_REPORT: f64 = 1e-3;
const STALL: Duration = Duration::from_millis(30);
const SLOW: Duration = Duration::from_millis(20);
const SKIP_BLOCKS: usize = 20;
const SETTLE_BLOCKS: usize = 50;
const REARM_BLOCKS: usize = 1000;
const MOVED_FRAMES: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxBuffer {
    pub target: usize,
}

impl Default for TxBuffer {
    fn default() -> Self {
        Self::ms(DEFAULT_MS)
    }
}

impl TxBuffer {
    pub fn ms(ms: u32) -> Self {
        Self { target: ms as usize * (RATE / 1000) as usize }
    }

    pub fn capacity(self) -> usize {
        4 * self.target
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    pub frames: usize,
    pub floor: usize,
    pub underruns: u64,
}

impl Queue {
    pub fn ours(self) -> usize {
        self.frames.saturating_sub(self.floor)
    }
}

pub struct Interp(Stream);

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}

impl Interp {
    pub const DELAY: usize = HALF;

    pub fn new() -> Self {
        Self(Stream::new(Kernel::new(HALF, PHASES, CUTOFF), 1))
    }

    pub fn silent(&self) -> bool {
        self.0.silent()
    }

    pub fn process(&mut self, x: &[f64], step: f64, out: &mut Vec<f64>) {
        out.clear();
        let whole = step.floor();
        self.0.run(x, whole as usize, step - whole, out);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    Aligned { start: f64, hold: f64, target: f64 },
    Realign(i64),
    Latency(usize),
    Skew(f64),
    Pinned,
    Headroom { hold: f64, headroom: f64 },
    NoQueue,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    pub ppm: f64,
    pub skew: f64,
    pub fill: f64,
    pub hold: f64,
    pub underruns: u64,
}

struct Hold {
    skip: usize,
    sum: f64,
    floor: f64,
    lag: Option<f64>,
    seen: usize,
    n: usize,
    all: f64,
    ticks: usize,
}

struct Settled {
    mean: f64,
    floor: f64,
    lag: f64,
}

impl Hold {
    fn new(skip: usize) -> Self {
        Self { skip, sum: 0.0, floor: 0.0, lag: Some(0.0), seen: 0, n: 0, all: 0.0, ticks: 0 }
    }

    fn tick(&mut self, q: Queue, lag: Option<usize>) -> Option<Settled> {
        let ours = q.ours() as f64;
        self.all += ours;
        self.ticks += 1;
        if self.skip > 0 {
            self.skip -= 1;
            return None;
        }
        self.sum += ours;
        self.floor += q.floor as f64;
        self.lag = match lag {
            Some(0) => None,
            Some(n) => self.lag.map(|s| s + n as f64),
            None => self.lag,
        };
        self.seen += usize::from(lag.is_some());
        self.n += 1;
        (self.n == SETTLE_BLOCKS).then(|| Settled {
            mean: self.sum / SETTLE_BLOCKS as f64,
            floor: self.floor / SETTLE_BLOCKS as f64,
            lag: self.lag.map_or(0.0, |s| s / self.seen.max(1) as f64),
        })
    }
}

fn excess(backlog: usize, busy: Duration) -> usize {
    if backlog < BLOCK { 0 } else { backlog + (busy.as_secs_f64() * f64::from(RATE)) as usize }
}

#[derive(Default)]
struct Stalls {
    underruns: u64,
    rearm: usize,
    slow: usize,
}

impl Stalls {
    fn check(&mut self, q: Option<Queue>, busy: Duration, short: f64) -> Option<Then> {
        self.rearm = self.rearm.saturating_sub(1);
        self.slow = if busy > SLOW { SKIP_BLOCKS } else { self.slow.saturating_sub(1) };
        let armed = self.rearm == 0 || self.slow > 0;
        let underran = q.is_some_and(|q| q.underruns > self.underruns) && armed && short < FAR_FRAMES;
        self.underruns = q.map_or(self.underruns, |q| q.underruns);
        if underran {
            self.rearm = REARM_BLOCKS;
        }
        if busy > STALL || underran && self.slow > 0 { Some(Then::Check) } else { underran.then_some(Then::Trim) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owed {
    Nothing,
    Pad(i64),
    Realign(Option<i64>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    Align,
    Check,
    Trim,
    Report,
}

enum State {
    Measure(Hold, Then),
    Run(Owed),
    Open,
}

struct Control {
    target: f64,
    capacity: f64,
    set: f64,
    fill: f64,
    floor: f64,
    lag: f64,
    told: Option<usize>,
    integral: f64,
    state: State,
    stalls: Stalls,
    calm: usize,
    report: f64,
    pinned: bool,
    raised: f64,
    cap: f64,
}

impl Control {
    fn new(b: TxBuffer) -> Self {
        Self {
            target: b.target as f64,
            capacity: b.capacity() as f64,
            set: b.target as f64,
            fill: b.target as f64,
            floor: 0.0,
            lag: 0.0,
            told: None,
            integral: 0.0,
            state: State::Measure(Hold::new(SKIP_BLOCKS), Then::Align),
            stalls: Stalls::default(),
            calm: 0,
            report: SKEW_REPORT,
            pinned: false,
            raised: 0.0,
            cap: f64::INFINITY,
        }
    }

    fn step(&self, silent: bool) -> f64 {
        if let State::Open = self.state {
            return 1.0;
        }
        1.0 + (self.proportional(silent) + self.integral).clamp(-MAX_SKEW, MAX_SKEW)
    }

    fn proportional(&self, silent: bool) -> f64 {
        let e = self.error();
        let guard = (self.fill - self.capacity + self.target / 2.0).max(0.0);
        let far = e.signum() * (e.abs() - FAR_FRAMES).max(0.0) + guard;
        let p = (KP_PER_FRAME * e).clamp(-P_CAP, P_CAP) + KP_FAR_PER_FRAME * far;
        if silent { p } else { p.clamp(-KEYED_CAP, KEYED_CAP) }
    }

    fn error(&self) -> f64 {
        match self.state {
            State::Run(Owed::Realign(None)) => 0.0,
            State::Run(Owed::Pad(n) | Owed::Realign(Some(n))) => self.fill + n as f64 - self.set,
            _ => self.fill - self.set,
        }
    }

    fn latency(&self) -> usize {
        self.told.unwrap_or(self.target as usize + Interp::DELAY)
    }

    fn align(&mut self, out: &mut Vec<f64>) {
        let (mut n, then) = match self.state {
            State::Run(Owed::Pad(n)) => (n, Then::Report),
            State::Run(Owed::Realign(None)) => {
                self.fill += std::mem::take(&mut self.lag);
                ((self.set - self.fill).round() as i64, Then::Check)
            }
            State::Run(Owed::Realign(Some(n))) => (n, Then::Check),
            _ => return,
        };
        let before = out.len();
        pad(&mut n, out);
        self.fill += out.len() as f64 - before as f64;
        self.state = match (n, then) {
            (0, _) => State::Measure(Hold::new(SKIP_BLOCKS), then),
            (_, Then::Report) => State::Run(Owed::Pad(n)),
            _ => State::Run(Owed::Realign(Some(n))),
        };
    }

    fn steer(&mut self, q: Option<Queue>, backlog: Option<usize>, busy: Duration) -> Vec<Event> {
        let mut ev = Vec::new();
        if let Some(seen) = self.stalls.check(q, busy, self.set - self.fill) {
            if let State::Measure(h @ Hold { ticks: 1.., .. }, Then::Align) = &self.state {
                self.cap = self.cap.min(h.all / h.ticks as f64);
            }
            let behind = (busy.as_secs_f64() * f64::from(RATE) / BLOCK as f64).ceil() as usize + SKIP_BLOCKS;
            let (skip, then) = match &self.state {
                State::Measure(h, keep @ (Then::Align | Then::Check)) => (h.skip.max(behind), *keep),
                State::Measure(h, _) => (h.skip.max(behind), seen),
                State::Run(Owed::Nothing) => (behind, seen),
                State::Run(_) => (behind, Then::Check),
                State::Open => return ev,
            };
            if then == Then::Check {
                self.calm = REARM_BLOCKS;
            }
            self.state = State::Measure(Hold::new(skip), then);
        }
        let Some(q) = q else { return ev };
        let lag = backlog.map(|n| excess(n, busy));
        match &mut self.state {
            State::Run(_) => self.track(q, lag, &mut ev),
            State::Open => {}
            State::Measure(h, then) => {
                let then = *then;
                if let Some(s) = h.tick(q, lag) {
                    self.settle(s, then, &mut ev);
                }
            }
        }
        ev
    }

    fn track(&mut self, q: Queue, lag: Option<usize>, ev: &mut Vec<Event>) {
        self.fill += (q.ours() as f64 - self.fill) / SMOOTH_BLOCKS;
        self.floor += (q.floor as f64 - self.floor) / SMOOTH_BLOCKS;
        if let (State::Run(Owed::Realign(None)), Some(n)) = (&self.state, lag) {
            self.lag = if n == 0 { 0.0 } else { self.lag + (n as f64 - self.lag) / SMOOTH_BLOCKS };
        }
        if let State::Run(Owed::Nothing) = self.state {
            self.calm = self.calm.saturating_sub(1);
            if self.calm == 0 {
                self.integral = (self.integral + KI_PER_FRAME_BLOCK * self.error()).clamp(-MAX_SKEW, MAX_SKEW);
            }
            if self.told.is_some() && self.moved() {
                self.state = State::Measure(Hold::new(SKIP_BLOCKS), Then::Report);
            }
        }
        let skew = self.integral.abs();
        if skew >= self.report {
            self.report *= 2.0;
            ev.push(Event::Skew(-self.integral * 1e6));
        }
        if skew >= MAX_SKEW && !std::mem::replace(&mut self.pinned, true) {
            ev.push(Event::Pinned);
        }
    }

    fn path(&self) -> usize {
        (self.set + self.floor).round() as usize + Interp::DELAY
    }

    fn moved(&self) -> bool {
        self.told.is_none_or(|n| n.abs_diff(self.path()) > MOVED_FRAMES)
    }

    fn settle(&mut self, Settled { mean, floor, lag }: Settled, then: Then, ev: &mut Vec<Event>) {
        (self.fill, self.floor, self.lag) = (mean, floor, lag);
        if then == Then::Align {
            if mean + floor < BLOCK as f64 {
                self.state = State::Open;
                ev.push(Event::NoQueue);
                return;
            }
            let level = mean.min(self.cap);
            if level - self.target >= MIN_FLOOR_FRAMES {
                self.set = level.min(2.0 * self.target);
            }
            self.state = State::Run(Owed::Pad((self.set - mean).round() as i64));
            ev.push(Event::Aligned { start: mean, hold: self.set, target: self.target });
            return;
        }
        let off = (self.set - mean - lag).round() as i64;
        let top = self.capacity - self.target;
        let short = then == Then::Trim && off as f64 > FAR_FRAMES && self.set < top;
        let far = match then {
            Then::Check => off.unsigned_abs() as f64 > FAR_FRAMES,
            Then::Trim => -off as f64 > FAR_FRAMES,
            _ => false,
        };
        if far {
            self.calm = REARM_BLOCKS;
        }
        self.state = State::Run(if far || short { Owed::Realign(None) } else { Owed::Nothing });
        if far {
            ev.push(Event::Realign(off));
        }
        if short {
            let set = (self.set + off as f64).min(top);
            self.raised += set - self.set;
            self.set = set;
            ev.push(Event::Headroom { hold: set, headroom: self.target + self.raised });
        }
        if self.moved() {
            let n = self.path();
            self.told = Some(n);
            ev.push(Event::Latency(n));
        }
    }
}

fn pad(frames: &mut i64, out: &mut Vec<f64>) {
    if *frames < 0 {
        let n = out.len().min(frames.unsigned_abs() as usize);
        out.truncate(out.len() - n);
        *frames += n as i64;
    } else {
        out.resize(out.len() + *frames as usize, 0.0);
        *frames = 0;
    }
}

pub struct RateMatch {
    interp: Interp,
    ctl: Control,
    step: f64,
    silent: bool,
}

impl Default for RateMatch {
    fn default() -> Self {
        Self::new(TxBuffer::default())
    }
}

impl RateMatch {
    pub fn new(b: TxBuffer) -> Self {
        Self { interp: Interp::new(), ctl: Control::new(b), step: 1.0, silent: true }
    }

    pub fn ppm(&self) -> f64 {
        (1.0 / self.step - 1.0) * 1e6
    }

    pub fn status(&self) -> Status {
        let c = &self.ctl;
        Status { ppm: self.ppm(), skew: -c.integral * 1e6, fill: c.fill, hold: c.set, underruns: c.stalls.underruns }
    }

    pub fn process(&mut self, x: &[f64], out: &mut Vec<f64>) {
        self.silent = x.iter().all(|&v| v == 0.0) && self.interp.silent();
        self.step = self.ctl.step(self.silent);
        self.interp.process(x, self.step, out);
        if self.silent {
            self.ctl.align(out);
        }
    }

    pub fn steer(&mut self, q: Option<Queue>, backlog: Option<usize>, busy: Duration) -> Vec<Event> {
        self.ctl.steer(q, backlog, busy)
    }

    pub fn latency(&self) -> usize {
        self.ctl.latency()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn tone(n: usize) -> Vec<f64> {
        (0..n).map(|i| 0.5 * (2.0 * PI * 1500.0 * i as f64 / f64::from(RATE)).sin()).collect()
    }

    #[test]
    fn unit_step_passes_a_tone_one_sample_late() {
        let (x, mut r) = (tone(48_000), RateMatch::default());
        let (mut y, mut out) = (Vec::new(), Vec::new());
        for b in x.chunks(BLOCK) {
            r.process(b, &mut out);
            y.extend_from_slice(&out);
        }
        assert!(y.len() + HALF + 1 >= x.len() && y.len() <= x.len(), "{}", y.len());
        let err = y[100..].iter().zip(&x[99..]).map(|(a, b)| (a - b).powi(2)).sum::<f64>() / (y.len() - 100) as f64;
        assert!(err < 1e-8, "{err}");
    }

    #[test]
    fn fractional_steps_interpolate_a_tone() {
        for hz in [1500.0, 4500.0] {
            let at = |t: f64| 0.5 * (2.0 * PI * hz * t / f64::from(RATE)).sin();
            let x: Vec<f64> = (0..48_000).map(|i| at(i as f64)).collect();
            let mut r = Interp::new();
            let (mut y, mut out) = (Vec::new(), Vec::new());
            for b in x.chunks(BLOCK) {
                r.process(b, 1.003, &mut out);
                y.extend_from_slice(&out);
            }
            let err = (100..y.len()).map(|n| (y[n] - at(n as f64 * 1.003 - 1.0)).powi(2)).sum::<f64>()
                / (y.len() - 100) as f64;
            assert!(err < 1e-8, "{hz}: {err}");
        }
    }

    const TARGET: usize = 1920;
    const FIRST_UPDATE: f64 = 480.0;
    const LAST_UPDATE: f64 = 72000.0;

    struct Sim {
        r: RateMatch,
        skew: f64,
        quantum: usize,
        chunk: f64,
        capture: f64,
        floor: usize,
        told: usize,
        cap: f64,
        queue: f64,
        clock: f64,
        frac: f64,
        done: f64,
        k: usize,
        underruns: u64,
        blocked: usize,
        keyed: bool,
        phase: usize,
        sparse: bool,
        written: f64,
        played: f64,
        update: (f64, f64),
        next: f64,
        every: f64,
        read: f64,
        flagged: u64,
        events: Vec<Event>,
        out: Vec<f64>,
    }

    impl Sim {
        fn new(skew: f64, quantum: usize, chunk: usize, floor: usize, backlog: f64) -> Self {
            Self::sized(TxBuffer::default(), skew, quantum, chunk, floor, backlog)
        }

        fn sized(b: TxBuffer, skew: f64, quantum: usize, chunk: usize, floor: usize, backlog: f64) -> Self {
            Self {
                r: RateMatch::new(b),
                skew,
                quantum,
                chunk: chunk as f64,
                capture: 1.0,
                floor,
                told: 0,
                cap: b.capacity() as f64,
                queue: b.target as f64 + backlog,
                clock: 0.0,
                frac: 0.0,
                done: 0.0,
                k: 0,
                underruns: 0,
                blocked: 0,
                keyed: false,
                phase: 0,
                sparse: false,
                written: b.target as f64 + backlog,
                played: 0.0,
                update: (0.0, 0.0),
                next: 0.0,
                every: FIRST_UPDATE,
                read: 0.0,
                flagged: 0,
                events: Vec::new(),
                out: Vec::new(),
            }
        }

        fn drain(&mut self, to: f64) {
            let due = (to - self.clock) * (1.0 + self.skew) + self.frac;
            let take = (due / self.chunk).floor() * self.chunk;
            self.frac = due - take;
            self.clock = to;
            if take > self.queue {
                self.underruns += u64::from(self.queue > 0.0);
                self.played += self.queue;
                self.queue = 0.0;
            } else {
                self.played += take;
                self.queue -= take;
            }
        }

        fn reported(&mut self) -> f64 {
            if !self.sparse {
                return self.queue;
            }
            if self.underruns > self.flagged {
                self.flagged = self.underruns;
                (self.read, self.next, self.every) = (self.played, self.clock, FIRST_UPDATE);
            }
            if self.clock >= self.next {
                self.update = (self.played, self.clock);
                self.next = self.clock + self.every;
                self.every = (2.0 * self.every).min(LAST_UPDATE);
            }
            self.read = self.read.max(self.update.0 + self.clock - self.update.1);
            (self.written - self.read).max(0.0)
        }

        fn block(&mut self, stall: f64) {
            self.k += 1;
            let due = (self.k * BLOCK) as f64;
            let start = ((due / self.capture).ceil() * self.capture).max(self.done);
            let mut x = [0.0; BLOCK];
            if self.keyed {
                let hz = 1500.0 / f64::from(RATE);
                x.iter_mut().enumerate().for_each(|(i, v)| *v = 0.5 * (2.0 * PI * hz * (self.phase + i) as f64).sin());
                self.phase = (self.phase + BLOCK) % RATE as usize;
            }
            self.r.process(&x, &mut self.out);
            let mut at = start + stall;
            self.drain(at);
            let over = self.queue + (self.floor + self.out.len()) as f64 - self.cap;
            if over > 0.0 {
                self.blocked += 1;
                at += over / (1.0 + self.skew);
                self.drain(at);
            }
            self.queue += self.out.len() as f64;
            self.written += self.out.len() as f64;
            self.done = at;
            let backlog = (start / self.capture).floor() * self.capture - due;
            let frames = self.reported() as usize / self.quantum * self.quantum + self.floor;
            let busy = Duration::from_secs_f64((at - start) / f64::from(RATE));
            let q = Queue { frames: frames + self.told, floor: self.told, underruns: self.underruns };
            let ev = self.r.steer(Some(q), Some(backlog as usize), busy);
            self.events.extend(ev);
        }
    }

    fn device(skew: f64, secs: usize, quantum: usize) -> (RateMatch, Vec<usize>) {
        let mut d = Sim::new(skew, quantum, 1, 0, 0.0);
        let mut seen = Vec::new();
        for _ in 0..secs * RATE as usize / BLOCK {
            d.block(0.0);
            seen.push(d.queue as usize);
        }
        (d.r, seen)
    }

    #[test]
    fn skewed_playback_settles_at_the_target_fill() {
        for quantum in [1, BLOCK] {
            for skew in [-8e-3, -3e-3, -1e-4, 0.0, 1e-4, 3e-3, 8e-3] {
                let (r, seen) = device(skew, 300, quantum);
                let (lo, hi) = seen.iter().minmax();
                assert!(skew.abs() > 3e-3 || (lo > 0 && hi < 2 * TARGET), "{skew} {quantum}: {lo}-{hi}");
                let (lo, hi) = seen[seen.len() - 3000..].iter().minmax();
                assert!(lo + 48 >= TARGET && hi <= TARGET + quantum + 48, "{skew} {quantum}: {lo}-{hi}");
                assert!((r.ppm() - skew * 1e6).abs() < 200.0, "{skew} {quantum}: {}", r.ppm());
            }
        }
    }

    const SECS: usize = 450;
    const STALLS: std::ops::Range<usize> = 15000..30000;

    fn stalled(skew: f64, quantum: usize, chunk: usize, every: usize) -> (Vec<f64>, Vec<usize>, u64) {
        stalled_on(skew, quantum, chunk, every, false)
    }

    fn stalled_on(skew: f64, quantum: usize, chunk: usize, every: usize, sparse: bool) -> (Vec<f64>, Vec<usize>, u64) {
        let mut d = Sim::new(skew, quantum, chunk, 0, 0.0);
        d.sparse = sparse;
        let (mut skews, mut seen) = (Vec::new(), Vec::new());
        for k in 0..SECS * 100 {
            let n = k / every.max(1);
            let stall = every > 0 && STALLS.contains(&k) && k % every == 0;
            d.block(if stall { [30.0, 50.0, 80.0][n % 3] * 48.0 } else { 0.0 });
            skews.push(-d.r.ctl.integral * 1e6);
            seen.push(d.queue as usize);
        }
        (skews, seen, d.underruns)
    }

    #[test]
    fn stalls_do_not_move_the_integrator() {
        for (quantum, chunk) in [(1, 1), (BLOCK, 1), (1, 1024)] {
            for skew in [-3e-3, 0.0, 3e-3] {
                let (base, _, _) = stalled(skew, quantum, chunk, 0);
                let (skews, seen, underruns) = stalled(skew, quantum, chunk, 300);
                let wander = base.iter().zip(&skews).skip(STALLS.start).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
                let (lo, hi) = seen[seen.len() - 3000..].iter().minmax();
                let (at, slack) = (format!("{skew} {quantum} {chunk}"), chunk / 2 + 48);
                assert!(underruns > 10, "{at}: {underruns}");
                assert!(wander < 200.0, "{at}: {wander}");
                assert!((skews[skews.len() - 1] - skew * 1e6).abs() < 100.0, "{at}: {}", skews[skews.len() - 1]);
                assert!(lo + slack >= TARGET && hi <= TARGET + quantum + slack, "{at}: {lo}-{hi}");
            }
        }
    }

    #[test]
    fn a_server_latency_floor_is_aligned_without_a_kick() {
        let cap = P_CAP * 1e6;
        for skew in [-4.166e-3, 0.0, 4.166e-3] {
            let mut d = Sim::new(skew, 1, 1024, 1300, 1000.0);
            let (mut lo, mut hi, mut least) = (f64::MAX, f64::MIN, usize::MAX);
            for k in 0..300 * 100 {
                d.block(0.0);
                lo = lo.min(d.r.ppm() - skew.min(0.0) * 1e6);
                hi = hi.max(d.r.ppm() - skew.max(0.0) * 1e6);
                least = least.min(if k > 100 { d.queue as usize } else { usize::MAX });
            }
            assert_eq!(d.blocked, 0, "{skew}");
            assert!(lo > -1.5 * cap && hi < 1.5 * cap, "{skew}: {lo:.0} {hi:.0}");
            assert!(skew != 0.0 || (lo > -cap && hi < cap && d.underruns == 0), "{skew}: {lo:.0} {hi:.0}");
            assert!(skew != 0.0 || least > TARGET, "{skew}: {least}");
            assert!((d.r.ppm() - skew * 1e6).abs() < 100.0, "{skew}: {}", d.r.ppm());
        }
    }

    #[test]
    fn keying_during_startup_holds_the_alignment_until_silence() {
        for keyed in [false, true] {
            let mut d = Sim::new(0.0, 1, 1, 1300, 1000.0);
            d.keyed = keyed;
            for _ in 0..200 {
                d.block(0.0);
            }
            d.keyed = false;
            let mut seen = Vec::new();
            for _ in 0..100 * 100 {
                d.block(0.0);
                seen.push(d.queue as usize);
            }
            let Event::Aligned { start, hold, .. } = d.events[0] else { panic!("{keyed}: {:?}", d.events) };
            let (lo, hi) = seen[100..].iter().minmax();
            let held = hold as usize - 1300;
            assert!(hold == 2.0 * TARGET as f64 && start > hold + 300.0, "{keyed}: {start} {hold}");
            assert!(lo + 48 >= held && hi <= held + 48, "{keyed}: {lo}-{hi} {held}");
            assert!(!d.events.iter().any(|e| matches!(e, Event::Realign(_))), "{keyed}: {:?}", d.events);
            assert!(matches!(d.r.ctl.state, State::Run(Owed::Nothing)), "{keyed}");
        }
    }

    #[test]
    fn a_moved_latency_floor_is_reported_again() {
        for (skew, chunk) in [(0.0, 1), (-3e-3, 1024), (3e-3, 1)] {
            let mut d = Sim::new(skew, 1, chunk, 0, 0.0);
            d.told = 1300;
            for _ in 0..60 * 100 {
                d.block(0.0);
            }
            let (seen, set) = (d.events.len(), d.r.ctl.set.round() as usize);
            d.told = 300;
            for _ in 0..10 * 100 {
                d.block(0.0);
            }
            let latency: Vec<usize> = d.events[seen..]
                .iter()
                .filter_map(|e| if let Event::Latency(n) = e { Some(*n) } else { None })
                .collect();
            let at = format!("{skew} {chunk}");
            assert_eq!(latency.len(), 1, "{at}: {:?}", d.events);
            assert!(latency[0].abs_diff(set + 300 + Interp::DELAY) <= 4, "{at}: {latency:?} {set}");
            assert_eq!(d.r.latency(), latency[0], "{at}");
        }
    }

    #[test]
    fn a_stall_without_a_queue_reading_is_still_seen() {
        let mut d = Sim::new(0.0, 1, 1, 0, 0.0);
        for _ in 0..10 * 100 {
            d.block(0.0);
        }
        assert!(matches!(d.r.ctl.state, State::Run(Owed::Nothing)));
        d.r.steer(None, None, Duration::from_millis(50));
        assert!(matches!(d.r.ctl.state, State::Measure(_, Then::Check)));
        assert_eq!(d.r.ctl.calm, REARM_BLOCKS);
    }

    #[test]
    fn stalls_follow_busy_time_and_armed_underruns() {
        let q = |underruns| Some(Queue { frames: TARGET, floor: 0, underruns });
        let (idle, slow) = (Duration::ZERO, SLOW + Duration::from_millis(1));
        let mut s = Stalls::default();
        assert_eq!(s.check(None, STALL, 0.0), None);
        assert_eq!(s.check(None, STALL + Duration::from_millis(1), 0.0), Some(Then::Check));
        assert!(s.check(None, idle, 0.0).is_none() && s.slow == SKIP_BLOCKS - 1);
        let mut s = Stalls::default();
        assert!(s.check(q(1), idle, FAR_FRAMES).is_none() && s.rearm == 0 && s.underruns == 1);
        assert!(s.check(None, idle, 0.0).is_none() && s.underruns == 1);
        assert!(s.check(q(2), idle, 0.0) == Some(Then::Trim) && s.rearm == REARM_BLOCKS);
        assert!(s.check(q(3), idle, 0.0).is_none() && s.rearm == REARM_BLOCKS - 1);
        assert!(s.check(q(3), slow, 0.0).is_none() && s.slow == SKIP_BLOCKS);
        assert!(s.check(q(4), idle, 0.0) == Some(Then::Check) && s.rearm == REARM_BLOCKS);
        for _ in 0..REARM_BLOCKS + SKIP_BLOCKS {
            assert_eq!(s.check(q(4), idle, 0.0), None);
        }
        assert_eq!(s.check(q(5), idle, 0.0), Some(Then::Trim));
    }

    #[test]
    fn the_settled_latency_counts_the_hold_the_server_floor_and_the_filter() {
        for (floor, told, backlog, held) in
            [(0, 0, 0.0, TARGET), (1300, 0, 0.0, 3220), (0, 700, 0.0, TARGET), (1300, 700, 1000.0, 2 * TARGET)]
        {
            let mut d = Sim::new(0.0, 1, 1, floor, backlog);
            d.told = told;
            for _ in 0..300 {
                d.block(0.0);
            }
            let Event::Aligned { hold, .. } = d.events[0] else { panic!("{floor} {told}: {:?}", d.events) };
            let latency: Vec<usize> =
                d.events.iter().filter_map(|e| if let Event::Latency(n) = e { Some(*n) } else { None }).collect();
            assert!((hold - held as f64).abs() < 10.0, "{floor} {told}: {hold}");
            assert_eq!(latency, [hold.round() as usize + told + Interp::DELAY], "{floor} {told}: {:?}", d.events);
        }
    }

    #[test]
    fn a_stall_while_aligning_does_not_raise_the_hold() {
        for (chunk, floor, backlog) in
            [(1, 0, 0.0), (1024, 0, 0.0), (1, 1300, 0.0), (1, 1300, 1000.0), (1024, 1300, 1000.0)]
        {
            for skew in [-3e-3, 0.0, 3e-3] {
                let run = |stall: f64, at: usize| {
                    let mut d = Sim::new(skew, 1, chunk, floor, backlog);
                    let mut seen = Vec::new();
                    for k in 0..120 * 100 {
                        d.block(if k == at { stall * 48.0 } else { 0.0 });
                        seen.push(d.queue as usize);
                    }
                    let hold = d
                        .events
                        .iter()
                        .find_map(|e| if let Event::Aligned { hold, .. } = e { Some(*hold) } else { None });
                    let told =
                        d.events.iter().rev().find_map(|e| if let Event::Latency(n) = e { Some(*n) } else { None });
                    assert_eq!(told, Some(d.r.latency()), "{skew} {stall} {at}: {:?}", d.events);
                    (hold.unwrap(), d.r.latency(), d.r.ctl.set as usize - floor, seen.split_off(seen.len() - 1000))
                };
                let (want, latency, _, _) = run(0.0, 0);
                let (edge, drift) =
                    (chunk / 2 + 48, (skew.abs() * ((SKIP_BLOCKS + SETTLE_BLOCKS) * BLOCK) as f64) as usize);
                for stall in [50.0, 150.0] {
                    for at in [20, 35, 50] {
                        let (hold, told, held, seen) = run(stall, at);
                        let (lo, hi) = seen.iter().minmax();
                        let at = format!("{chunk} {floor} {backlog} {skew} {stall} {at}");
                        assert!((hold - want).abs() <= (edge + drift) as f64, "{at}: {hold} {want}");
                        assert!(told.abs_diff(latency) <= edge + drift, "{at}: {told} {latency}");
                        assert!(lo + edge >= held && hi <= held + edge, "{at}: {lo}-{hi} {held}");
                    }
                }
            }
        }
    }

    #[test]
    fn every_buffer_size_settles() {
        for ms in [MIN_MS, DEFAULT_MS, MAX_MS] {
            for chunk in [1, 1024] {
                for skew in [-3e-3, 0.0, 3e-3] {
                    let b = TxBuffer::ms(ms);
                    let mut d = Sim::sized(b, skew, 1, chunk, 1300, 500.0);
                    let (mut lo, mut hi) = (usize::MAX, 0);
                    for k in 0..300 * 100 {
                        d.block(0.0);
                        if k >= 27000 {
                            lo = lo.min(d.queue as usize);
                            hi = hi.max(d.queue as usize);
                        }
                    }
                    let (at, slack, held) =
                        (format!("{ms} {chunk} {skew}"), chunk / 2 + 48, d.r.ctl.set as usize - 1300);
                    assert!(held >= b.target && lo + slack >= held && hi <= held + slack, "{at}: {lo}-{hi} {held}");
                    assert!((d.r.ppm() - skew * 1e6).abs() < 100.0, "{at}: {}", d.r.ppm());
                    assert!(d.blocked == 0 && (skew != 0.0 || d.underruns == 0), "{at}: {} {}", d.blocked, d.underruns);
                }
            }
        }
    }

    #[test]
    fn keyed_audio_after_a_stall_is_not_warped() {
        for chunk in [1, 1024] {
            for skew in [-3e-3, 0.0, 3e-3] {
                for stall in [50.0, 150.0, 300.0] {
                    let mut d = Sim::new(skew, 1, chunk, 0, 0.0);
                    for _ in 0..200 * 100 {
                        d.block(0.0);
                    }
                    d.keyed = true;
                    d.block(stall * 48.0);
                    let mut worst = 0.0f64;
                    for _ in 0..30 * 100 {
                        d.block(0.0);
                        worst = worst.max((d.r.ppm() - skew * 1e6).abs());
                    }
                    d.keyed = false;
                    let mut seen = Vec::new();
                    for _ in 0..60 * 100 {
                        d.block(0.0);
                        seen.push(d.queue as usize);
                    }
                    let (lo, hi) = seen[seen.len() - 1000..].iter().minmax();
                    let (at, slack) = (format!("{chunk} {skew} {stall}"), chunk / 2 + 48);
                    assert!(worst < KEYED_CAP * 1e6 + 50.0, "{at}: {worst:.0}");
                    assert!(lo + slack >= TARGET && hi <= TARGET + slack, "{at}: {lo}-{hi}");
                }
            }
        }
    }

    #[test]
    fn sparse_timing_updates_settle() {
        for (quantum, chunk) in [(1, 1), (BLOCK, 1), (1, 1024)] {
            for skew in [-3e-3, 0.0, 3e-3] {
                let mut d = Sim::new(skew, quantum, chunk, 0, 0.0);
                d.sparse = true;
                let (mut seen, mut early) = (Vec::new(), 0);
                for k in 0..300 * 100 {
                    d.block(0.0);
                    seen.push(d.queue as usize);
                    early = if k < 3000 { d.underruns } else { early };
                }
                assert_eq!(d.underruns, early, "{skew} {quantum} {chunk}");
                let (lo, hi) = seen[seen.len() - 3000..].iter().minmax();
                let (at, bias) = (format!("{skew} {quantum} {chunk}"), (skew * LAST_UPDATE / 2.0).abs() as usize);
                let skewed = (-d.r.ctl.integral - skew).abs() * 1e6;
                if chunk == 1 {
                    assert!(lo + bias + 48 >= TARGET && hi <= TARGET + bias + quantum + 48, "{at}: {lo}-{hi}");
                    assert!((d.r.ppm() - skew * 1e6).abs() < 200.0 && skewed < 50.0, "{at}: {} {skewed}", d.r.ppm());
                } else {
                    assert!(lo > TARGET / 2 && hi < 2 * TARGET && skewed < 200.0, "{at}: {lo}-{hi} {skewed}");
                }
            }
        }
    }

    #[test]
    fn stalls_under_sparse_timing_updates_do_not_move_the_integrator() {
        for (quantum, chunk) in [(1, 1), (BLOCK, 1), (1, 1024)] {
            for skew in [-3e-3, 0.0, 3e-3] {
                let (base, _, _) = stalled_on(skew, quantum, chunk, 0, true);
                let (skews, _, underruns) = stalled_on(skew, quantum, chunk, 300, true);
                let off = |v: &[f64]| v[STALLS.start..].iter().map(|a| (a - skew * 1e6).abs()).fold(0.0, f64::max);
                let at = format!("{skew} {quantum} {chunk}");
                assert!(underruns > 10, "{at}: {underruns}");
                assert!(off(&skews) < off(&base) + 150.0, "{at}: {} {}", off(&skews), off(&base));
                assert!((skews[skews.len() - 1] - skew * 1e6).abs() < 100.0, "{at}: {}", skews[skews.len() - 1]);
            }
        }
    }

    #[test]
    fn a_stall_in_silence_is_realigned_within_three_seconds() {
        for chunk in [1, 1024] {
            for skew in [-3e-3, 0.0, 3e-3] {
                for stall in [80.0, 150.0, 300.0] {
                    let mut d = Sim::new(skew, 1, chunk, 0, 0.0);
                    for _ in 0..200 * 100 {
                        d.block(0.0);
                    }
                    let base = d.r.ctl.integral;
                    d.block(stall * 48.0);
                    let mut worst = 0.0f64;
                    let mut seen = Vec::new();
                    for _ in 0..10 * 100 {
                        d.block(0.0);
                        seen.push(d.queue as usize);
                        worst = worst.max((d.r.ctl.integral - base).abs() * 1e6);
                    }
                    let (lo, hi) = seen[300..].iter().minmax();
                    let (at, slack) = (format!("{chunk} {skew} {stall}"), chunk / 2 + FAR_FRAMES as usize);
                    assert!(lo + slack >= TARGET && hi <= TARGET + slack, "{at}: {lo}-{hi}");
                    assert!(worst < 5.0, "{at}: {worst}");
                    assert!(d.events.iter().any(|e| matches!(e, Event::Realign(_))), "{at}");
                }
            }
        }
    }

    #[test]
    fn a_long_stall_in_silence_counts_the_capture_backlog() {
        for (chunk, capture) in [(1, 1), (1024, 1), (1, 1024)] {
            for skew in [-3e-3, 0.0, 3e-3] {
                let mut d = Sim::new(skew, 1, chunk, 0, 0.0);
                d.capture = capture as f64;
                for _ in 0..200 * 100 {
                    d.block(0.0);
                }
                let seen = d.events.len();
                d.block(300.0 * 48.0);
                let mut queue = Vec::new();
                for _ in 0..10 * 100 {
                    d.block(0.0);
                    queue.push(d.queue as usize);
                }
                let realigns = d.events[seen..].iter().filter(|e| matches!(e, Event::Realign(_))).count();
                let (lo, hi) = queue[200..].iter().minmax();
                let (at, slack) = (format!("{chunk} {capture} {skew}"), (chunk + capture) / 2 + FAR_FRAMES as usize);
                assert_eq!(realigns, 1, "{at}: {:?}", &d.events[seen..]);
                assert!(lo + slack >= TARGET && hi <= TARGET + slack, "{at}: {lo}-{hi}");
            }
        }
    }

    #[test]
    fn equal_capture_and_playback_quanta_settle_without_an_underrun_loop() {
        for (skew, learned) in [(-3e-3, true), (0.0, true), (1e-4, false), (1e-3, false), (3e-3, true)] {
            let mut d = Sim::new(skew, 1, 1024, 0, 0.0);
            d.capture = 1024.0;
            let mut early = 0;
            for k in 0..300 * 100 {
                d.block(0.0);
                early = if k < 60 * 100 { d.underruns } else { early };
            }
            let at = format!("{skew}");
            let raised: Vec<f64> = d
                .events
                .iter()
                .filter_map(|e| if let Event::Headroom { hold, .. } = e { Some(*hold) } else { None })
                .collect();
            let last = d.events.iter().rev().find_map(|e| if let Event::Latency(n) = e { Some(*n) } else { None });
            assert!(!d.events.iter().any(|e| matches!(e, Event::Realign(_))), "{at}: {:?}", d.events);
            assert!(d.underruns <= 2 && (d.underruns == early || skew == 1e-4), "{at}: {} {early}", d.underruns);
            assert!(raised.len() == usize::from(d.underruns > 0), "{at}: {:?}", d.events);
            assert!(raised.iter().all(|&h| h > TARGET as f64 + 512.0 && h <= 3.0 * TARGET as f64), "{at}: {raised:?}");
            assert_eq!(last, Some(d.r.latency()), "{at}");
            assert!(d.r.latency().abs_diff(d.r.ctl.set as usize + Interp::DELAY) <= 4, "{at}: {}", d.r.latency());
            assert!(!learned || (-d.r.ctl.integral - skew).abs() * 1e6 < 100.0, "{at}: {}", d.r.ctl.integral);
        }
    }

    #[test]
    fn a_playback_queue_that_reads_zero_runs_open_loop() {
        for skew in [-3e-3, 0.0, 3e-3] {
            let mut d = Sim::new(skew, usize::MAX, 1, 0, 0.0);
            for k in 0..120 * 100 {
                d.keyed = (3000..6000).contains(&k);
                d.block(if k % 2000 == 1000 { 50.0 * 48.0 } else { 0.0 });
                assert_eq!(d.r.ppm(), 0.0, "{skew} {k}");
            }
            assert_eq!(d.events, [Event::NoQueue], "{skew}");
            assert!(matches!(d.r.ctl.state, State::Open), "{skew}");
            assert_eq!(d.r.latency(), TARGET + Interp::DELAY, "{skew}");
        }
    }

    trait MinMax {
        fn minmax(self) -> (usize, usize);
    }

    impl<'a, I: Iterator<Item = &'a usize>> MinMax for I {
        fn minmax(self) -> (usize, usize) {
            self.fold((usize::MAX, 0), |(lo, hi), &q| (lo.min(q), hi.max(q)))
        }
    }
}
