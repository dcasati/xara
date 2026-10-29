use super::scheduler::{Kind, TxFront};
use super::{Arq, Common, MIN_GAP, Modem, Options, Report, secs};
use std::collections::VecDeque;
use xara_arq::fm::adapt::SNR_OFFSET_DB;
use xara_arq::fm::broadcast::{self, Received};
use xara_arq::fm::digi::{DigiListen, DigiOutput, DigiTx};
use xara_arq::host::HostEvent;
use xara_arq::rng::XorShift;
use xara_arq::time::Time;
use xara_arq::{fm, kiss};
use xara_host::command::Bw;
use xara_phy::busy::Busy;
use xara_phy::dsp::quantise;
use xara_phy::fm::ctrl::{Codebook, Side, frame_audio};
use xara_phy::fm::frame::parse_data;
use xara_phy::fm::level::{DataLevel, FmBandwidth, Record};
use xara_phy::fm::rx::FS;
use xara_phy::fm::rx::receiver::{Event, Receiver, Transmission};
use xara_phy::fm::tx::{IDLE_BEACON_LEAD_IN, Modulator, NFFT, TxOptions, tune_block};
use xara_ui::telemetry::Link;

const SILENT_TAIL: usize = 45 * 1152;
const WAVEOUT_QUEUE: usize = 200 * 256;
pub const DATA_OVERHANG: usize = SILENT_TAIL - WAVEOUT_QUEUE;
const SEED_MIX: u64 = 0x9E37_79B9_7F4A_7C15;
pub const FRAGMENT_GAP: usize = 480;

pub struct Station {
    c: Common<fm::Session>,
    rx: Receiver,
    rx_base: Time,
    modm: Modulator,
    cb: Option<Codebook>,
    digi: fm::Digi,
    digi_cb: [Option<Codebook>; 2],
    rng: XorShift,
    unproto: VecDeque<(usize, Vec<Vec<u8>>)>,
    reasm: broadcast::Reassembler,
    kiss_rx: bool,
    ignore_dcd: bool,
}

fn bitrate(bw: FmBandwidth, record: Record, tx: bool) -> Report {
    let shown = record.data_level().map_or(1, DataLevel::get);
    Report::Bitrate { level: shown, bps: bw.level(record).net_bps().round_ties_even() as u32, tx }
}

impl Station {
    pub fn new(cfg: fm::Config, opts: Options) -> Self {
        xara_phy::fm::code::warm();
        let bw = cfg.bw;
        let mut s = Self {
            rng: XorShift::new(cfg.seed.wrapping_mul(SEED_MIX)),
            c: Common::new(fm::Session::new(cfg.clone()), cfg, opts, Busy::fm()),
            rx: Receiver::new(bw, 0, &[Record::Connect], 1),
            rx_base: 0,
            modm: Modulator::new(bw),
            cb: None,
            digi: fm::Digi::new("", bw),
            digi_cb: [None, None],
            unproto: VecDeque::new(),
            reasm: broadcast::Reassembler::default(),
            kiss_rx: false,
            ignore_dcd: false,
        };
        s.drain();
        s
    }

    pub fn state(&self) -> fm::State {
        self.c.session.state()
    }

    pub fn session(&self) -> &fm::Session {
        &self.c.session
    }

    pub fn bandwidth(&self) -> FmBandwidth {
        self.c.session.bandwidth()
    }

    pub fn set_digipeater(&mut self, call: &str) {
        self.digi.set_call(call);
        self.refresh_listen();
    }

    pub fn digipeating(&self) -> bool {
        self.digi.active()
    }

