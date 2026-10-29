use crate::bus::Source;
use crate::messages::{self, Info};
use crate::settings::{self, Settings};
use crate::telemetry::{Control, Controller, Feed};
use crate::waterfall::Waterfall;
use crate::ws;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};
use xara_host::writer::Writer;

pub const TICK: Duration = Duration::from_millis(100);
pub(crate) const HISTORY: usize = 500;
pub const MAX_PAGES: usize = 8;
pub const MAX_STREAMS: usize = 4;
pub(crate) const MAX_PENDING: usize = 32;
const MAX_REQUEST: usize = 8192;
const MAX_FRAME: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const KEEPALIVE_TICKS: u32 = 20;
pub const WS_IDLE: Duration = Duration::from_secs(6);
pub const TOKEN_FILE: &str = "ui-token";
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const HEADERS: &str = "Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\
    Content-Security-Policy: default-src 'self'; connect-src 'self' ws: wss:; frame-ancestors 'none'; \
    base-uri 'none'; form-action 'none'\r\nReferrer-Policy: no-referrer\r\n";

pub trait Controls: Send + 'static {
    fn control(&mut self, client: u64, c: Control) -> bool;
}

#[derive(Clone, Copy)]
pub struct Pages {
    pub name: &'static str,
    pub index: &'static str,
    pub app: &'static str,
    pub style: &'static str,
}

const COMMON_JS: &str = include_str!("../web/common.js");
const COMMON_CSS: &str = include_str!("../web/common.css");

pub(crate) const MODEM: Pages = Pages {
    name: "xara",
    index: include_str!("../web/index.html"),
    app: include_str!("../web/app.js"),
    style: include_str!("../web/style.css"),
};

pub const MONITOR: Pages = Pages {
    name: "xara-monitor",
    index: include_str!("../web/monitor/index.html"),
    app: include_str!("../web/monitor/app.js"),
    style: include_str!("../web/monitor/style.css"),
};

impl Controls for Controller {
    fn control(&mut self, client: u64, c: Control) -> bool {
        self.send(client, c)
    }
}

#[derive(Clone, Default)]
pub struct Access {
    pub control: bool,
    pub token: Option<String>,
    pub hosts: Vec<String>,
    pub settings: Option<Arc<Settings>>,
}

struct Ctx {
    access: Access,
    cookie: String,
    pages: Pages,
    open: [Arc<AtomicUsize>; 2],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ws,
    Stream,
}

struct Client {
    id: u64,
    w: Arc<Writer>,
    _permit: Permit,
    kind: Kind,
    waterfall: bool,
    role: Role,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    View,
    Locked,
    Control,
}

impl Client {
    fn ping(&self) -> bool {
        match self.kind {
            Kind::Ws => self.w.send(&ws::frame(ws::PING, b"")),
            Kind::Stream => true,
        }
    }

    fn send(&self, msg: &str) -> bool {
        match self.kind {
            Kind::Ws => self.w.send(&ws::frame(ws::TEXT, msg.as_bytes())),
            Kind::Stream => self.w.send(format!("{msg}\n").as_bytes()),
        }
    }

    fn send_binary(&self, b: &[u8]) -> bool {
        match self.kind {
            Kind::Ws => self.w.send(&ws::frame(ws::BINARY, b)),
            Kind::Stream => true,
        }
    }
}

enum Msg {
    Join(Client),
    Leave(u64),
    Waterfall(u64, bool),
    Control(u64, Control),
}

#[derive(Debug, PartialEq)]
enum Cmd<'a> {
    Waterfall(bool),
    Control(Control),
    Settings(Query<'a>),
}

