use alsa_sys as sys;
use std::ffi::{CStr, CString, c_int, c_void};
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::ptr;
use std::time::{Duration, Instant};

use super::ratematch::{self, Queue, TxBuffer};
use super::{Audio, BLOCK, DeviceList, IO_TIMEOUT, RATE, capture_only, quantize, remaining, sample};

type Frames = sys::snd_pcm_uframes_t;

const PERIOD: Frames = BLOCK as Frames;
const CAPTURE_BUFFER: Frames = RATE as Frames;
const PLAYBACK_START: Frames = 2 * PERIOD;
const DEPTH: usize = 16;
const WALK_DEPTH: usize = 64;
const REPORT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(50);

const SERVERS: [&str; 4] = ["default", "pulse", "pipewire", "jack"];
const NEED_CARD: [&str; 4] = ["hw", "plughw", "dmix", "dshare"];
const PASS: [&str; 18] = [
    "plug",
    "route",
    "rate",
    "linear",
    "lfloat",
    "mulaw",
    "alaw",
    "adpcm",
    "iec958",
    "copy",
    "softvol",
    "dmix",
    "dshare",
    "file",
    "hooks",
    "empty",
    "mmap_emul",
    "asym",
];

#[derive(Clone, Debug, PartialEq)]
enum Def {
    Name(String),
    Plugin { kind: String, slave: Option<Box<Def>> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Card,
    Null,
}

fn name_problem(name: &str) -> Option<String> {
    let (base, args) = name.split_once(':').unwrap_or((name, ""));
    if base.is_empty() || base.contains('.') {
        Some("not a PCM name".into())
    } else if SERVERS.contains(&base) || base.starts_with("sysdefault") {
        Some("it can route to whatever the default device is".into())
    } else if args.is_empty() && NEED_CARD.contains(&base) {
        Some(format!("it takes the default card; name it ({base}:CARD=...)"))
    } else {
        None
    }
}

fn follow(
    lookup: &mut dyn FnMut(&str) -> Result<Def, String>,
    def: &Def,
    top: &str,
    depth: usize,
) -> Result<Route, String> {
    if depth > DEPTH {
        return Err(format!("{top}: ALSA definition nested too deeply"));
    }
    match def {
        Def::Name(n) => {
            if let Some(why) = name_problem(n) {
                let via = if n == top { String::new() } else { format!("it goes through {n:?}, and ") };
                return Err(format!("refusing ALSA playback to {top:?}: {via}{why}"));
            }
            let d = lookup(n)?;
            follow(lookup, &d, top, depth + 1)
        }
        Def::Plugin { kind, .. } if kind == "hw" => Ok(Route::Card),
        Def::Plugin { kind, .. } if kind == "null" => Ok(Route::Null),
        Def::Plugin { kind, slave: Some(s) } if PASS.contains(&kind.as_str()) => follow(lookup, s, top, depth + 1),
        Def::Plugin { kind, .. } => Err(format!(
            "refusing ALSA playback to {top:?}: it goes through a {kind:?} plugin; only ALSA's own plugins \
             in front of a hw card are allowed (sound servers can route anywhere)"
        )),
    }
}

fn check_playback(
    lookup: &mut dyn FnMut(&str) -> Result<Def, String>,
    name: &str,
    radio: bool,
) -> Result<Route, String> {
    if radio {
        return follow(lookup, &Def::Name(name.into()), name, 0);
    }
    let refuse = || {
        format!(
            "refusing ALSA playback to {name:?}: without --radio only the ALSA null PCM is allowed \
             (a real radio interface needs --radio)"
        )
    };
    if name != "null" {
        return Err(refuse());
    }
    match lookup(name)? {
        Def::Plugin { kind, .. } if kind == "null" => Ok(Route::Null),
        _ => Err(refuse()),
    }
}

struct Config(*mut sys::snd_config_t);

struct Node(*mut sys::snd_config_t);

impl Drop for Node {
    fn drop(&mut self) {
        // SAFETY: snd_config_search_definition handed over this copy and only Drop deletes it.
        unsafe { sys::snd_config_delete(self.0) };
    }
}

impl Node {
    fn cfg(&self) -> Cfg<'_> {
        Cfg(self.0, PhantomData)
    }
}

#[derive(Clone, Copy)]
struct Cfg<'a>(*mut sys::snd_config_t, PhantomData<&'a Node>);

impl<'a> Cfg<'a> {
    fn child(self, key: &str) -> Option<Self> {
        let key = CString::new(key).ok()?;
        let mut out = ptr::null_mut();
        // SAFETY: self.0 is a node of a tree the borrowed Node keeps alive, and key is a C string.
        (unsafe { sys::snd_config_search(self.0, key.as_ptr(), &mut out) } >= 0).then_some(Cfg(out, PhantomData))
    }

    fn id(self) -> Option<String> {
        let mut s = ptr::null();
        // SAFETY: self.0 is alive for 'a and a non-null id points into it.
        unsafe {
            if sys::snd_config_get_id(self.0, &mut s) < 0 || s.is_null() {
                return None;
            }
            Some(CStr::from_ptr(s).to_string_lossy().into_owned())
        }
    }

