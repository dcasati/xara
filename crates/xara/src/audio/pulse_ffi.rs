use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::marker::PhantomData;
use std::ptr::{NonNull, null, null_mut};
use std::rc::Rc;
use std::time::Instant;

use super::remaining;

type Raw = c_void;
type ListCb<T> = extern "C" fn(*mut Raw, *const T, c_int, *mut c_void);
type ListFn<T> = unsafe extern "C" fn(*mut Raw, ListCb<T>, *mut c_void) -> *mut Raw;
type NotifyCb = extern "C" fn(*mut Raw, *mut c_void);
type SuccessCb = extern "C" fn(*mut Raw, c_int, *mut c_void);

pub const S16LE: c_int = 3;
pub const STREAM_DONT_MOVE: c_int = 0x0200;
pub const STREAM_ADJUST_LATENCY: c_int = 0x2000;
pub const STREAM_INTERPOLATE_TIMING: c_int = 0x0002;
pub const STREAM_AUTO_TIMING_UPDATE: c_int = 0x0008;
const CONTEXT_NOAUTOSPAWN: c_int = 1;
const CONTEXT_READY: c_int = 4;
const CONTEXT_FAILED: c_int = 5;
const CONTEXT_TERMINATED: c_int = 6;
const STREAM_READY: c_int = 2;
const STREAM_FAILED: c_int = 3;
const STREAM_TERMINATED: c_int = 4;
const OPERATION_RUNNING: c_int = 0;
const SEEK_RELATIVE: c_int = 0;

#[repr(C)]
pub struct SampleSpec {
    pub format: c_int,
    pub rate: u32,
    pub channels: u8,
}

#[repr(C)]
pub struct BufferAttr {
    pub maxlength: u32,
    pub tlength: u32,
    pub prebuf: u32,
    pub minreq: u32,
    pub fragsize: u32,
}

#[repr(C)]
struct ChannelMap {
    _channels: u8,
    _map: [c_int; 32],
}

#[repr(C)]
struct CVolume {
    _channels: u8,
    _values: [u32; 32],
}

#[repr(C)]
pub struct SinkInfo {
    name: *const c_char,
    _index: u32,
    description: *const c_char,
    _sample_spec: SampleSpec,
    _channel_map: ChannelMap,
    _owner_module: u32,
    _volume: CVolume,
    _mute: c_int,
    _monitor_source: u32,
    _monitor_source_name: *const c_char,
    _latency: u64,
    _driver: *const c_char,
    _flags: c_int,
    proplist: *const Raw,
}

#[cfg(target_pointer_width = "64")]
#[repr(C)]
struct TimingInfo {
    _timestamp: libc::timeval,
    _synchronized_clocks: c_int,
    sink_usec: u64,
}

#[repr(C)]
pub struct SourceInfo {
    name: *const c_char,
    _index: u32,
    description: *const c_char,
}