#[derive(Debug, PartialEq)]
enum Query<'a> {
    Get,
    Devices,
    Save(&'a str),
    Reset,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Audio {
    wf: Waterfall,
    wanted: bool,
    rows: Vec<Vec<u8>>,
}

impl Audio {
    fn sync(&mut self, clients: &[Client], src: &mut impl Source) {
        let want = clients.iter().any(|c| c.waterfall);
        if want != self.wanted {
            self.wanted = want;
            src.want_audio(want);
            self.wf.reset();
            src.audio(&mut |_| {});
        }
    }

    fn frames(&mut self, src: &mut impl Source) -> Vec<Vec<u8>> {
        self.rows.clear();
        let (wanted, wf, rows) = (self.wanted, &mut self.wf, &mut self.rows);
        src.audio(&mut |block| {
            if wanted {
                wf.feed(block, rows);
            }
        });
        self.rows.iter().map(|r| messages::waterfall_row(r)).collect()
    }
}

pub struct UiServer {
    pub addr: SocketAddr,
}

impl UiServer {
    pub fn bind(addr: &str, port: u16, feed: Feed, info: Info, access: Access) -> io::Result<Self> {
        let controls: Box<dyn Controls> = Box::new(feed.controller());
        serve(addr, port, feed, Some(controls), info, MODEM, access)
    }

    pub fn bind_readonly(
        addr: &str,
        port: u16,
        src: impl Source,
        info: Info,
        pages: Pages,
        hosts: Vec<String>,
    ) -> io::Result<Self> {
        serve(addr, port, src, None, info, pages, Access { hosts, ..Access::default() })
    }
}

fn serve(
    addr: &str,
    port: u16,
    src: impl Source,
    controls: Option<Box<dyn Controls>>,
    info: Info,
    pages: Pages,
    mut access: Access,
) -> io::Result<UiServer> {
    let l = TcpListener::bind((addr, port))?;
    let local = l.local_addr()?;
    if let Some(h) = hostname() {
        access.hosts.push(h);
    }
    let cookie = format!("xara_token_{}", local.port());
    let open = [Arc::default(), Arc::default()];
    let ctx = Arc::new(Ctx { access, cookie, pages, open });
    let (tx, rx) = channel();
    thread::Builder::new().name("ui-hub".into()).spawn(move || hub(src, controls, info, rx))?;
    thread::Builder::new().name("ui-accept".into()).spawn(move || accept(l, tx, ctx))?;
    Ok(UiServer { addr: local })
}

fn hostname() -> Option<String> {
    let mut b = [0u8; 256];
    // SAFETY: b is valid for its length; gethostname writes at most that many bytes.
    if unsafe { libc::gethostname(b.as_mut_ptr().cast(), b.len()) } != 0 {
        return None;
    }
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let h = std::str::from_utf8(&b[..n]).ok()?.trim();
    (!h.is_empty()).then(|| h.to_string())
}

pub fn new_token() -> io::Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

pub fn valid_token(t: &str) -> bool {
    (8..=128).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn stored_token(dir: &Path) -> io::Result<String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = dir.join(TOKEN_FILE);
    match std::fs::read_to_string(&path) {
        Ok(t) if valid_token(t.trim()) => {
            if std::fs::metadata(&path)?.permissions().mode() & 0o077 != 0 {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            return Ok(t.trim().to_string());
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let t = new_token()?;
    let tmp = dir.join(format!(".{TOKEN_FILE}.tmp"));
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    writeln!(f, "{t}")?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(t)
}

struct Permit(Arc<AtomicUsize>);

impl Permit {
    #[allow(deprecated)]
    fn take(n: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |k| (k < max).then_some(k + 1)).ok()?;
        Some(Self(n.clone()))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept(l: TcpListener, joins: Sender<Msg>, ctx: Arc<Ctx>) {
    let pending = Arc::new(AtomicUsize::new(0));
    loop {
        match l.accept() {
            Ok((s, _)) => {
                let Some(p) = Permit::take(&pending, MAX_PENDING) else { continue };
                let (j, c) = (joins.clone(), ctx.clone());
                if thread::Builder::new().spawn(move || handle(s, p, j, &c)).is_err() {
                    thread::sleep(ACCEPT_BACKOFF);
                }
            }
            Err(_) => thread::sleep(ACCEPT_BACKOFF),
        }
    }
}

fn hub(mut src: impl Source, mut controls: Option<Box<dyn Controls>>, info: Info, msgs: Receiver<Msg>) {
    let mut clients: Vec<Client> = Vec::new();
    let mut history: VecDeque<String> = VecDeque::new();
    let mut audio = Audio { wf: Waterfall::new(info.span_hz), wanted: false, rows: Vec::new() };
    let mut next = Instant::now() + TICK;
    let mut ticks = 0u32;
    loop {
        match msgs.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(Msg::Waterfall(id, on)) => {
                if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                    if on && !c.waterfall {
                        c.send(&messages::waterfall(&audio.wf));
                    }
                    c.waterfall = on;
                }
                audio.sync(&clients, &mut src);
                continue;
            }
            Ok(Msg::Control(id, ctl)) => {
                let what = label(ctl);
                if let Some(c) = clients.iter().find(|c| c.id == id) {
                    match controls.as_mut() {
                        Some(k) if c.role == Role::Control => {
                            if !k.control(id, ctl) {
                                c.send(&messages::refused(&what, "modem busy, try again"));
                            }
                        }
                        _ => {
                            c.send(&messages::refused(&what, "controls are not enabled for this page"));
                        }
                    }
                }
                continue;
            }
            Ok(Msg::Join(c)) => {
                if c.send(&messages::hello(&info, history.len(), c.role == Role::Control, c.role == Role::Locked))
                    && history.iter().all(|m| c.send(m))
                {
                    clients.push(c);
                } else {
                    c.w.close();
                }
                continue;
            }
            Ok(Msg::Leave(id)) => {
                let (gone, kept) = clients.into_iter().partition(|c| c.id == id);
                clients = kept;
                left(gone, &mut controls);
                audio.sync(&clients, &mut src);
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        next += TICK;
        let now = Instant::now();
        if next < now {
            next = now + TICK;
        }
        let mut batch = src.events();
        history.extend(batch.iter().cloned());
        let excess = history.len().saturating_sub(HISTORY);
        history.drain(..excess);
        batch.extend(src.status());
        let rows = audio.frames(&mut src);
        ticks = (ticks + 1) % KEEPALIVE_TICKS;
        let (kept, gone) = clients.into_iter().partition(|c| {
            batch.iter().all(|m| c.send(m))
                && (!c.waterfall || rows.iter().all(|r| c.send_binary(r)))
                && (ticks != 0 || c.ping())
        });
        clients = kept;
        gone.iter().for_each(|c| c.w.close());
        left(gone, &mut controls);
        audio.sync(&clients, &mut src);
    }
}

fn left(gone: Vec<Client>, controls: &mut Option<Box<dyn Controls>>) {
    for c in gone {
        if let Some(k) = controls.as_mut().filter(|_| c.role == Role::Control) {
            k.control(c.id, Control::Left);
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(h, _)| h == k).map(|(_, v)| v.as_str())
    }

    fn param(&self, k: &str) -> Option<&str> {
        self.query.split('&').filter_map(|p| p.split_once('=')).find(|(n, _)| *n == k).map(|(_, v)| v)
    }
}

fn parse_request(head: &[u8]) -> Option<Request> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target, _) = (first.next()?, first.next()?, first.next()?);
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(Request { method: method.into(), path: path.into(), query: query.into(), headers })
}

fn read_request(s: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return parse_request(&buf[..end]);
        }
        if buf.len() > MAX_REQUEST {
            return None;
        }
        let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
        s.set_read_timeout(Some(left)).ok()?;
        let n = s.read(&mut chunk).ok().filter(|&n| n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn handle(mut s: TcpStream, pending: Permit, joins: Sender<Msg>, ctx: &Ctx) {
    let r = read_request(&mut s);
    drop(pending);
    if let Some(r) = r {
        let _ = route(s, &r, &joins, ctx);
    }
}

fn host_allowed(host: &str, names: &[String]) -> bool {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').is_some_and(|(ip, _)| ip.parse::<Ipv6Addr>().is_ok());
    }
    let name = host.rsplit_once(':').map_or(host, |(n, _)| n).trim_end_matches('.').to_ascii_lowercase();
    !name.is_empty()
        && (name.parse::<Ipv4Addr>().is_ok()
            || name == "localhost"
            || name.ends_with(".local")
            || names.iter().any(|a| a.eq_ignore_ascii_case(&name)))
}

fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |x, (p, q)| x | (p ^ q)) == 0
}

fn route(mut s: TcpStream, r: &Request, joins: &Sender<Msg>, ctx: &Ctx) -> io::Result<()> {
    let host = r.header("host").unwrap_or("");
    if !host_allowed(host, &ctx.access.hosts) {
        let body = format!(
            "Host {host:?} is not allowed. Use an IP address, localhost or a .local name, \
             or start {} with --ui-host NAME.\n",
            ctx.pages.name
        );
        return respond(&mut s, "403 Forbidden", "text/plain", body.as_bytes());
    }
    if r.method != "GET" {
        return respond(&mut s, "405 Method Not Allowed", "text/plain", b"GET only\n");
    }
    if !fetch_allowed(r) {
        return respond(&mut s, "403 Forbidden", "text/plain", b"cross-site request refused\n");
    }
    match r.path.as_str() {
        "/" | "/index.html" if r.param("token").is_some() => login(&mut s, r, ctx),
        "/" | "/index.html" => respond(&mut s, "200 OK", "text/html; charset=utf-8", ctx.pages.index.as_bytes()),
        "/app.js" => respond(&mut s, "200 OK", "text/javascript; charset=utf-8", ctx.pages.app.as_bytes()),
        "/style.css" => respond(&mut s, "200 OK", "text/css; charset=utf-8", ctx.pages.style.as_bytes()),
        "/common.js" => respond(&mut s, "200 OK", "text/javascript; charset=utf-8", COMMON_JS.as_bytes()),
        "/common.css" => respond(&mut s, "200 OK", "text/css; charset=utf-8", COMMON_CSS.as_bytes()),
        "/ws" => websocket(s, r, joins, ctx),
        "/stream" => stream(s, joins, ctx),
        _ => respond(&mut s, "404 Not Found", "text/plain", b"not found\n"),
    }
}

fn respond(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> io::Result<()> {
    respond_with(s, status, ctype, "", body)
}

fn respond_with(s: &mut TcpStream, status: &str, ctype: &str, extra: &str, body: &[u8]) -> io::Result<()> {
    write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{HEADERS}{extra}Connection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body)
}

fn token_ok(given: Option<&str>, a: &Access) -> bool {
    a.control && a.token.as_deref().is_some_and(|t| given.is_some_and(|g| same_secret(g, t)))
}

fn cookie<'a>(r: &'a Request, name: &str) -> Option<&'a str> {
    r.header("cookie")?.split(';').filter_map(|c| c.trim().split_once('=')).find(|(n, _)| *n == name).map(|(_, v)| v)
}