    fn children(self) -> Vec<Self> {
        let mut out = Vec::new();
        // SAFETY: self.0 is alive for 'a and only a compound node is iterated, from first to end.
        unsafe {
            if sys::snd_config_get_type(self.0) != sys::SND_CONFIG_TYPE_COMPOUND {
                return out;
            }
            let end = sys::snd_config_iterator_end(self.0);
            let mut i = sys::snd_config_iterator_first(self.0);
            while i != end {
                out.push(Cfg(sys::snd_config_iterator_entry(i), PhantomData));
                i = sys::snd_config_iterator_next(i);
            }
        }
        out
    }

    fn string(self) -> Option<String> {
        let mut s = ptr::null();
        // SAFETY: self.0 is alive for 'a and a non-null string points into it.
        unsafe {
            if sys::snd_config_get_string(self.0, &mut s) < 0 || s.is_null() {
                return None;
            }
            Some(CStr::from_ptr(s).to_string_lossy().into_owned())
        }
    }
}

impl Config {
    fn load() -> io::Result<Self> {
        let mut top = ptr::null_mut();
        // SAFETY: top receives a reference that Config's Drop releases with snd_config_unref.
        let r = unsafe { sys::snd_config_update_ref(&mut top) };
        if r < 0 {
            return Err(io::Error::other(format!("ALSA configuration: {}", io::Error::from_raw_os_error(-r))));
        }
        Ok(Self(top))
    }

    fn definition(&self, base: &str, name: &str) -> Result<Node, String> {
        let (b, k) = (CString::new(base).map_err(|e| e.to_string())?, CString::new(name).map_err(|e| e.to_string())?);
        let mut out = ptr::null_mut();
        // SAFETY: self.0 is a live config tree, b and k are C strings, and out becomes an owned copy.
        if unsafe { sys::snd_config_search_definition(self.0, b.as_ptr(), k.as_ptr(), &mut out) } < 0 {
            return Err(format!("no ALSA {base} named {name:?} (aplay -L)"));
        }
        Ok(Node(out))
    }

    fn pcm(&self, name: &str) -> Result<Def, String> {
        let n = self.definition("pcm", name)?;
        self.parse(n.cfg(), name)
    }

    fn parse(&self, n: Cfg<'_>, at: &str) -> Result<Def, String> {
        if let Some(s) = n.string() {
            return Ok(Def::Name(s));
        }
        let kind = n.child("type").and_then(Cfg::string).ok_or_else(|| format!("ALSA PCM {at:?} has no type"))?;
        let key = if kind == "asym" { "playback" } else { "slave" };
        let slave = match n.child(key) {
            Some(s) if PASS.contains(&kind.as_str()) => Some(Box::new(self.slave(s, at)?)),
            _ => None,
        };
        Ok(Def::Plugin { kind, slave })
    }

    fn slave(&self, n: Cfg<'_>, at: &str) -> Result<Def, String> {
        let missing = || format!("ALSA PCM {at:?}: slave without a pcm");
        match n.string() {
            Some(s) => {
                let d = self.definition("pcm_slave", &s)?;
                self.parse(d.cfg().child("pcm").ok_or_else(missing)?, at)
            }
            None => self.parse(n.child("pcm").ok_or_else(missing)?, at),
        }
    }
}

impl Config {
    fn pcm_command(&self, name: &str, depth: usize) -> Option<String> {
        if depth > WALK_DEPTH {
            return Some(format!("{name:?} nests too deeply to check"));
        }
        let d = self.definition("pcm", name).ok()?;
        match d.cfg().string() {
            Some(alias) => self.pcm_command(&alias, depth + 1),
            None => self.node_command(d.cfg(), depth + 1),
        }
    }

    fn node_command(&self, n: Cfg<'_>, depth: usize) -> Option<String> {
        if depth > WALK_DEPTH {
            return Some("the definition nests too deeply to check".into());
        }
        if n.child("type").and_then(Cfg::string).as_deref() == Some("file") {
            for key in ["file", "infile"] {
                if let Some(f) = n.child(key).and_then(Cfg::string).filter(|f| f.trim_start().starts_with('|')) {
                    return Some(format!("its file plugin runs a command ({f:?})"));
                }
            }
        }
        n.children().into_iter().find_map(|c| match (c.id().as_deref(), c.string()) {
            (Some("pcm" | "playback" | "capture"), Some(r)) => self.pcm_command(&r, depth + 1),
            (Some("slave"), Some(r)) => {
                let d = self.definition("pcm_slave", &r).ok()?;
                self.node_command(d.cfg(), depth + 1)
            }
            _ => self.node_command(c, depth + 1),
        })
    }
}

impl Config {
    fn check_device(&self, name: &str, what: &str) -> io::Result<()> {
        match self.pcm_command(name, 0) {
            Some(why) => Err(io::Error::other(format!("refusing ALSA {what} {name:?}: {why}"))),
            None => Ok(()),
        }
    }