    fn send_unproto(&mut self) {
        let clear = self.ignore_dcd || !self.c.busy.is_on();
        if self.c.session.state() != fm::State::Listen
            || self.digi.active()
            || !self.c.tx.is_empty()
            || self.c.tx.ptt()
            || !clear
        {
            return;
        }
        let Some((_, group)) = self.unproto.pop_front() else { return };
        let bw = self.c.cfg.bw;
        if self.modm.bw != bw {
            self.modm = Modulator::new(bw);
        }
        let opt = TxOptions { drive_db: self.c.opts.drive_db, pre_signal: false };
        let n = group.len();
        for (i, f) in group.iter().enumerate() {
            let rng = &mut self.rng;
            let mut r = |n: u32| rng.below(n);
            let x = self.modm.transmission(Record::Broadcast, 0, &[f], IDLE_BEACON_LEAD_IN, opt, &mut r);
            let tail = if i + 1 == n { DATA_OVERHANG } else { FRAGMENT_GAP };
            let mut keyed = vec![
                Report::Host(HostEvent::Pending),
                bitrate(bw, Record::Broadcast, true),
                Report::Trace(format!("tx rec16 unproto {}/{n}", i + 1)),
            ];
            if i == 0 {
                self.c.reports.extend(keyed.drain(..));
            }
            self.c.tx.push(quantised(x), tail, self.c.now, keyed, Kind::Fragment { last: i + 1 == n });
        }
    }

    fn linked(&self) -> bool {
        self.c.listen.as_ref().is_some_and(|l| l.linked)
    }

    fn on_event(&mut self, ev: Event) {
        let now = self.c.now;
        let Some(t_end) = rx_end(&ev).map(|t| self.rx_base + t) else {
            self.drain();
            return;
        };
        self.c.tx.hold(t_end + MIN_GAP);
        match ev {
            Event::Ctrl(c) => {
                let d = &c.det;
                let digi = self.digi.active();
                self.c.reports.push_back(Report::Trace(format!(
                    "rx c{} hits {} lead {} end {:.3} {}",
                    d.code,
                    d.hits,
                    d.lead_row,
                    secs(t_end),
                    if digi { format!("Digi side {}", c.side) } else { format!("{:?}", self.c.session.state()) }
                )));
                if digi {
                    self.digi.on_ctrl(now, d.code, d.lead_row, c.side);
                    self.drain();
                    return;
                }
                if c.side != Side::Source {
                    return;
                }
                self.c.session.on_ctrl(now, fm::CtrlIn { code: d.code, lead_row: d.lead_row, t_end });
            }
            Event::Tx(t) if t.record == Record::Broadcast => {
                let Some(f) = t.frames.first().filter(|f| f.ok) else {
                    self.reasm.reset();
                    return;
                };
                self.c.reports.push_back(Report::Trace(format!(
                    "rx rec16 end {:.3} {}",
                    secs(t_end),
                    f.bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
                )));
                self.c.reports.push_back(Report::Host(HostEvent::Sn(f.snr_db + SNR_OFFSET_DB)));
                match self.reasm.push(&f.bytes) {
                    Some(Received::Cq(c)) if self.c.cfg.host.chat => {
                        self.c.reports.push_back(Report::Line(c.host_line()))
                    }
                    Some(Received::Kiss(cmd, data)) => self.c.reports.push_back(Report::Kiss(cmd, data)),
                    _ => {}
                }
            }
            Event::Tx(t)
                if self.digi.active()
                    || (t.record == Record::Connect && self.c.session.state() == fm::State::Listen) =>
            {
                let ok: String = t.frames.iter().map(|f| if f.ok { '+' } else { '-' }).collect();
                let first = t.frames.iter().find(|f| f.ok);
                let took =
                    t.record == Record::Connect && first.is_some_and(|f| self.digi.on_record0(now, &f.bytes, f.snr_db));
                if took || self.digi.active() {
                    self.c.reports.push_back(Report::Trace(format!(
                        "rx rec{} [{ok}] end {:.3} Digi",
                        t.record,
                        secs(t_end)
                    )));
                    if !took {
                        self.digi.on_sync(now);
                        for f in &t.frames {
                            self.digi.on_data(now, t.record, f.ok && !f.missing, &f.bytes, f.snr_db);
                        }
                    }
                } else {
                    self.on_session_data(now, &t, t_end);
                }
            }
            Event::Tx(t) => self.on_session_data(now, &t, t_end),
            Event::Frame(_) => {}
        }
        self.drain();
    }