fn authorized(r: &Request, ctx: &Ctx) -> bool {
    token_ok(cookie(r, &ctx.cookie), &ctx.access)
}

fn login(s: &mut TcpStream, r: &Request, ctx: &Ctx) -> io::Result<()> {
    let t = r.param("token").unwrap_or_default();
    if !ctx.access.control {
        return respond(s, "403 Forbidden", "text/plain", b"UI controls are not enabled\n");
    }
    if !token_ok(Some(t), &ctx.access) {
        return respond(s, "403 Forbidden", "text/plain", b"wrong token\n");
    }
    let extra =
        format!("Location: {}\r\nSet-Cookie: {}={t}; Path=/; HttpOnly; SameSite=Strict\r\n", r.path, ctx.cookie);
    respond_with(s, "303 See Other", "text/plain", &extra, b"")
}

fn fetch_allowed(r: &Request) -> bool {
    match r.header("sec-fetch-site") {
        None | Some("same-origin") | Some("none") => true,
        Some(_) => r.header("sec-fetch-mode") == Some("navigate") && r.header("sec-fetch-dest") == Some("document"),
    }
}

fn same_origin(r: &Request) -> bool {
    let Some(origin) = r.header("origin") else { return true };
    let Some(host) = r.header("host") else { return false };
    origin.split_once("://").is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

fn join(s: &TcpStream, kind: Kind, role: Role, permit: Permit, joins: &Sender<Msg>) -> io::Result<(u64, Arc<Writer>)> {
    s.set_read_timeout(if kind == Kind::Ws { Some(WS_IDLE) } else { None })?;
    let _ = s.set_nodelay(true);
    let w = Arc::new(Writer::spawn(s)?);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let c = Client { id, w: w.clone(), _permit: permit, kind, waterfall: false, role };
    joins.send(Msg::Join(c)).map_err(|_| io::Error::other("ui hub stopped"))?;
    Ok((id, w))
}

fn command(text: &str) -> Option<Cmd<'_>> {
    let (head, body) = text.split_once('\n').unwrap_or((text, ""));
    Some(match head.trim().to_ascii_uppercase().as_str() {
        "WATERFALL ON" => Cmd::Waterfall(true),
        "WATERFALL OFF" => Cmd::Waterfall(false),
        "TUNE ON" => Cmd::Control(Control::Tune(true)),
        "TUNE OFF" => Cmd::Control(Control::Tune(false)),
        "ABORT" => Cmd::Control(Control::Abort),
        "RESTART" => Cmd::Control(Control::Restart),
        "SETTINGS ?" => Cmd::Settings(Query::Get),
        "DEVICES ?" => Cmd::Settings(Query::Devices),
        "SETTINGS SAVE" => Cmd::Settings(Query::Save(body)),
        "SETTINGS RESET" => Cmd::Settings(Query::Reset),
        l => {
            let db = l.strip_prefix("DRIVE ")?.trim().parse::<f64>().ok().filter(|db| db.is_finite())?;
            Cmd::Control(Control::Drive(db))
        }
    })
}