    fn resolve_playback(&self, name: &str, radio: bool) -> io::Result<Route> {
        self.check_device(name, "playback")?;
        check_playback(&mut |n| self.pcm(n), name, radio).map_err(io::Error::other)
    }
}

pub fn check_device(name: &str, what: &str) -> io::Result<()> {
    Config::load()?.check_device(name, what)
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: self.0 holds the reference taken by snd_config_update_ref and only Drop releases it.
        unsafe { sys::snd_config_unref(self.0) };
    }
}

pub fn list() -> io::Result<(DeviceList, DeviceList)> {
    let mut hints = ptr::null_mut();
    // SAFETY: the interface name is a C string and hints receives an array freed below.
    check("snd_device_name_hint", unsafe { sys::snd_device_name_hint(-1, c"pcm".as_ptr(), &mut hints) })
        .map_err(alsa_err("listing ALSA devices"))?;
    let hint = |h: *mut c_void, id: &CStr| {
        // SAFETY: h is a live hint entry and the returned string is malloc'd, copied and freed here.
        unsafe {
            let s = sys::snd_device_name_get_hint(h, id.as_ptr());
            if s.is_null() {
                return None;
            }
            let v = CStr::from_ptr(s).to_str().ok().map(String::from);
            libc::free(s.cast());
            v
        }
    };
    let (mut capture, mut playback) = (DeviceList::new(), DeviceList::new());
    let mut i = 0;
    loop {
        // SAFETY: hints is a null-terminated array and i never passes its terminator.
        let h = unsafe { *hints.add(i) };
        if h.is_null() {
            break;
        }
        i += 1;
        let dir = hint(h, c"IOID");
        let Some(name) = hint(h, c"NAME") else { continue };
        let desc = hint(h, c"DESC").unwrap_or_default().replace('\n', " ");
        if dir.as_deref() != Some("Output") {
            capture.push((name.clone(), desc.clone()));
        }
        if dir.as_deref() != Some("Input") {
            playback.push((name, desc));
        }
    }
    // SAFETY: hints came from snd_device_name_hint and is freed once.
    unsafe { sys::snd_device_name_free_hint(hints) };
    Ok((capture, playback))
}

pub fn resolve_playback(name: &str, radio: bool) -> io::Result<Route> {
    Config::load()?.resolve_playback(name, radio)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Capture,
    Playback,
}

fn what(dir: Direction) -> &'static str {
    if dir == Direction::Capture { "capture" } else { "playback" }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    FloatLE,
    S32LE,
    S16LE,
}

impl Format {
    fn code(self) -> sys::snd_pcm_format_t {
        match self {
            Self::FloatLE => sys::SND_PCM_FORMAT_FLOAT_LE,
            Self::S32LE => sys::SND_PCM_FORMAT_S32_LE,
            Self::S16LE => sys::SND_PCM_FORMAT_S16_LE,
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FloatLE => "FLOAT_LE",
            Self::S32LE => "S32_LE",
            Self::S16LE => "S16_LE",
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct Error {
    func: &'static str,
    errno: c_int,
}

fn check<T: Into<i64>>(func: &'static str, r: T) -> Result<i64, Error> {
    let r = r.into();
    if r < 0 { Err(Error { func, errno: -r as c_int }) } else { Ok(r) }
}

fn alsa_err(what: &str) -> impl Fn(Error) -> io::Error + '_ {
    move |e| io::Error::other(format!("{what}: {}: {}", e.func, io::Error::from_raw_os_error(e.errno)))
}

struct Owned<T>(*mut T, unsafe extern "C" fn(*mut T));

impl<T> Owned<T> {
    fn new(
        func: &'static str,
        malloc: unsafe extern "C" fn(*mut *mut T) -> c_int,
        free: unsafe extern "C" fn(*mut T),
    ) -> Result<Self, Error> {
        let mut p = ptr::null_mut();
        // SAFETY: malloc stores a fresh block in p, which is kept only on success.
        check(func, unsafe { malloc(&mut p) })?;
        Ok(Self(p, free))
    }
}

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        // SAFETY: self.0 came from the matching malloc and only Drop frees it.
        unsafe { (self.1)(self.0) }
    }
}

struct Handle(*mut sys::snd_pcm_t);

impl Handle {
    fn open(name: &CStr, dir: Direction) -> Result<Self, Error> {
        let stream = if dir == Direction::Capture { sys::SND_PCM_STREAM_CAPTURE } else { sys::SND_PCM_STREAM_PLAYBACK };
        let mut p = ptr::null_mut();
        // SAFETY: name is a C string and p is kept only once snd_pcm_open succeeds.
        check("snd_pcm_open", unsafe { sys::snd_pcm_open(&mut p, name.as_ptr(), stream, sys::SND_PCM_NONBLOCK) })?;
        Ok(Self(p))
    }

    fn start(&self) -> Result<(), Error> {
        // SAFETY: self.0 is open until Drop.
        check("snd_pcm_start", unsafe { sys::snd_pcm_start(self.0) }).map(drop)
    }