#[link(name = "pulse")]
unsafe extern "C" {
    fn pa_mainloop_new() -> *mut Raw;
    fn pa_mainloop_free(m: *mut Raw);
    fn pa_mainloop_get_api(m: *mut Raw) -> *mut Raw;
    fn pa_mainloop_prepare(m: *mut Raw, timeout: c_int) -> c_int;
    fn pa_mainloop_poll(m: *mut Raw) -> c_int;
    fn pa_mainloop_dispatch(m: *mut Raw) -> c_int;
    fn pa_context_new(api: *mut Raw, name: *const c_char) -> *mut Raw;
    fn pa_context_connect(c: *mut Raw, server: *const c_char, flags: c_int, spawn: *const Raw) -> c_int;
    fn pa_context_get_state(c: *mut Raw) -> c_int;
    fn pa_context_errno(c: *mut Raw) -> c_int;
    fn pa_context_disconnect(c: *mut Raw);
    fn pa_context_unref(c: *mut Raw);
    fn pa_context_get_sink_info_list(c: *mut Raw, cb: ListCb<SinkInfo>, userdata: *mut c_void) -> *mut Raw;
    fn pa_context_get_source_info_list(c: *mut Raw, cb: ListCb<SourceInfo>, userdata: *mut c_void) -> *mut Raw;
    fn pa_operation_get_state(o: *mut Raw) -> c_int;
    fn pa_operation_cancel(o: *mut Raw);
    fn pa_operation_unref(o: *mut Raw);
    fn pa_proplist_gets(p: *const Raw, key: *const c_char) -> *const c_char;
    fn pa_proplist_contains(p: *const Raw, key: *const c_char) -> c_int;
    fn pa_stream_new(c: *mut Raw, name: *const c_char, ss: *const SampleSpec, map: *const Raw) -> *mut Raw;
    fn pa_stream_connect_record(s: *mut Raw, dev: *const c_char, attr: *const BufferAttr, flags: c_int) -> c_int;
    fn pa_stream_connect_playback(
        s: *mut Raw,
        dev: *const c_char,
        attr: *const BufferAttr,
        flags: c_int,
        volume: *const Raw,
        sync: *mut Raw,
    ) -> c_int;
    fn pa_stream_get_state(s: *mut Raw) -> c_int;
    fn pa_stream_get_device_name(s: *mut Raw) -> *const c_char;
    fn pa_stream_peek(s: *mut Raw, data: *mut *const c_void, nbytes: *mut usize) -> c_int;
    fn pa_stream_drop(s: *mut Raw) -> c_int;
    fn pa_stream_readable_size(s: *mut Raw) -> usize;
    fn pa_stream_get_latency(s: *mut Raw, usec: *mut u64, negative: *mut c_int) -> c_int;
    #[cfg(target_pointer_width = "64")]
    fn pa_stream_get_timing_info(s: *mut Raw) -> *const TimingInfo;
    fn pa_stream_update_timing_info(s: *mut Raw, cb: Option<SuccessCb>, userdata: *mut c_void) -> *mut Raw;
    fn pa_stream_set_underflow_callback(s: *mut Raw, cb: Option<NotifyCb>, userdata: *mut c_void);
    fn pa_stream_write(
        s: *mut Raw,
        data: *const c_void,
        nbytes: usize,
        free_cb: Option<extern "C" fn(*mut c_void)>,
        offset: i64,
        seek: c_int,
    ) -> c_int;
    fn pa_stream_disconnect(s: *mut Raw) -> c_int;
    fn pa_stream_unref(s: *mut Raw);
    fn pa_strerror(error: c_int) -> *const c_char;
}

fn text(p: *const c_char) -> Option<String> {
    // SAFETY: every pointer passed here is null or a NUL-terminated string owned by libpulse for the call.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

fn error(what: &str, code: c_int) -> io::Error {
    // SAFETY: pa_strerror takes any int and returns null or a static string.
    io::Error::other(format!("{what}: {}", text(unsafe { pa_strerror(code) }).unwrap_or_default()))
}

fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|e| io::Error::other(format!("{s}: {e}")))
}

fn status(what: &str, r: c_int) -> io::Result<()> {
    if r == 0 { Ok(()) } else { Err(error(what, r)) }
}

struct Inner {
    ml: NonNull<Raw>,
    ctx: NonNull<Raw>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: Inner owns both handles, every stream and operation holding them is gone, and the context goes first.
        unsafe {
            pa_context_disconnect(self.ctx.as_ptr());
            pa_context_unref(self.ctx.as_ptr());
            pa_mainloop_free(self.ml.as_ptr());
        }
    }
}

#[derive(Clone)]
pub struct Conn(Rc<Inner>);