    fn on_session_data(&mut self, now: Time, t: &Transmission, t_end: Time) {
        let ok: String = t.frames.iter().map(|f| if f.ok { '+' } else { '-' }).collect();
        let lvl = t
            .frames
            .last()
            .filter(|f| f.ok)
            .map_or(String::new(), |f| level_note(t.record, &f.bytes, self.c.session.has_vias()));
        self.c.reports.push_back(Report::Trace(format!(
            "rx rec{} [{ok}]{lvl} end {:.3} {:?}",
            t.record,
            secs(t_end),
            self.c.session.state()
        )));
        self.c.session.on_data(now, data_in(t, t_end));
    }

    fn apply_digi(&mut self, l: DigiListen) {
        self.c.reports.push_back(Report::Trace(format!("digi expect {:?} data key {:08x}", l.records, l.data_key)));
        self.rx.set_bandwidth(l.bw);
        self.rx.set_key(l.data_key);
        self.rx.expect(&l.records, Some(1));
        for (s, c) in Side::BOTH.into_iter().zip(l.ctrl) {
            let cb = Codebook::new(c.key_d, c.key_u);
            self.rx.set_ctrl(s, Some(cb.clone()), &c.codes);
            self.digi_cb[s.idx()] = Some(cb);
        }
    }

    fn render_digi(&mut self, t: &DigiTx) -> (Vec<f64>, usize) {
        let d = self.c.opts.drive_db;
        let (x, tail) = match t {
            DigiTx::Ctrl { code, lead, side } => {
                let cb = self.digi_cb[side.idx()].as_ref().expect("digipeater codebook");
                (frame_audio(cb, *code, *lead, d), 0)
            }
            DigiTx::Data { record, frame, lead, side } => {
                let bw = if *record == Record::Connect { self.c.cfg.bw } else { self.digi.bandwidth() };
                if self.modm.bw != bw {
                    self.modm = Modulator::new(bw);
                }
                let key = self.digi.key(*side);
                let opt = TxOptions { drive_db: d, pre_signal: false };
                let rng = &mut self.rng;
                let mut r = |n: u32| rng.below(n);
                (self.modm.transmission(*record, key, &[frame.as_slice()], *lead, opt, &mut r), DATA_OVERHANG)
            }
        };
        (quantised(x), tail)
    }

    fn host(&mut self, h: HostEvent) {
        let r = match h {
            HostEvent::Unencrypted => Report::UnencryptedLink,
            HostEvent::RxFrame(r) => bitrate(self.c.session.bandwidth(), r, false),
            HostEvent::Log(s) => Report::Trace(s),
            h => Report::Host(h),
        };
        self.c.reports.push_back(r);
    }

    pub fn refresh_listen(&mut self) {
        if let Some(l) = self.c.listen.clone() {
            self.apply(l);
        }
    }

    fn render(&mut self, t: &fm::Tx) -> (Vec<f64>, usize, Vec<Report>) {
        let d = self.c.opts.drive_db;
        let mut keyed = Vec::new();
        let (x, tail) = match t {
            fm::Tx::Ctrl { code, lead } => {
                let cb = self.cb.as_ref().expect("codebook before the first control frame");
                (frame_audio(cb, *code, *lead, d), 0)
            }
            fm::Tx::Data { record, frames, lead } => {
                let bw = if *record == Record::Connect { self.c.cfg.bw } else { self.c.session.bandwidth() };
                if self.modm.bw != bw {
                    self.modm = Modulator::new(bw);
                }
                self.c.reports.push_back(bitrate(bw, *record, true));
                keyed.extend((1..frames.len()).map(|_| bitrate(bw, *record, true)));
                let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
                let key = self.c.session.key();
                let opt = TxOptions { drive_db: d, pre_signal: false };
                let rng = &mut self.rng;
                let mut r = |n: u32| rng.below(n);
                (self.modm.transmission(*record, key, &refs, *lead, opt, &mut r), DATA_OVERHANG)
            }
        };
        (quantised(x), tail, keyed)
    }
}