    fn stop(&self) -> Result<(), Error> {
        // SAFETY: self.0 is open until Drop.
        check("snd_pcm_drop", unsafe { sys::snd_pcm_drop(self.0) }).map(drop)
    }

    fn recover(&self, err: Error) -> Result<(), Error> {
        // SAFETY: self.0 is open until Drop.
        check("snd_pcm_recover", unsafe { sys::snd_pcm_recover(self.0, -err.errno, 1) }).map(drop)
    }

    fn wait(&self, ms: c_int) -> Result<(), Error> {
        // SAFETY: self.0 is open until Drop.
        check("snd_pcm_wait", unsafe { sys::snd_pcm_wait(self.0, ms) }).map(drop)
    }

    fn buffer_at_least(&self, hw: &Owned<sys::snd_pcm_hw_params_t>, val: &mut Frames) -> Result<(), Error> {
        let mut min = *val;
        // SAFETY: self.0 is open until Drop, hw is a live hw_params block, and a failed set_min leaves hw unchanged.
        unsafe {
            sys::snd_pcm_hw_params_set_buffer_size_min(self.0, hw.0, &mut min);
            check(
                "snd_pcm_hw_params_set_buffer_size_near",
                sys::snd_pcm_hw_params_set_buffer_size_near(self.0, hw.0, val),
            )
            .map(drop)
        }
    }

    fn card(&self) -> Result<i32, Error> {
        let info = Owned::new("snd_pcm_info_malloc", sys::snd_pcm_info_malloc, sys::snd_pcm_info_free)?;
        // SAFETY: self.0 is open until Drop and info is a live info block.
        unsafe {
            check("snd_pcm_info", sys::snd_pcm_info(self.0, info.0))?;
            Ok(sys::snd_pcm_info_get_card(info.0))
        }
    }

    fn state(&self) -> sys::snd_pcm_state_t {
        // SAFETY: self.0 is open until Drop.
        unsafe { sys::snd_pcm_state(self.0) }
    }

    fn readi(&self, buf: &mut [u8], frame: usize) -> Result<usize, Error> {
        // SAFETY: self.0 is open and buf has room for buf.len() / frame whole frames.
        let n = unsafe { sys::snd_pcm_readi(self.0, buf.as_mut_ptr().cast(), (buf.len() / frame) as Frames) };
        check("snd_pcm_readi", n).map(|n| n as usize)
    }

    fn avail(&self) -> Result<usize, Error> {
        // SAFETY: self.0 is open until Drop.
        check("snd_pcm_avail", unsafe { sys::snd_pcm_avail(self.0) }).map(|n| n as usize)
    }

    fn delay(&self) -> Result<usize, Error> {
        let mut n: sys::snd_pcm_sframes_t = 0;
        // SAFETY: self.0 is open until Drop and n is a valid out pointer.
        check("snd_pcm_delay", unsafe { sys::snd_pcm_delay(self.0, &mut n) })?;
        Ok(n.max(0) as usize)
    }

    fn writei(&self, buf: &[u8], frame: usize) -> Result<usize, Error> {
        // SAFETY: self.0 is open and buf holds buf.len() / frame whole frames.
        let n = unsafe { sys::snd_pcm_writei(self.0, buf.as_ptr().cast(), (buf.len() / frame) as Frames) };
        check("snd_pcm_writei", n).map(|n| n as usize)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: self.0 came from snd_pcm_open and only Drop closes it.
        unsafe { sys::snd_pcm_close(self.0) };
    }
}

struct Pcm {
    pcm: Handle,
    dir: Direction,
    name: String,
    format: Format,
    width: usize,
    channels: usize,
    buffer: usize,
    buf: Vec<u8>,
    xruns: u64,
    reported: Option<Instant>,
}

impl Pcm {
    fn open(cfg: &Config, name: &str, dir: Direction) -> io::Result<Self> {
        cfg.check_device(name, what(dir))?;
        let c = CString::new(name).map_err(|e| io::Error::other(format!("{name:?}: {e}")))?;
        let pcm = Handle::open(&c, dir).map_err(alsa_err(name))?;
        Ok(Self {
            pcm,
            dir,
            name: name.into(),
            format: Format::FloatLE,
            width: 4,
            channels: 1,
            buffer: 0,
            buf: Vec::new(),
            xruns: 0,
            reported: None,
        })
    }

    fn card(&self) -> io::Result<i32> {
        self.pcm.card().map_err(alsa_err(&self.name))
    }