fn label(c: Control) -> String {
    match c {
        Control::Tune(on) => format!("tune {}", if on { "on" } else { "off" }),
        Control::Drive(db) => format!("drive {db}"),
        Control::Abort => "abort".into(),
        Control::Restart => "restart".into(),
        Control::Left => "left".into(),
    }
}

fn settings_command(q: Query, control: bool, ctx: &Ctx) -> Vec<String> {
    let what = match q {
        Query::Get => "settings",
        Query::Devices => "devices",
        Query::Save(_) => "save",
        Query::Reset => "reset",
    };
    let done = |ok: bool, text: &str| messages::settings_result(what, ok, text);
    if !control {
        return vec![done(false, "controls are not enabled for this page")];
    }
    let Some(s) = &ctx.access.settings else {
        return vec![done(false, "settings are not available")];
    };
    match q {
        Query::Get => vec![messages::settings(s)],
        Query::Devices => vec![s.devices().map_or_else(|e| done(false, &e.to_string()), |d| messages::devices(&d))],
        Query::Save(body) => match settings::parse_save(body).and_then(|p| s.save(&p)) {
            Ok(_) => vec![done(true, "saved; restart to apply"), messages::settings(s)],
            Err(e) => vec![done(false, &e)],
        },
        Query::Reset => match s.reset() {
            Ok(()) => vec![done(true, "saved settings cleared; restart to apply"), messages::settings(s)],
            Err(e) => vec![done(false, &e)],
        },
    }
}