impl TxFront for Station {
    fn burst_done(&mut self, kind: Kind, start: Time, end: Time) {
        let heard = end + self.c.tx.latency();
        self.rx.mute(start - self.rx_base, heard - self.rx_base);
        self.rx.mark_tx_end(heard - self.rx_base);
        let mark = self.c.reports.len();
        match kind {
            Kind::Digi => {
                self.digi.on_tx_done();
                self.drain_at(end);
            }
            Kind::Session => {
                self.c.session.on_tx_done(heard);
                self.drain_at(end);
            }
            Kind::Announce | Kind::Fragment { .. } => {}
        }
        self.c.tx.release(end, &mut self.c.reports, mark);
        if kind == (Kind::Fragment { last: true }) {
            self.c.reports.push_back(Report::Host(HostEvent::CancelPending));
            self.refresh_listen();
        }
    }
}

impl Modem for Station {
    type Config = fm::Config;
    type Session = fm::Session;
    const VERSION: &'static str = "VERSION VARA FM v4.4.0";
    const VIAS: bool = true;
    const KISS_RX_FOLLOWS_CLIENTS: bool = false;
    const CWID_GATEWAY: bool = false;

    fn common(&self) -> &Common<fm::Session> {
        &self.c
    }

    fn common_mut(&mut self) -> &mut Common<fm::Session> {
        &mut self.c
    }

    fn apply(&mut self, l: fm::Listen) {
        if self.digi.active() {
            self.c.listen = Some(l);
            return;
        }
        if self.c.listen.as_ref().is_none_or(|o| o.records != l.records || o.nframes != l.nframes) {
            self.c.reports.push_back(Report::Trace(format!("expect {:?} x{}", l.records, l.nframes)));
        }
        self.rx.set_bandwidth(l.bw);
        self.rx.set_key(l.key);
        let answering = self.c.listen_on || l.linked || self.c.session.state() != fm::State::Listen;
        let mut records: Vec<Record> = if answering { l.records.clone() } else { Vec::new() };
        if !self.digi.call().is_empty()
            && self.c.session.state() == fm::State::Listen
            && !records.contains(&Record::Connect)
        {
            records.push(Record::Connect);
        }
        if self.kiss_rx && self.c.session.state() == fm::State::Listen && self.c.tx.is_empty() {
            records.push(Record::Broadcast);
        }
        self.rx.expect(&records, Some(l.nframes));
        self.cb = l.ctrl.map(|(d, u)| Codebook::new(d, u));
        match &self.cb {
            Some(cb) if !l.codes.is_empty() => self.rx.set_ctrl(Side::Source, Some(cb.clone()), &l.codes),
            _ => self.rx.set_ctrl(Side::Source, None, &[]),
        }
        self.c.listen = Some(l);
    }