    fn setup(&mut self, want: Frames) -> io::Result<()> {
        let e = alsa_err(&self.name);
        let ok = |func, r: c_int| check(func, r).map_err(&e);
        let hw_params =
            || Owned::new("snd_pcm_hw_params_malloc", sys::snd_pcm_hw_params_malloc, sys::snd_pcm_hw_params_free);
        let (p, capture) = (self.pcm.0, self.dir == Direction::Capture);
        let (hw, cur) = (hw_params().map_err(&e)?, hw_params().map_err(&e)?);
        let (mut period, mut buffer) = (PERIOD, want);
        let (mut rate, mut channels, mut dir) = (0, 1, 0);
        // SAFETY: p is an open PCM and hw is a live hw_params block.
        let found = unsafe {
            ok("snd_pcm_hw_params_any", sys::snd_pcm_hw_params_any(p, hw.0))?;
            ok(
                "snd_pcm_hw_params_set_access",
                sys::snd_pcm_hw_params_set_access(p, hw.0, sys::SND_PCM_ACCESS_RW_INTERLEAVED),
            )?;
            [Format::S16LE, Format::S32LE, Format::FloatLE]
                .into_iter()
                .find(|&f| sys::snd_pcm_hw_params_test_format(p, hw.0, f.code()) >= 0)
        };
        let format = found
            .ok_or_else(|| io::Error::other(format!("{}: no S16_LE, S32_LE or FLOAT_LE sample format", self.name)))?;
        // SAFETY: p is an open PCM and hw is a live hw_params block.
        let rate_set = unsafe {
            ok("snd_pcm_hw_params_set_format", sys::snd_pcm_hw_params_set_format(p, hw.0, format.code()))?;
            ok(
                "snd_pcm_hw_params_set_channels_near",
                sys::snd_pcm_hw_params_set_channels_near(p, hw.0, &mut channels),
            )?;
            sys::snd_pcm_hw_params_set_rate(p, hw.0, RATE, 0) >= 0
        };
        if !rate_set {
            return Err(io::Error::other(format!(
                "{}: the device cannot run at {RATE} Hz (a plughw: PCM lets ALSA convert the rate)",
                self.name
            )));
        }
        // SAFETY: p is an open PCM and hw and cur are live hw_params blocks.
        unsafe {
            ok(
                "snd_pcm_hw_params_set_period_size_near",
                sys::snd_pcm_hw_params_set_period_size_near(p, hw.0, &mut period, &mut dir),
            )?;
            self.pcm.buffer_at_least(&hw, &mut buffer).map_err(&e)?;
            ok("snd_pcm_hw_params", sys::snd_pcm_hw_params(p, hw.0))?;
            ok("snd_pcm_hw_params_current", sys::snd_pcm_hw_params_current(p, cur.0))?;
            ok("snd_pcm_hw_params_get_rate", sys::snd_pcm_hw_params_get_rate(cur.0, &mut rate, &mut dir))?;
            ok("snd_pcm_hw_params_get_channels", sys::snd_pcm_hw_params_get_channels(cur.0, &mut channels))?;
            ok(
                "snd_pcm_hw_params_get_period_size",
                sys::snd_pcm_hw_params_get_period_size(cur.0, &mut period, &mut dir),
            )?;
            ok("snd_pcm_hw_params_get_buffer_size", sys::snd_pcm_hw_params_get_buffer_size(cur.0, &mut buffer))?;
        }
        if rate != RATE {
            return Err(io::Error::other(format!("{}: ALSA did not set {RATE} Hz", self.name)));
        }
        let sw = Owned::new("snd_pcm_sw_params_malloc", sys::snd_pcm_sw_params_malloc, sys::snd_pcm_sw_params_free)
            .map_err(&e)?;
        // SAFETY: p is an open PCM and sw is a live sw_params block.
        unsafe {
            ok("snd_pcm_sw_params_current", sys::snd_pcm_sw_params_current(p, sw.0))?;
            ok("snd_pcm_sw_params_set_avail_min", sys::snd_pcm_sw_params_set_avail_min(p, sw.0, period))?;
            if !capture {
                let start = PLAYBACK_START.max(period).min(buffer);
                ok(
                    "snd_pcm_sw_params_set_start_threshold",
                    sys::snd_pcm_sw_params_set_start_threshold(p, sw.0, start),
                )?;
            }
            ok("snd_pcm_sw_params", sys::snd_pcm_sw_params(p, sw.0))?;
        }
        if capture {
            self.pcm.start().map_err(&e)?;
        }
        info!(
            "alsa {} {:?}: {format}, {channels} channel(s), {RATE} Hz, period {period}, buffer {buffer} frames",
            what(self.dir),
            self.name,
        );
        self.format = format;
        self.buffer = buffer as usize;
        self.width = if format == Format::S16LE { 2 } else { 4 };
        self.channels = channels as usize;
        Ok(())
    }

    fn frame(&self) -> usize {
        self.width * self.channels
    }

    fn recover(&mut self, err: Error) -> io::Result<()> {
        if err.errno == libc::EINTR {
            return Ok(());
        }
        if let Err(r) = self.pcm.recover(err) {
            let what = format!("{} {}", what(self.dir), self.name);
            return Err(io::Error::other(format!(
                "{}; recovery failed: {}",
                alsa_err(&what)(err),
                io::Error::from_raw_os_error(r.errno)
            )));
        }
        if self.dir == Direction::Capture {
            let _ = self.pcm.start();
        }
        self.xruns += 1;
        if self.reported.is_none_or(|t| t.elapsed() >= REPORT) {
            self.reported = Some(Instant::now());
            let kind = match (err.errno, self.dir) {
                (libc::ESTRPIPE, _) => "suspended",
                (_, Direction::Playback) => "underrun",
                _ => "overrun",
            };
            say!("alsa {} {:?}: {kind}, recovered ({} so far)", what(self.dir), self.name, self.xruns);
        }
        Ok(())
    }