fn websocket(mut s: TcpStream, r: &Request, joins: &Sender<Msg>, ctx: &Ctx) -> io::Result<()> {
    let upgrade = r.header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let (Some(key), true, Some("13")) = (r.header("sec-websocket-key"), upgrade, r.header("sec-websocket-version"))
    else {
        return respond(&mut s, "400 Bad Request", "text/plain", b"websocket upgrade required\n");
    };
    if !same_origin(r) {
        return respond(&mut s, "403 Forbidden", "text/plain", b"cross-origin websocket refused\n");
    }
    let control = authorized(r, ctx);
    let Some(permit) = Permit::take(&ctx.open[0], MAX_PAGES - usize::from(!control)) else {
        return respond(&mut s, "503 Service Unavailable", "text/plain", b"too many pages open\n");
    };
    write!(
        s,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        ws::accept_key(key)
    )?;
    let role = match (control, ctx.access.control) {
        (true, _) => Role::Control,
        (false, true) => Role::Locked,
        (false, false) => Role::View,
    };
    let (id, w) = join(&s, Kind::Ws, role, permit, joins)?;
    let mut rd = ws::Reader::default();
    let shut = |code: u16| w.send(&ws::frame(ws::CLOSE, &code.to_be_bytes()));
    let closed = loop {
        let f = match rd.next(&mut s, MAX_FRAME) {
            Ok(f) => f,
            Err(ws::Fail::Close(code)) => break shut(code),
            Err(ws::Fail::Io) => break false,
        };
        match f.op {
            ws::TEXT => {
                let Ok(text) = std::str::from_utf8(&f.payload) else { break shut(1007) };
                let msg = match command(text) {
                    Some(Cmd::Waterfall(on)) => Msg::Waterfall(id, on),
                    Some(Cmd::Control(c)) => Msg::Control(id, c),
                    Some(Cmd::Settings(q)) => {
                        for m in settings_command(q, control, ctx) {
                            w.send(&ws::frame(ws::TEXT, m.as_bytes()));
                        }
                        continue;
                    }
                    None => continue,
                };
                if joins.send(msg).is_err() {
                    break false;
                }
            }
            ws::PING => {
                w.send(&ws::frame(ws::PONG, &f.payload));
            }
            ws::CLOSE => break w.send(&ws::frame(ws::CLOSE, &f.payload[..f.payload.len().min(2)])),
            _ => {}
        }
    };
    let _ = joins.send(Msg::Leave(id));
    if closed {
        w.finish()
    } else {
        w.close()
    }
    Ok(())
}