    fn drain_at(&mut self, now: Time) {
        if !self.c.pending.is_empty() && self.c.session.state() != fm::State::Listen {
            let d = std::mem::take(&mut self.c.pending);
            self.c.session.send(now, &d);
        }
        let mut txs = Vec::new();
        while let Some(o) = self.c.session.pop() {
            match o {
                fm::Output::Tx(t) => txs.push(t),
                fm::Output::Listen(l) => self.apply(l),
                fm::Output::Host(h) => self.host(h),
            }
        }
        for t in txs {
            let what = match &t {
                fm::Tx::Ctrl { code, lead } => format!("tx c{code} lead {lead}"),
                fm::Tx::Data { record, frames, lead } => format!(
                    "tx rec{record}x{}{} lead {lead}",
                    frames.len(),
                    frames.last().map_or(String::new(), |f| level_note(*record, f, self.c.session.has_vias()))
                ),
            };
            self.c.reports.push_back(Report::Trace(format!("{what} {:?}", self.c.session.state())));
            self.c.tx.stop_tune();
            let earliest = self.c.tx.next_start(now);
            let (samples, tail, keyed) = self.render(&t);
            self.c.tx.push(samples, tail, earliest, keyed, Kind::Session);
        }
        while let Some(o) = self.digi.pop() {
            match o {
                DigiOutput::Tx(t) => {
                    let what = match &t {
                        DigiTx::Ctrl { code, lead, side } => format!("tx c{code} lead {lead} Digi side {side}"),
                        DigiTx::Data { record, lead, side, .. } => {
                            format!("tx rec{record}x1 lead {lead} Digi side {side}")
                        }
                    };
                    self.c.reports.push_back(Report::Trace(what));
                    self.c.tx.stop_tune();
                    let earliest = self.c.tx.next_start(now);
                    let (samples, tail) = self.render_digi(&t);
                    self.c.tx.push(samples, tail, earliest, Vec::new(), Kind::Digi);
                }
                DigiOutput::Listen(l) => self.apply_digi(l),
                DigiOutput::Host(h) => self.host(h),
                DigiOutput::Done => {
                    self.digi_cb = [None, None];
                    self.rx.set_ctrl(Side::Target, None, &[]);
                    self.refresh_listen();
                }
            }
        }
    }

    fn tune_tone(drive_db: f64) -> Vec<f64> {
        tune_block(drive_db).iter().map(|&v| quantise(v)).collect()
    }

    fn idle(&self) -> bool {
        self.c.session.state() == fm::State::Listen
            && !self.digi.active()
            && self.c.tx.is_empty()
            && self.unproto.is_empty()
    }

    fn link(&self) -> Link {
        let s = &self.c.session;
        Link {
            state: if self.digi.active() { "Digipeating".into() } else { format!("{:?}", s.state()) },
            level: fm::adapt::as_level(s.level()).get(),
            peer: s.peer().to_string(),
            bw: format!("{:?}", s.bandwidth()).to_ascii_lowercase(),
            passband: passband(s.bandwidth()),
            attempts: s.attempts(),
            afc_hz: None,
        }
    }

    fn reconfigure(&mut self, cfg: fm::Config) -> bool {
        if !self.idle() {
            return false;
        }
        self.c.cfg = cfg;
        self.digi.set_bandwidth(self.c.cfg.bw);
        self.c.session = fm::Session::new(self.c.cfg.clone());
        self.drain();
        true
    }

    fn process(&mut self, input: &[f64], output: &mut [f64]) {
        let (t0, n) = (self.c.now, input.len());
        let fed = self.key_block(t0, input, output);
        self.c.now = t0 + n as Time;
        let forced = self.c.tx.ptt()
            || !self.c.tx.is_empty()
            || self.c.session.state() != fm::State::Listen
            || self.digi.active();
        for e in self.c.busy.feed(input, forced) {
            self.c.reports.push_back(Report::Busy(e));
        }
        for ev in self.rx.feed(&fed) {
            self.on_event(ev);
        }
        if self.c.session.next_deadline().is_some_and(|d| d <= self.c.now) {
            self.c.session.poll(self.c.now);
        }
        if self.digi.next_deadline().is_some_and(|d| d <= self.c.now) {
            self.digi.poll(self.c.now);
        }
        self.drain();
        self.send_unproto();
    }

    fn connect(&mut self, mycall: &str, remote: &str, vias: &[String]) -> bool {
        if self.c.session.state() == fm::State::AwaitGo && !self.c.tx.ptt() && self.c.tx.is_empty() && !self.linked() {
            self.c.session = fm::Session::new(self.c.cfg.clone());
        }
        if self.c.session.state() != fm::State::Listen || self.digi.active() {
            return false;
        }
        let ok = self.c.session.connect(self.c.now, Some(mycall), remote, vias);
        self.drain();
        ok
    }