    fn wait(&mut self, until: Instant, what: &str) -> io::Result<()> {
        let left = remaining(until, what)?;
        if let Err(e) = self.pcm.wait(left.min(POLL).as_millis().max(1) as c_int) {
            return self.recover(e);
        }
        let errno = match self.pcm.state() {
            sys::SND_PCM_STATE_XRUN => libc::EPIPE,
            sys::SND_PCM_STATE_SUSPENDED => libc::ESTRPIPE,
            sys::SND_PCM_STATE_DISCONNECTED => libc::ENODEV,
            _ => return Ok(()),
        };
        self.recover(Error { func: "snd_pcm_state", errno })
    }
}

fn put(buf: &mut Vec<u8>, format: Format, v: i16) {
    match format {
        Format::S16LE => buf.extend_from_slice(&v.to_le_bytes()),
        Format::S32LE => buf.extend_from_slice(&(i32::from(v) << 16).to_le_bytes()),
        _ => buf.extend_from_slice(&(sample(v) as f32).to_le_bytes()),
    }
}

fn get(f: &[u8], format: Format) -> i16 {
    match format {
        Format::S16LE => i16::from_le_bytes([f[0], f[1]]),
        Format::S32LE => quantize(f64::from(i32::from_le_bytes([f[0], f[1], f[2], f[3]])) / 2147483648.0),
        _ => quantize(f64::from(f32::from_le_bytes([f[0], f[1], f[2], f[3]]))),
    }
}

fn short_buffer(name: &str, granted: usize, tx: TxBuffer) -> Option<String> {
    if granted >= tx.capacity() {
        return None;
    }
    let ms = |frames: usize| frames * 1000 / RATE as usize;
    let fits = granted / TxBuffer::ms(1).capacity();
    let advice = if fits >= ratematch::MIN_MS as usize {
        format!("use --tx-buffer-ms {fits} or less")
    } else {
        format!("even --tx-buffer-ms {} needs {} ms", ratematch::MIN_MS, ms(TxBuffer::ms(ratematch::MIN_MS).capacity()))
    };
    Some(format!(
        "ALSA playback {name:?}: the device buffers at most {} ms, but --tx-buffer-ms {} needs {} ms (4x); {advice}",
        ms(granted),
        ms(tx.target),
        ms(tx.capacity())
    ))
}

pub struct AlsaIo {
    rec: Pcm,
    play: Option<Pcm>,
}

impl AlsaIo {
    pub fn open(capture: &str, playback: &str, radio: bool, tx: TxBuffer) -> io::Result<Self> {
        let cfg = Config::load()?;
        let route = cfg.resolve_playback(playback, radio)?;
        let mut play = Pcm::open(&cfg, playback, Direction::Playback)?;
        let card = play.card()?;
        if (route == Route::Card) != (card >= 0) {
            return Err(io::Error::other(format!(
                "refusing ALSA playback to {playback:?}: its configuration says {route:?} but ALSA opened card {card}"
            )));
        }
        let mut rec = Pcm::open(&cfg, capture, Direction::Capture)?;
        play.setup(tx.capacity() as Frames)?;
        if let Some(e) = short_buffer(playback, play.buffer, tx) {
            return Err(io::Error::other(e));
        }
        rec.setup(CAPTURE_BUFFER)?;
        Ok(Self { rec, play: Some(play) })
    }

    pub fn open_capture(capture: &str) -> io::Result<Self> {
        let mut rec = Pcm::open(&Config::load()?, capture, Direction::Capture)?;
        rec.setup(CAPTURE_BUFFER)?;
        Ok(Self { rec, play: None })
    }
}

impl Audio for AlsaIo {
    fn read(&mut self, x: &mut [i16]) -> io::Result<()> {
        let r = &mut self.rec;
        let frame = r.frame();
        let need = x.len() * frame;
        r.buf.resize(need, 0);
        let (mut got, until) = (0, Instant::now() + IO_TIMEOUT);
        while got < need {
            let n = r.pcm.readi(&mut r.buf[got..], frame);
            match n {
                Ok(0) => r.wait(until, "capture (no audio)")?,
                Ok(n) => got += n * frame,
                Err(e) if e.errno == libc::EAGAIN => r.wait(until, "capture (no audio)")?,
                Err(e) => r.recover(e)?,
            }
        }
        for (v, f) in x.iter_mut().zip(r.buf.chunks_exact(frame)) {
            *v = get(f, r.format);
        }
        Ok(())
    }