fn stream(mut s: TcpStream, joins: &Sender<Msg>, ctx: &Ctx) -> io::Result<()> {
    let Some(permit) = Permit::take(&ctx.open[1], MAX_STREAMS) else {
        return respond(&mut s, "503 Service Unavailable", "text/plain", b"too many streams open\n");
    };
    write!(
        s,
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    )?;
    join(&s, Kind::Stream, Role::View, permit, joins).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(head: &str) -> Request {
        parse_request(head.as_bytes()).unwrap()
    }

    #[test]
    fn requests() {
        let r = req("GET /ws?x=1 HTTP/1.1\r\nHost: pi.local:8080\r\nSec-WebSocket-Key:  abc \r\nbad line");
        assert_eq!((r.method.as_str(), r.path.as_str()), ("GET", "/ws"));
        assert_eq!((r.header("host"), r.header("sec-websocket-key")), (Some("pi.local:8080"), Some("abc")));
        assert!(parse_request(b"GET /\r\n").is_none());
    }

    #[test]
    fn commands() {
        assert_eq!(command("WATERFALL ON"), Some(Cmd::Waterfall(true)));
        assert_eq!(command(" waterfall off\r\n"), Some(Cmd::Waterfall(false)));
        assert_eq!(command("TUNE ON"), Some(Cmd::Control(Control::Tune(true))));
        assert_eq!(command("tune off"), Some(Cmd::Control(Control::Tune(false))));
        assert_eq!(command("ABORT"), Some(Cmd::Control(Control::Abort)));
        assert_eq!(command("RESTART"), Some(Cmd::Control(Control::Restart)));
        assert_eq!(command("DRIVE -12.5"), Some(Cmd::Control(Control::Drive(-12.5))));
        assert_eq!(command("SETTINGS ?"), Some(Cmd::Settings(Query::Get)));
        assert_eq!(command("SETTINGS SAVE\nMyCall = a\nb"), Some(Cmd::Settings(Query::Save("MyCall = a\nb"))));
        assert_eq!(label(Control::Drive(-12.5)), "drive -12.5");
        for bad in ["DRIVE", "DRIVE x", "DRIVE NaN", "DRIVE inf", "TUNE", "MYCALL N0CALL"] {
            assert_eq!(command(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hosts() {
        let names = vec!["shack-pi".to_string(), "pi.home.arpa".to_string()];
        for ok in [
            "127.0.0.1:8080",
            "192.168.1.20",
            "[::1]:8080",
            "[fe80::1]",
            "localhost:8080",
            "LOCALHOST",
            "shack-pi:8080",
            "Shack-Pi",
            "pi.home.arpa.",
            "raspberrypi.local:8080",
        ] {
            assert!(host_allowed(ok, &names), "{ok}");
        }
        for bad in [
            "",
            ":8080",
            "evil.example:8080",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "local",
            "[not-an-ip]",
            "shack-pi.evil.example",
        ] {
            assert!(!host_allowed(bad, &names), "{bad}");
        }
    }

    #[test]
    fn tokens() {
        assert!(same_secret("abc", "abc") && !same_secret("abc", "abd") && !same_secret("abc", "abcd"));
        assert!(valid_token("0123456789abcdef") && !valid_token("short") && !valid_token("has space 1234"));
        let t = new_token().unwrap();
        assert!(valid_token(&t) && t.len() == 32 && t != new_token().unwrap());
        assert_eq!(req("GET /ws?a=1&token=xyz HTTP/1.1").param("token"), Some("xyz"));
    }

    #[test]
    fn origins() {
        let h = "GET /ws HTTP/1.1\r\nHost: Pi.local:8080";
        assert!(same_origin(&req(h)));
        assert!(same_origin(&req(&format!("{h}\r\nOrigin: http://pi.local:8080"))));
        assert!(!same_origin(&req(&format!("{h}\r\nOrigin: http://evil.example"))));
        assert!(!same_origin(&req(&format!("{h}\r\nOrigin: null"))));
        assert!(!same_origin(&req("GET /ws HTTP/1.1\r\nOrigin: http://pi.local:8080")));
    }

    #[test]
    fn fetch_metadata() {
        let h = "GET /stream HTTP/1.1\r\nHost: 127.0.0.1:8080";
        for ok in ["", "\r\nSec-Fetch-Site: same-origin", "\r\nSec-Fetch-Site: none"] {
            assert!(fetch_allowed(&req(&format!("{h}{ok}"))), "{ok:?}");
        }
        let nav = "\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document";
        assert!(fetch_allowed(&req(&format!("{h}{nav}"))));
        for bad in [
            "\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\nSec-Fetch-Dest: image",
            "\r\nSec-Fetch-Site: same-site\r\nSec-Fetch-Mode: cors\r\nSec-Fetch-Dest: empty",
            "\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: iframe",
        ] {
            assert!(!fetch_allowed(&req(&format!("{h}{bad}"))), "{bad:?}");
        }
    }

    #[test]
    fn pending_requests_are_capped() {
        let n = Arc::new(AtomicUsize::new(0));
        let held: Vec<Permit> = (0..MAX_PENDING).map_while(|_| Permit::take(&n, MAX_PENDING)).collect();
        assert_eq!(held.len(), MAX_PENDING);
        assert!(Permit::take(&n, MAX_PENDING).is_none());
        drop(held);
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(Permit::take(&n, MAX_PENDING).is_some());
    }
}