impl Conn {
    pub fn open(until: Instant) -> io::Result<Self> {
        // SAFETY: no arguments; the result is checked for null.
        let ml = NonNull::new(unsafe { pa_mainloop_new() }).ok_or_else(|| io::Error::other("pulse mainloop"))?;
        // SAFETY: ml is a live mainloop and the name is NUL-terminated; the context is freed before ml below or in Inner.
        let ctx = NonNull::new(unsafe { pa_context_new(pa_mainloop_get_api(ml.as_ptr()), c"xara".as_ptr()) });
        let Some(ctx) = ctx else {
            // SAFETY: ml came from pa_mainloop_new and nothing else refers to it.
            unsafe { pa_mainloop_free(ml.as_ptr()) };
            return Err(io::Error::other("pulse context"));
        };
        let c = Conn(Rc::new(Inner { ml, ctx }));
        // SAFETY: ctx is live; a null server and spawn api select the defaults.
        let r = unsafe { pa_context_connect(c.ctx(), null(), CONTEXT_NOAUTOSPAWN, null()) };
        if r < 0 {
            return Err(error("sound server", r));
        }
        loop {
            // SAFETY: ctx is live.
            match unsafe { pa_context_get_state(c.ctx()) } {
                CONTEXT_READY => return Ok(c),
                CONTEXT_FAILED | CONTEXT_TERMINATED => return Err(c.error("sound server")),
                _ => c.iterate(until, "sound server")?,
            }
        }
    }

    fn ctx(&self) -> *mut Raw {
        self.0.ctx.as_ptr()
    }

    pub fn error(&self, what: &str) -> io::Error {
        // SAFETY: ctx is live.
        error(what, unsafe { pa_context_errno(self.ctx()) })
    }

    pub fn iterate(&self, until: Instant, what: &str) -> io::Result<()> {
        self.run(c_int::try_from(remaining(until, what)?.as_micros()).unwrap_or(c_int::MAX), what)
    }

    pub fn dispatch(&self, what: &str) -> io::Result<()> {
        self.run(0, what)
    }

    fn run(&self, left: c_int, what: &str) -> io::Result<()> {
        let ml = self.0.ml.as_ptr();
        // SAFETY: ml is live and only driven from this thread, since Conn is not Send.
        let r = unsafe {
            let mut r = pa_mainloop_prepare(ml, left);
            if r >= 0 {
                r = pa_mainloop_poll(ml);
            }
            if r >= 0 {
                r = pa_mainloop_dispatch(ml);
            }
            r
        };
        if r < 0 { Err(error(what, r)) } else { Ok(()) }
    }

    pub fn sinks<F: FnMut(&SinkInfo)>(&self, f: F) -> Option<List<'_, SinkInfo, F>> {
        List::start(self, pa_context_get_sink_info_list, f)
    }

    pub fn sources<F: FnMut(&SourceInfo)>(&self, f: F) -> Option<List<'_, SourceInfo, F>> {
        List::start(self, pa_context_get_source_info_list, f)
    }
}

impl SinkInfo {
    pub fn name(&self) -> Option<String> {
        text(self.name)
    }

    pub fn description(&self) -> Option<String> {
        text(self.description)
    }

    pub fn prop(&self, key: &CStr) -> Option<String> {
        // SAFETY: proplist is valid while the callback that lent us self runs.
        text(unsafe { pa_proplist_gets(self.proplist, key.as_ptr()) })
    }