    fn write(&mut self, x: &[i16]) -> io::Result<()> {
        let Some(p) = &mut self.play else { return Err(capture_only()) };
        p.buf.clear();
        for &v in x {
            for _ in 0..p.channels {
                put(&mut p.buf, p.format, v);
            }
        }
        let frame = p.frame();
        let (mut off, until) = (0, Instant::now() + IO_TIMEOUT);
        while off < p.buf.len() {
            let n = p.pcm.writei(&p.buf[off..], frame);
            match n {
                Ok(0) => p.wait(until, "playback (device not draining)")?,
                Ok(n) => off += n * frame,
                Err(e) if e.errno == libc::EAGAIN => p.wait(until, "playback (device not draining)")?,
                Err(e) => p.recover(e)?,
            }
        }
        Ok(())
    }

    fn queued(&mut self) -> io::Result<Option<Queue>> {
        let Some(p) = &mut self.play else { return Err(capture_only()) };
        let frames = match p.pcm.delay() {
            Ok(n) => n,
            Err(e) => p.recover(e).map(|()| 0)?,
        };
        Ok(Some(Queue { frames, floor: 0, underruns: p.xruns }))
    }

    fn backlog(&mut self) -> io::Result<Option<usize>> {
        Ok(self.rec.pcm.avail().ok())
    }
}