    fn kiss(&mut self, cmd: kiss::Command, data: Vec<u8>) -> bool {
        let queued: usize = self.unproto.iter().map(|u| u.0).sum();
        if queued + data.len() > kiss::MAX_QUEUED_BYTES {
            return false;
        }
        if let Some(f) = kiss::encode(cmd, &data).ok().and_then(|b| broadcast::frames(self.c.cfg.bw, &b.bytes)) {
            self.unproto.push_back((data.len(), f));
        }
        true
    }

    fn cq_frame(&mut self, call: &str, bw: Option<&str>) -> bool {
        if !self.idle() {
            return false;
        }
        match broadcast::cq_buffer(call, bw).and_then(|b| broadcast::frames(self.c.cfg.bw, &b)) {
            Some(f) => {
                self.unproto.push_back((0, f));
                true
            }
            None => false,
        }
    }

    fn set_ignore_dcd(&mut self, on: bool) {
        self.ignore_dcd = on;
    }

    fn set_bw(&mut self, _: Bw) -> bool {
        false
    }

    fn set_p2p(&mut self, _: bool) {}

    fn set_kiss_rx(&mut self, on: bool) {
        self.kiss_rx = on;
        self.refresh_listen();
    }
}

impl Arq for fm::Session {
    type Config = fm::Config;
    type Listen = fm::Listen;

    fn listening(&self) -> bool {
        self.state() == fm::State::Listen
    }

    fn buffered(&self) -> usize {
        fm::Session::buffered(self)
    }

    fn config_mut(&mut self) -> &mut fm::Config {
        fm::Session::config_mut(self)
    }

    fn send(&mut self, now: Time, data: &[u8]) {
        fm::Session::send(self, now, data)
    }

    fn disconnect(&mut self, now: Time) {
        fm::Session::disconnect(self, now)
    }

    fn abort(&mut self, now: Time) {
        fm::Session::abort(self, now)
    }

    fn drop_link(&mut self) {
        fm::Session::drop_link(self)
    }
}

fn quantised(mut x: Vec<f64>) -> Vec<f64> {
    x.iter_mut().for_each(|v| *v = quantise(*v));
    x
}

fn level_note(record: Record, frame: &[u8], vias: bool) -> String {
    match parse_data(record, frame, vias) {
        Some(d) if record == Record::Start && d.trailer.level != 0 => format!(" lvl {}", d.trailer.level),
        _ => String::new(),
    }
}

fn rx_end(ev: &Event) -> Option<Time> {
    match ev {
        Event::Ctrl(c) => Some(c.t_end48()),
        Event::Tx(t) => Some(t.t_end48()),
        Event::Frame(_) => None,
    }
}

fn data_in(t: &Transmission, t_end: Time) -> fm::DataIn {
    let frames = t
        .frames
        .iter()
        .map(|f| fm::FrameIn {
            ok: f.ok && !f.missing,
            bytes: if f.ok { f.bytes.clone() } else { Vec::new() },
            snr_db: f.snr_db,
        })
        .collect();
    fm::DataIn { record: t.record, frames, t_end }
}

fn passband(bw: FmBandwidth) -> (f64, f64) {
    let r = bw.layouts();
    let lo = r.iter().map(|r| r.first).min().unwrap_or(0);
    let hi = r.iter().map(|r| r.first + r.step * (r.nc - 1)).max().unwrap_or(0);
    let df = FS / NFFT as f64;
    ((f64::from(lo) - 0.5) * df, (f64::from(hi) + 0.5) * df)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_level_is_a_data_level() {
        let st = Station::new(fm::Config::new(FmBandwidth::Narrow, "N0CALL"), Options::default());
        assert_eq!(st.c.session.level(), Record::Start);
        assert_eq!(st.link().level, 1);
    }
}