    pub fn has_prop(&self, key: &CStr) -> Option<bool> {
        // SAFETY: proplist is valid while the callback that lent us self runs.
        match unsafe { pa_proplist_contains(self.proplist, key.as_ptr()) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}

impl SourceInfo {
    pub fn name(&self) -> Option<String> {
        text(self.name)
    }

    pub fn description(&self) -> Option<String> {
        text(self.description)
    }
}

struct Collect<T, F> {
    f: F,
    failed: bool,
    _t: PhantomData<fn(&T)>,
}

extern "C" fn item<T, F: FnMut(&T)>(_: *mut Raw, i: *const T, eol: c_int, userdata: *mut c_void) {
    // SAFETY: userdata is the Collect that the List owning the operation keeps alive until it is done or cancelled.
    let c = unsafe { &mut *userdata.cast::<Collect<T, F>>() };
    if eol < 0 {
        c.failed = true;
    } else if eol == 0 && !i.is_null() {
        // SAFETY: libpulse lends a valid item for the duration of the callback.
        (c.f)(unsafe { &*i });
    }
}

pub struct List<'a, T, F> {
    op: NonNull<Raw>,
    data: NonNull<Collect<T, F>>,
    _conn: PhantomData<&'a Conn>,
}

impl<'a, T, F: FnMut(&T)> List<'a, T, F> {
    fn start(conn: &'a Conn, list: ListFn<T>, f: F) -> Option<Self> {
        let data = NonNull::from(Box::leak(Box::new(Collect { f, failed: false, _t: PhantomData })));
        // SAFETY: ctx is live and data stays allocated until Drop has cancelled or finished the operation.
        match NonNull::new(unsafe { list(conn.ctx(), item::<T, F>, data.as_ptr().cast()) }) {
            Some(op) => Some(Self { op, data, _conn: PhantomData }),
            None => {
                // SAFETY: data came from Box::leak and libpulse never saw it, as no operation started.
                drop(unsafe { Box::from_raw(data.as_ptr()) });
                None
            }
        }
    }
}

impl<T, F> List<'_, T, F> {
    pub fn running(&self) -> bool {
        // SAFETY: op is a live operation reference owned by self.
        unsafe { pa_operation_get_state(self.op.as_ptr()) == OPERATION_RUNNING }
    }

    pub fn failed(&self) -> bool {
        // SAFETY: data is allocated until Drop and the callback only runs inside Conn::iterate, not during this read.
        unsafe { self.data.as_ref().failed }
    }
}

impl<T, F> Drop for List<'_, T, F> {
    fn drop(&mut self) {
        // SAFETY: op and data are owned by self; cancelling first means the callback never sees data after it is freed.
        unsafe {
            if pa_operation_get_state(self.op.as_ptr()) == OPERATION_RUNNING {
                pa_operation_cancel(self.op.as_ptr());
            }
            pa_operation_unref(self.op.as_ptr());
            drop(Box::from_raw(self.data.as_ptr()));
        }
    }
}

pub enum Peek<'a> {
    Empty,
    Hole(usize),
    Data(&'a [u8]),
}

extern "C" fn underflow(_: *mut Raw, userdata: *mut c_void) {
    // SAFETY: userdata is the counter the Stream owns, freed only after its Drop has cleared this callback.
    let n = unsafe { &*userdata.cast::<Cell<u64>>() };
    n.set(n.get().wrapping_add(1));
}

pub struct Stream {
    s: NonNull<Raw>,
    underflows: NonNull<Cell<u64>>,
    conn: Conn,
}

impl Stream {
    pub fn new(conn: &Conn, name: &CStr, spec: &SampleSpec) -> Option<Self> {
        // SAFETY: ctx is live and spec outlives the call; a null map selects the default.
        let s = NonNull::new(unsafe { pa_stream_new(conn.ctx(), name.as_ptr(), spec, null()) })?;
        let underflows = NonNull::from(Box::leak(Box::new(Cell::new(0))));
        Some(Self { s, underflows, conn: conn.clone() })
    }

    fn raw(&self) -> *mut Raw {
        self.s.as_ptr()
    }

    pub fn connect_record(&mut self, dev: &str, attr: &BufferAttr, flags: c_int) -> io::Result<()> {
        let d = cstring(dev)?;
        // SAFETY: the stream is live and d and attr outlive the call.
        status(dev, unsafe { pa_stream_connect_record(self.raw(), d.as_ptr(), attr, flags) })
    }

    pub fn connect_playback(&mut self, dev: &str, attr: &BufferAttr, flags: c_int) -> io::Result<()> {
        let d = cstring(dev)?;
        // SAFETY: the stream is live, d and attr outlive the call, and null volume and sync select the defaults.
        status(dev, unsafe { pa_stream_connect_playback(self.raw(), d.as_ptr(), attr, flags, null(), null_mut()) })
    }