impl Drop for AlsaIo {
    fn drop(&mut self) {
        let _ = self.rec.pcm.stop();
        if let Some(p) = &self.play {
            let _ = p.pcm.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn plugin(kind: &str, slave: Option<Def>) -> Def {
        Def::Plugin { kind: kind.into(), slave: slave.map(Box::new) }
    }

    fn name(n: &str) -> Def {
        Def::Name(n.into())
    }

    fn table() -> HashMap<&'static str, Def> {
        let hw = || plugin("hw", None);
        HashMap::from([
            ("null", plugin("null", None)),
            ("hw:1,0", hw()),
            ("hw:9,0", hw()),
            ("plughw:CARD=Device,DEV=0", plugin("plug", Some(hw()))),
            ("default", plugin("pipewire", None)),
            ("pulse", plugin("pulse", None)),
            ("pipewire", plugin("pipewire", None)),
            ("radio", plugin("plug", Some(name("hw:1,0")))),
            ("radio_asym", plugin("asym", Some(plugin("dmix", Some(name("hw:9,0")))))),
            ("radio_file", plugin("file", Some(name("null")))),
            ("alias_pulse", name("pulse")),
            ("via_default", plugin("plug", Some(name("default")))),
            ("via_sysdefault", plugin("route", Some(name("sysdefault:CARD=Device")))),
            ("via_bare_hw", plugin("plug", Some(name("hw")))),
            ("via_dot", plugin("plug", Some(name("pcm.pulse")))),
            ("inline_pw", plugin("plug", Some(plugin("pipewire", None)))),
            ("speex", plugin("speex", Some(name("hw:1,0")))),
            ("multi", plugin("multi", None)),
            ("dangling", plugin("plug", None)),
            ("loop", name("loop")),
        ])
    }

    fn check(n: &str, radio: bool) -> Result<Route, String> {
        let t = table();
        check_playback(&mut |k| t.get(k).cloned().ok_or_else(|| format!("no ALSA pcm named {k:?}")), n, radio)
    }

    #[test]
    fn without_radio_only_the_null_pcm_plays() {
        assert_eq!(check("null", false), Ok(Route::Null));
        for n in ["hw:1,0", "plughw:CARD=Device,DEV=0", "radio", "radio_file", "default", "pulse", "null:x", " null"] {
            assert!(check(n, false).unwrap_err().contains("needs --radio"), "{n}");
        }
        let t = HashMap::from([("null", plugin("plug", Some(name("hw:1,0"))))]);
        assert!(check_playback(&mut |k| t.get(k).cloned().ok_or_else(String::new), "null", false).is_err());
    }

    #[test]
    fn radio_allows_alsa_plugins_in_front_of_a_card_or_null() {
        for n in ["hw:1,0", "plughw:CARD=Device,DEV=0", "radio", "radio_asym"] {
            assert_eq!(check(n, true), Ok(Route::Card), "{n}");
        }
        assert_eq!(check("radio_file", true), Ok(Route::Null));
        assert_eq!(check("null", true), Ok(Route::Null));
    }

    #[test]
    fn default_and_sound_server_names_are_refused_in_every_mode() {
        for radio in [false, true] {
            for n in [
                "default",
                "default:CARD=Device",
                "sysdefault",
                "sysdefault:CARD=Device",
                "pulse",
                "pipewire",
                "pipewire:NODE=xara_tx",
                "jack",
                "",
                "pcm.pulse",
            ] {
                assert!(check(n, radio).is_err(), "{n} radio={radio}");
            }
        }
        for n in ["hw", "plughw", "dmix", "dshare"] {
            assert!(check(n, true).unwrap_err().contains("default card"), "{n}");
        }
    }

    #[test]
    fn radio_refuses_anything_that_resolves_to_a_sound_server_or_unknown_plugin() {
        for (n, why) in [
            ("alias_pulse", "goes through \"pulse\""),
            ("via_default", "goes through \"default\""),
            ("via_sysdefault", "goes through \"sysdefault:CARD=Device\""),
            ("via_bare_hw", "default card"),
            ("via_dot", "not a PCM name"),
            ("inline_pw", "\"pipewire\" plugin"),
            ("speex", "\"speex\" plugin"),
            ("multi", "\"multi\" plugin"),
            ("dangling", "\"plug\" plugin"),
            ("loop", "nested too deeply"),
            ("nonexistent", "no ALSA pcm named"),
        ] {
            let e = check(n, true).unwrap_err();
            assert!(e.contains(why), "{n}: {e}");
        }
    }

    #[test]
    fn a_playback_buffer_smaller_than_the_tx_capacity_is_refused() {
        let tx = TxBuffer::ms(500);
        assert_eq!(short_buffer("dmix:CARD=Device", 96000, tx), None);
        assert_eq!(short_buffer("dmix:CARD=Device", 100000, tx), None);
        assert_eq!(
            short_buffer("dmix:CARD=Device", 16384, tx).as_deref(),
            Some(
                "ALSA playback \"dmix:CARD=Device\": the device buffers at most 341 ms, but --tx-buffer-ms 500 needs \
                 2000 ms (4x); use --tx-buffer-ms 85 or less"
            )
        );
        assert_eq!(short_buffer("hw:1,0", 7680, TxBuffer::ms(40)), None);
        assert!(
            short_buffer("hw:1,0", 7679, TxBuffer::ms(40)).is_some_and(|e| e.ends_with("--tx-buffer-ms 39 or less"))
        );
        assert!(short_buffer("hw:1,0", 4096, TxBuffer::ms(40)).is_some_and(|e| {
            e.ends_with("at most 85 ms, but --tx-buffer-ms 40 needs 160 ms (4x); even --tx-buffer-ms 30 needs 120 ms")
        }));
    }

    #[test]
    fn the_playback_buffer_rounds_up_to_whole_periods_unless_the_device_max_is_lower() {
        let pcm = Handle::open(c"null", Direction::Playback).unwrap();
        let setup = |max: Option<Frames>| {
            let hw = Owned::new("snd_pcm_hw_params_malloc", sys::snd_pcm_hw_params_malloc, sys::snd_pcm_hw_params_free)
                .unwrap();
            let (mut period, mut channels, mut dir) = (PERIOD, 1, 0);
            // SAFETY: pcm is open and hw is a live hw_params block.
            unsafe {
                assert!(sys::snd_pcm_hw_params_any(pcm.0, hw.0) >= 0);
                assert!(sys::snd_pcm_hw_params_set_access(pcm.0, hw.0, sys::SND_PCM_ACCESS_RW_INTERLEAVED) >= 0);
                assert!(sys::snd_pcm_hw_params_set_format(pcm.0, hw.0, Format::S16LE.code()) >= 0);
                assert!(sys::snd_pcm_hw_params_set_channels_near(pcm.0, hw.0, &mut channels) >= 0);
                assert!(sys::snd_pcm_hw_params_set_rate(pcm.0, hw.0, RATE, 0) >= 0);
                assert!(sys::snd_pcm_hw_params_set_period_size_near(pcm.0, hw.0, &mut period, &mut dir) >= 0);
                assert!(sys::snd_pcm_hw_params_set_periods_integer(pcm.0, hw.0) >= 0);
                if let Some(mut max) = max {
                    assert!(sys::snd_pcm_hw_params_set_buffer_size_max(pcm.0, hw.0, &mut max) >= 0);
                }
            }
            assert_eq!(period, PERIOD);
            hw
        };
        let tx = TxBuffer::ms(33);
        let mut buffer = tx.capacity() as Frames;
        pcm.buffer_at_least(&setup(None), &mut buffer).unwrap();
        assert_eq!(buffer, 14 * PERIOD);
        assert_eq!(short_buffer("null", buffer as usize, tx), None);
        let mut buffer = tx.capacity() as Frames;
        pcm.buffer_at_least(&setup(Some(4000)), &mut buffer).unwrap();
        assert_eq!(buffer, 8 * PERIOD);
        assert!(short_buffer("null", buffer as usize, tx).is_some_and(|e| e.contains("at most 80 ms")));
    }

    #[test]
    fn samples_round_trip_through_the_device_formats() {
        for (f, w) in [(Format::FloatLE, 4), (Format::S32LE, 4), (Format::S16LE, 2)] {
            for v in [0, 1, -1, 16384, -16384, 32767, -32768] {
                let mut b = Vec::new();
                put(&mut b, f, v);
                assert_eq!(b.len(), w);
                assert_eq!(get(&b, f), v, "{f} {v}");
            }
        }
        let mut b = Vec::new();
        put(&mut b, Format::S32LE, -32768);
        assert_eq!(b, i32::MIN.to_le_bytes());
        assert_eq!(get(&2.0f32.to_le_bytes(), Format::FloatLE), 32767);
        assert_eq!(get(&i32::MAX.to_le_bytes(), Format::S32LE), 32767);
        assert_eq!(get(&(-3.0f32).to_le_bytes(), Format::FloatLE), -32768);
    }
}