    pub fn live(&self, what: &str) -> io::Result<bool> {
        // SAFETY: the stream is live.
        match unsafe { pa_stream_get_state(self.raw()) } {
            STREAM_READY => Ok(true),
            STREAM_FAILED | STREAM_TERMINATED => Err(self.conn.error(what)),
            _ => Ok(false),
        }
    }

    pub fn device_name(&self) -> Option<String> {
        // SAFETY: the stream is live; the name is copied before the next libpulse call.
        text(unsafe { pa_stream_get_device_name(self.raw()) })
    }

    pub fn peek(&mut self, what: &str) -> io::Result<Peek<'_>> {
        let (mut data, mut n) = (null::<c_void>(), 0usize);
        // SAFETY: the stream is live and both out pointers are valid.
        status(what, unsafe { pa_stream_peek(self.raw(), &mut data, &mut n) })?;
        Ok(match (data.is_null(), n) {
            (true, 0) => Peek::Empty,
            (true, n) => Peek::Hole(n),
            // SAFETY: libpulse keeps n bytes at data until pa_stream_drop, which needs &mut self.
            (false, n) => Peek::Data(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), n) }),
        })
    }

    pub fn discard(&mut self, what: &str) -> io::Result<()> {
        // SAFETY: the stream is live and no Peek borrow is outstanding.
        status(what, unsafe { pa_stream_drop(self.raw()) })
    }

    pub fn readable(&self) -> Option<usize> {
        // SAFETY: the stream is live.
        let n = unsafe { pa_stream_readable_size(self.raw()) };
        (n != usize::MAX).then_some(n)
    }

    pub fn latency_us(&self) -> Option<u64> {
        let (mut usec, mut negative) = (0u64, 0);
        // SAFETY: the stream is live and both out pointers are valid.
        let r = unsafe { pa_stream_get_latency(self.raw(), &mut usec, &mut negative) };
        (r >= 0).then_some(if negative != 0 { 0 } else { usec })
    }

    #[cfg(target_pointer_width = "64")]
    pub fn sink_us(&self) -> Option<u64> {
        // SAFETY: the stream is live; the result is null or its timing info, read before the next libpulse call.
        let t = unsafe { pa_stream_get_timing_info(self.raw()) };
        // SAFETY: a non-null t is valid for this read.
        (!t.is_null()).then(|| unsafe { (*t).sink_usec })
    }

    #[cfg(not(target_pointer_width = "64"))]
    pub fn sink_us(&self) -> Option<u64> {
        None
    }

    pub fn request_timing(&self) {
        // SAFETY: the stream is live and no callback is given, so nothing of ours is lent to the operation.
        let op = unsafe { pa_stream_update_timing_info(self.raw(), None, null_mut()) };
        if !op.is_null() {
            // SAFETY: op is the reference just returned; releasing it leaves the request running.
            unsafe { pa_operation_unref(op) };
        }
    }

    pub fn count_underflows(&mut self) {
        // SAFETY: the stream is live and the counter outlives the registration, as Drop clears it first.
        unsafe { pa_stream_set_underflow_callback(self.raw(), Some(underflow), self.underflows.as_ptr().cast()) };
    }

    pub fn underflows(&self) -> u64 {
        // SAFETY: the counter lives until Drop and the callback only runs inside Conn's mainloop, not during this read.
        unsafe { self.underflows.as_ref().get() }
    }

    pub fn write(&mut self, b: &[u8], what: &str) -> io::Result<()> {
        // SAFETY: the stream is live and libpulse copies b before returning, as no free callback is given.
        status(what, unsafe { pa_stream_write(self.raw(), b.as_ptr().cast(), b.len(), None, 0, SEEK_RELATIVE) })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: self owns the stream and leaked counter, self.conn keeps the context, and the callback goes first.
        unsafe {
            pa_stream_set_underflow_callback(self.raw(), None, null_mut());
            pa_stream_disconnect(self.raw());
            pa_stream_unref(self.raw());
            drop(Box::from_raw(self.underflows.as_ptr()));
        }
    }
}
