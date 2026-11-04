//! The phone panel: a plain HTTP server inside this process, because what it
//! serves — agent status — only exists here (see `docs/web-panel.md`).
//!
//! One thread accepts and answers; every question that needs app state is sent
//! to the GPUI main thread over `async_channel` and answered there, because the
//! terminals are `Rc` and cannot leave it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use serde_json::Value;
use syro_core::api;
use syro_core::config::WebSettings;
use syro_core::term::Subscription;

/// The one page, compiled in: no build step, no files to deploy beside the app.
const PAGE: &str = include_str!("web/index.html");
/// Where the app's language is baked into the page. Replaced by [`page`].
const LOCALE_SLOT: &str = "\"__syro_locale__\"";
/// xterm.js, vendored (MIT, license in the file). 291 KiB, and the reason the
/// panel's full terminal is opt-in rather than what loads by default.
const XTERM_JS: &str = include_str!("web/xterm.js");
const XTERM_CSS: &str = include_str!("web/xterm.css");
/// `@xterm/addon-fit`, so the full terminal asks the child for the size the
/// phone's screen actually has.
const XTERM_FIT_JS: &str = include_str!("web/xterm-addon-fit.js");
/// The app's icon, for a phone that pins the panel to its home screen.
const ICON: &[u8] = include_bytes!("../assets/logo.png");

const HTML: &str = "text/html; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";
const JSON: &str = "application/json";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const EVENTS: &str = "text/event-stream";
const PNG: &str = "image/png";

/// How often a stream with nothing to say says so anyway: a comment keeps the
/// connection warm, and the write that fails is how a page that left is found.
const PING: Duration = Duration::from_secs(10);
/// How often the state stream looks for a change. This is a hop to the app's own
/// thread, not a request over the network: the phone is woken only when the answer
/// differs from the last one it was sent, which is the whole point of the stream.
const STATE_TICK: Duration = Duration::from_secs(2);
/// And how often that stream pings while nothing changes, so a page that left is
/// found even on a quiet afternoon.
const STATE_PING_TICKS: u32 = 15;

/// A request body larger than this is refused rather than read: a page sends
/// keystrokes and one worktree name, not a file.
const MAX_BODY: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
/// How long the page waits for the app to answer before it gives up.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

/// What the page asked for. Answered on the main thread.
#[derive(Debug)]
pub struct WebRequest {
    pub action: Action,
    /// The HTTP thread blocks on this; one reply per request.
    pub reply: std::sync::mpsc::Sender<Result<Payload, String>>,
}

#[derive(Debug)]
pub enum Action {
    /// Every worktree, its agent status, and its terminals.
    State,
    /// One terminal's visible text.
    Snapshot {
        terminal: String,
    },
    /// Bytes to the child, exactly as typed keys.
    Input {
        terminal: String,
        bytes: Vec<u8>,
    },
    Resize {
        terminal: String,
        cols: usize,
        rows: usize,
    },
    /// `repo` is an id or a name; `name` is the new worktree's.
    CreateWorktree {
        repo: String,
        name: String,
        base: Option<String>,
    },
    RemoveWorktree {
        id: String,
        force: bool,
    },
    /// The presets the page may start, for its own picker.
    Agents,
    /// Open a terminal tab in a worktree, with one of those presets or a plain
    /// shell.
    StartTerminal {
        worktree: String,
        agent: Option<String>,
    },
    /// A live view of one terminal: the screen now, then every byte after it.
    Stream {
        terminal: String,
    },
}

/// A successful answer. What it is decides the content type.
#[derive(Debug)]
pub enum Payload {
    Json(Value),
    Text(String),
    /// Done, with nothing to say.
    Ok,
    /// A terminal's screen and its byte stream, for the one route that answers
    /// with a connection rather than a body.
    Stream {
        text: String,
        bytes: Subscription,
    },
}

/// What one request gets: a body, or a stream that outlives the request.
enum Answer {
    Once(Outcome),
    Stream {
        cookie: Option<String>,
        source: StreamSource,
    },
}

/// What a stream is carrying.
enum StreamSource {
    /// One terminal: the screen now, then every byte after it.
    Terminal {
        terminal: String,
        text: String,
        bytes: Subscription,
    },
    /// Every worktree, re-sent whenever it changes.
    State,
}

/// Bind the panel and serve it. `Ok(url)` is the address to open, token and all.
///
/// The caller keeps `requests` and answers them; this only listens. `token` is
/// shared so the settings form can replace it while the panel runs.
pub fn start(
    settings: &WebSettings,
    token: Arc<Mutex<String>>,
    requests: async_channel::Sender<WebRequest>,
) -> anyhow::Result<String> {
    let ip: IpAddr = settings
        .bind
        .parse()
        .with_context(|| format!("[web] bind is not an IP address: {}", settings.bind))?;
    let listener = TcpListener::bind(SocketAddr::new(ip, settings.port)).with_context(|| {
        format!(
            "binding the web panel to {}:{}",
            settings.bind, settings.port
        )
    })?;
    // Read back rather than reuse the request: port 0 means "any free one", and
    // the URL is what the user has to open.
    let mut addr = listener.local_addr()?;
    // `0.0.0.0` listens everywhere but opens nowhere: show the address a phone
    // on the same network would reach instead.
    if addr.ip().is_unspecified() {
        addr.set_ip(outward_ip("192.0.2.1:9").unwrap_or(IpAddr::from([127, 0, 0, 1])));
    }
    let url = format!("http://{addr}/?token={}", token.lock().unwrap());
    std::thread::Builder::new()
        .name("syro-web".to_string())
        .spawn(move || {
            // Sequential on purpose: a phone polls one page, and every request
            // hops to the same main thread anyway, so a second thread here buys
            // nothing and costs a way to starve the UI.
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let token = token.lock().unwrap().clone();
                        handle(stream, &token, &requests)
                    }
                    Err(error) => tracing::debug!(%error, "the web panel refused a connection"),
                }
            }
        })
        .context("starting the web panel thread")?;
    Ok(url)
}

/// The address this machine would send to `target` from. A UDP `connect` sends
/// nothing; it only asks the OS which interface it would use.
fn outward_ip(target: &str) -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(target).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// The panel's URL on this Mac's Tailscale address, for a phone away from home.
/// `None` when Tailscale is not up, or the panel is not bound to every
/// interface and so does not listen there. Cheap enough to ask on every render,
/// so turning Tailscale on shows up without a restart.
pub fn tailscale_url(local: &str, bind: &str) -> Option<String> {
    if !bind.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified()) {
        return None;
    }
    // 100.100.100.100 is Tailscale's own resolver: only its tunnel routes there.
    let ip = outward_ip("100.100.100.100:53").filter(is_tailscale)?;
    rehost(local, ip)
}

/// Tailscale hands out addresses from the CGNAT range, 100.64.0.0/10.
fn is_tailscale(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 100 && v4.octets()[1] & 0xC0 == 64)
}

/// `url` with its host swapped for `ip`, port and token kept.
fn rehost(url: &str, ip: IpAddr) -> Option<String> {
    let (authority, rest) = url.strip_prefix("http://")?.split_once('/')?;
    let mut addr: SocketAddr = authority.parse().ok()?;
    addr.set_ip(ip);
    Some(format!("http://{addr}/{rest}"))
}

/// The panel's token, read from `path` or made and written there, so a
/// bookmarked URL keeps working across restarts. Called with
/// `~/.config/syro/web-token` by the app and with a scratch file by a test.
pub fn token_at(path: &std::path::Path) -> anyhow::Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let text = text.trim().to_string();
        if !text.is_empty() {
            return Ok(text);
        }
    }
    let token = random_hex();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, &token).with_context(|| format!("writing {}", path.display()))?;
    restrict(path);
    Ok(token)
}

/// Throw the token at `path` away for a new one, so every link and every
/// device holding the old one is locked out.
pub fn regenerate_token(path: &std::path::Path) -> anyhow::Result<String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("removing {}", path.display())),
    }
    token_at(path)
}

pub(crate) fn token_path() -> PathBuf {
    syro_core::config_dir().join("web-token")
}

/// Owner-only, because the token is the whole of the panel's authentication.
#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {}

/// 32 hex characters, from the same randomly keyed hasher `new_session_id` uses:
/// enough for a token nobody guesses at, and no dependency.
fn random_hex() -> String {
    use std::hash::{BuildHasher as _, RandomState};
    let now = std::time::SystemTime::now();
    let a = RandomState::new().hash_one(now);
    let b = RandomState::new().hash_one((now, std::process::id()));
    format!("{a:016x}{b:016x}")
}

fn handle(mut stream: TcpStream, token: &str, requests: &async_channel::Sender<WebRequest>) {
    match read_request(&mut stream, token, requests) {
        Ok(Answer::Once(outcome)) => {
            if let Err(error) = write_response(&mut stream, outcome) {
                tracing::debug!(%error, "the web panel could not answer");
            }
        }
        // Its own thread: the accept loop has to keep taking requests while one
        // connection streams, and a stream is held open by design.
        Ok(Answer::Stream {
            cookie,
            source:
                StreamSource::Terminal {
                    terminal,
                    text,
                    bytes,
                },
        }) => {
            // Cloned, because a stream that fell behind has to ask the app for
            // the screen again.
            let requests = requests.clone();
            let spawned = std::thread::Builder::new()
                .name("syro-web-stream".to_string())
                .spawn(move || {
                    if let Err(error) =
                        serve_stream(stream, cookie, terminal, text, bytes, &requests)
                    {
                        tracing::debug!(%error, "a web terminal stream ended");
                    }
                });
            if let Err(error) = spawned {
                tracing::warn!(%error, "the web panel could not start a stream thread");
            }
        }
        Ok(Answer::Stream {
            cookie,
            source: StreamSource::State,
        }) => {
            let requests = requests.clone();
            let spawned = std::thread::Builder::new()
                .name("syro-web-state".to_string())
                .spawn(move || {
                    if let Err(error) = serve_state_stream(stream, cookie, &requests) {
                        tracing::debug!(%error, "a web state stream ended");
                    }
                });
            if let Err(error) = spawned {
                tracing::warn!(%error, "the web panel could not start a state stream thread");
            }
        }
        Err(error) => {
            tracing::debug!(%error, "the web panel could not read a request");
            let _ = write_response(&mut stream, error_outcome(400, "bad request"));
        }
    }
}

fn read_request(
    stream: &mut TcpStream,
    token: &str,
    requests: &async_channel::Sender<WebRequest>,
) -> anyhow::Result<Answer> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };

    // Headers. `?token=` is how the URL someone bookmarked gets in; the cookie
    // it sets is how every fetch after that one gets in.
    let query_token = query_param(&query, "token");
    let mut presented = query_token.clone();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap_or(0),
            "authorization" => {
                if let Some(bearer) = value.strip_prefix("Bearer ") {
                    presented = Some(bearer.to_string());
                }
            }
            "cookie" => {
                if let Some(cookie) = cookie_token(value) {
                    presented = Some(cookie);
                }
            }
            _ => {}
        }
    }

    if presented.as_deref() != Some(token) {
        return Ok(Answer::Once(error_outcome(
            401,
            "the token is missing or wrong",
        )));
    }
    if length > MAX_BODY {
        return Ok(Answer::Once(error_outcome(413, "the body is too large")));
    }

    let mut body = Vec::new();
    if length > 0 {
        let mut buffer = vec![0u8; length];
        reader.read_exact(&mut buffer)?;
        body = buffer;
    }

    // Set only off a matching query token: a bad one must not be handed back as
    // a cookie.
    let cookie = query_token
        .filter(|given| given.as_str() == token)
        .map(|_| format!("syro_token={token}; HttpOnly; SameSite=Strict; Path=/"));

    // The two routes that answer with a stream instead of a body, and so need the
    // request to be split rather than turned into an `Outcome`.
    let parts = segments(&path);
    if method == "GET"
        && let ["api", "state", "stream"] = parts.as_slice()
    {
        return Ok(Answer::Stream {
            cookie,
            source: StreamSource::State,
        });
    }
    if method == "GET"
        && let ["api", "terminal", id, "stream"] = parts.as_slice()
    {
        let asked = ask(
            requests,
            Action::Stream {
                terminal: (*id).to_string(),
            },
        );
        return Ok(match asked {
            Ok(Payload::Stream { text, bytes }) => Answer::Stream {
                cookie,
                source: StreamSource::Terminal {
                    terminal: (*id).to_string(),
                    text,
                    bytes,
                },
            },
            Ok(_) => Answer::Once(error_outcome(500, "the app answered the wrong shape")),
            Err(outcome) => Answer::Once(outcome),
        });
    }

    let mut outcome = route(&method, &path, &body, requests);
    outcome.cookie = cookie;
    Ok(Answer::Once(outcome))
}

/// The page as it is served: the same HTML with the app's language in it. The
/// panel is one page for every reader, and the app is the one that knows which
/// language its user chose.
fn page() -> String {
    page_for(&rust_i18n::locale())
}

/// [`page`], with the language named outright — the seam a test can hold.
fn page_for(locale: &str) -> String {
    PAGE.replace(LOCALE_SLOT, &format!("\"{locale}\""))
}

/// The request path's parts, without the empty ones a leading or trailing slash
/// leaves behind.
fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}

/// Write the head both streams share: no length, because an event stream ends
/// when the connection does.
fn write_stream_head(stream: &mut TcpStream, cookie: Option<String>) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {EVENTS}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: keep-alive\r\n"
    );
    if let Some(cookie) = cookie {
        head.push_str(&format!("Set-Cookie: {cookie}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

/// Every worktree's state, sent when it changes and never otherwise.
///
/// A poll would be simpler and it is what this replaced: the page asked every two
/// seconds, which wakes a phone's radio every two seconds to be told nothing. The
/// looking still happens every two seconds — but in this process, and the phone
/// only hears about it when the answer is different.
fn serve_state_stream(
    mut stream: TcpStream,
    cookie: Option<String>,
    requests: &async_channel::Sender<WebRequest>,
) -> std::io::Result<()> {
    stream.set_write_timeout(Some(TIMEOUT))?;
    write_stream_head(&mut stream, cookie)?;
    let mut sent: Option<String> = None;
    let mut ticks: u32 = 0;
    loop {
        ticks = ticks.wrapping_add(1);
        if let Ok(Payload::Json(value)) = ask(requests, Action::State)
            && let Ok(body) = serde_json::to_string(&value)
            && sent.as_deref() != Some(body.as_str())
        {
            // One line by construction: `serde_json`'s compact form escapes what
            // a line-based protocol cannot carry.
            let body = body.clone();
            sent = Some(body.clone());
            event(&mut stream, "state", &body)?;
        }
        if ticks.is_multiple_of(STATE_PING_TICKS) {
            stream.write_all(b": ping\n\n")?;
            stream.flush()?;
        }
        std::thread::sleep(STATE_TICK);
    }
}

/// One terminal's bytes, as Server-Sent Events, for as long as the page listens.
///
/// SSE rather than a WebSocket because the other direction is already a POST:
/// this is one-way, and a browser has `EventSource` built in.
fn serve_stream(
    mut stream: TcpStream,
    cookie: Option<String>,
    terminal: String,
    text: String,
    bytes: Subscription,
    requests: &async_channel::Sender<WebRequest>,
) -> std::io::Result<()> {
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {EVENTS}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: keep-alive\r\n"
    );
    if let Some(cookie) = cookie {
        head.push_str(&format!("Set-Cookie: {cookie}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;

    // The screen first, as a paint. The subscription was taken before the screen
    // was read, so what it already caught up on is dropped rather than replayed
    // over the newer screen.
    bytes.drain();
    paint(&mut stream, &text)?;

    loop {
        match bytes.recv_timeout(PING) {
            Ok(chunk) => event(&mut stream, "bytes", &hex(&chunk))?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // A comment rather than an event: it says nothing to the page,
                // and the write that fails is how a page that left is found.
                stream.write_all(b": ping\n\n")?;
                stream.flush()?;
            }
            // The sender went with the terminal: it is gone, and the page is
            // told rather than left with an empty box.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // A reader that fell behind missed bytes with no way to know which, so
        // its screen is wrong from there on. The grid is the record: repaint it
        // and let the page carry on from a screen it can trust.
        if bytes.lagged()
            && let Ok(Payload::Text(text)) = ask(
                requests,
                Action::Snapshot {
                    terminal: terminal.clone(),
                },
            )
        {
            paint(&mut stream, &text)?;
        }
    }
    let _ = stream.write_all(b"event: end\ndata: 0\n\n");
    let _ = stream.flush();
    Ok(())
}

/// Send the screen to be drawn: cleared first, because a repaint must not leave
/// what the last one drew underneath it.
fn paint(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    let mut cleared = b"\x1b[2J\x1b[H\x1b[0m".to_vec();
    cleared.extend_from_slice(text.as_bytes());
    event(stream, "paint", &hex(&cleared))
}

/// One named event, whose payload is hex — the one encoding that survives the
/// line-based protocol without the panel having to think about UTF-8, and that
/// the page can hand straight to `xterm.js` as bytes.
fn event(stream: &mut TcpStream, name: &str, data: &str) -> std::io::Result<()> {
    stream.write_all(format!("event: {name}\ndata: {data}\n\n").as_bytes())?;
    stream.flush()
}

/// Lowercase hex, dependency-free. A chunk boundary is allowed to fall inside a
/// character: the page hands the bytes to `xterm.js`, which buffers UTF-8 itself.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xF) as usize] as char);
    }
    out
}

/// The page's side of [`hex`], so a test can prove the encoding round-trips.
#[cfg(test)]
fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

fn route(
    method: &str,
    path: &str,
    body: &[u8],
    requests: &async_channel::Sender<WebRequest>,
) -> Outcome {
    let parts = segments(path);
    match (method, parts.as_slice()) {
        ("GET", []) => Outcome::new(200, HTML, page().into_bytes()),
        // The full terminal's library, only fetched by the page that asks for it.
        ("GET", ["xterm.js"]) => Outcome::new(200, JS, XTERM_JS.as_bytes().to_vec()),
        ("GET", ["xterm-addon-fit.js"]) => Outcome::new(200, JS, XTERM_FIT_JS.as_bytes().to_vec()),
        ("GET", ["xterm.css"]) => Outcome::new(200, CSS, XTERM_CSS.as_bytes().to_vec()),
        ("GET", ["icon.png"]) => Outcome::new(200, PNG, ICON.to_vec()),
        ("GET", ["api", "state"]) => answered(ask(requests, Action::State)),
        ("GET", ["api", "agents"]) => answered(ask(requests, Action::Agents)),
        ("GET", ["api", "terminal", id, "snapshot"]) => answered(ask(
            requests,
            Action::Snapshot {
                terminal: (*id).to_string(),
            },
        )),
        ("POST", ["api", "terminal", id, "input"]) => {
            let Some(value) = json_body(body) else {
                return error_outcome(400, "the body is not JSON");
            };
            let bytes = if let Some(text) = value.get("text").and_then(Value::as_str) {
                text.as_bytes().to_vec()
            } else if let Some(keys) = value.get("keys").and_then(Value::as_array) {
                let Some(names) = keys
                    .iter()
                    .map(|key| key.as_str().map(str::to_string))
                    .collect::<Option<Vec<String>>>()
                else {
                    return error_outcome(400, "keys must be names");
                };
                match api::keys_to_bytes(&names) {
                    Some(bytes) => bytes,
                    None => return error_outcome(400, "a key name is not known"),
                }
            } else {
                return error_outcome(400, "send `text` or `keys`");
            };
            answered(ask(
                requests,
                Action::Input {
                    terminal: (*id).to_string(),
                    bytes,
                },
            ))
        }
        ("POST", ["api", "terminal", id, "resize"]) => {
            let Some(value) = json_body(body) else {
                return error_outcome(400, "the body is not JSON");
            };
            let (Some(cols), Some(rows)) = (
                value.get("cols").and_then(Value::as_u64),
                value.get("rows").and_then(Value::as_u64),
            ) else {
                return error_outcome(400, "cols and rows are required");
            };
            answered(ask(
                requests,
                Action::Resize {
                    terminal: (*id).to_string(),
                    cols: cols as usize,
                    rows: rows as usize,
                },
            ))
        }
        ("POST", ["api", "worktrees"]) => {
            let Some(value) = json_body(body) else {
                return error_outcome(400, "the body is not JSON");
            };
            let (Some(repo), Some(name)) = (
                value.get("repo").and_then(Value::as_str),
                value.get("name").and_then(Value::as_str),
            ) else {
                return error_outcome(400, "repo and name are required");
            };
            answered(ask(
                requests,
                Action::CreateWorktree {
                    repo: repo.to_string(),
                    name: name.to_string(),
                    base: value
                        .get("base")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
            ))
        }
        ("POST", ["api", "worktrees", "remove"]) => {
            let Some(value) = json_body(body) else {
                return error_outcome(400, "the body is not JSON");
            };
            let Some(id) = value.get("id").and_then(Value::as_str) else {
                return error_outcome(400, "id is required");
            };
            answered(ask(
                requests,
                Action::RemoveWorktree {
                    id: id.to_string(),
                    force: value.get("force").and_then(Value::as_bool).unwrap_or(false),
                },
            ))
        }
        ("POST", ["api", "terminals"]) => {
            let Some(value) = json_body(body) else {
                return error_outcome(400, "the body is not JSON");
            };
            let Some(worktree) = value.get("worktree").and_then(Value::as_str) else {
                return error_outcome(400, "worktree is required");
            };
            answered(ask(
                requests,
                Action::StartTerminal {
                    worktree: worktree.to_string(),
                    agent: value
                        .get("agent")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
            ))
        }
        _ => error_outcome(404, "no such route"),
    }
}

/// Ask the main thread and wait for it. The thread blocks, which is what a
/// sequential accept loop expects.
fn ask(requests: &async_channel::Sender<WebRequest>, action: Action) -> Result<Payload, Outcome> {
    let (reply, answer) = std::sync::mpsc::channel();
    requests
        .send_blocking(WebRequest { action, reply })
        .map_err(|_| error_outcome(503, "the app is not listening for panel requests"))?;
    match answer.recv_timeout(ANSWER_TIMEOUT) {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(reason)) => Err(error_outcome(400, &reason)),
        Err(_) => Err(error_outcome(503, "the app did not answer")),
    }
}

fn answered(result: Result<Payload, Outcome>) -> Outcome {
    match result {
        Ok(Payload::Json(value)) => match serde_json::to_vec(&value) {
            Ok(body) => Outcome::new(200, JSON, body),
            Err(error) => error_outcome(500, &format!("could not encode the answer: {error}")),
        },
        Ok(Payload::Text(text)) => Outcome::new(200, TEXT, text.into_bytes()),
        Ok(Payload::Ok) => Outcome::new(200, TEXT, b"ok".to_vec()),
        // The stream route is answered before `route` runs, so this arm is only
        // reachable if one is ever added there.
        Ok(Payload::Stream { .. }) => {
            error_outcome(500, "a stream was answered where a body was expected")
        }
        Err(outcome) => outcome,
    }
}

fn json_body(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

fn cookie_token(header: &str) -> Option<String> {
    header
        .split(';')
        .find_map(|part| part.trim().strip_prefix("syro_token=").map(str::to_string))
}

struct Outcome {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    cookie: Option<String>,
}

impl Outcome {
    fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body,
            cookie: None,
        }
    }
}

fn error_outcome(status: u16, message: &str) -> Outcome {
    Outcome::new(status, TEXT, message.as_bytes().to_vec())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        _ => "Service Unavailable",
    }
}

fn write_response(stream: &mut TcpStream, outcome: Outcome) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n",
        outcome.status,
        reason(outcome.status),
        outcome.content_type,
        outcome.body.len(),
    );
    if let Some(cookie) = outcome.cookie {
        head.push_str(&format!("Set-Cookie: {cookie}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&outcome.body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One request, through a real socket, answered by a fake main thread.
    /// `token` is `'static` because the handling thread outlives this call.
    /// The GPUI main thread, faked: every action answered with a marker the
    /// assertions can read.
    fn fake_main(action: Action) -> Payload {
        match action {
            Action::State => Payload::Json(serde_json::json!({"worktrees": []})),
            Action::Snapshot { terminal } => Payload::Text(format!("text of {terminal}")),
            Action::Input { terminal, bytes } => {
                Payload::Text(format!("{terminal} <- {}", String::from_utf8_lossy(&bytes)))
            }
            Action::Agents => Payload::Json(serde_json::json!([{
                "id": "claude",
                "name": "Claude Code",
            }])),
            _ => Payload::Ok,
        }
    }

    /// One request, through a real socket, answered by `main` standing in for the
    /// GPUI thread. `token` is `'static` because the handling thread outlives this
    /// call.
    fn round_trip_with(
        request: &str,
        token: &'static str,
        main: impl Fn(Action) -> Payload + Send + 'static,
    ) -> String {
        round_trip_until(request, token, main, |_| false)
    }

    /// The same, reading only until `done` says so — a stream does not end on its
    /// own, so something has to stop the read.
    fn round_trip_until(
        request: &str,
        token: &'static str,
        main: impl Fn(Action) -> Payload + Send + 'static,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (requests, answers) = async_channel::bounded::<WebRequest>(4);
        std::thread::spawn(move || {
            while let Ok(request) = answers.recv_blocking() {
                let payload = main(request.action);
                let _ = request.reply.send(Ok(payload));
            }
        });
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        let handler = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(stream, token, &requests);
        });
        client.write_all(request.as_bytes()).unwrap();
        // A plain answer ends with the connection close; a stream has to be cut
        // off by `done` or by the deadline.
        client
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let mut response = Vec::new();
        let mut buffer = [0u8; 8192];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match client.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    response.extend_from_slice(&buffer[..read]);
                    if done(&String::from_utf8_lossy(&response)) {
                        break;
                    }
                }
                // Timed out with nothing yet: the stream may still be thinking.
                Err(_) => {}
            }
            if std::time::Instant::now() > deadline {
                break;
            }
        }
        handler.join().unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    /// One request, through a real socket, answered by the default fake.
    fn round_trip(request: &str, token: &'static str) -> String {
        round_trip_with(request, token, fake_main)
    }

    /// A GET with the token as the cookie the first page load leaves behind.
    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: syro_token=abc\r\n\r\n")
    }

    /// A POST with that cookie, a JSON body and its length.
    fn post(path: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: syro_token=abc\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    const TOKEN: &str = "abc";

    #[test]
    fn binding_everywhere_shows_an_address_a_phone_can_open() {
        let settings = WebSettings {
            bind: "0.0.0.0".to_string(),
            port: 0,
            ..WebSettings::default()
        };
        let (requests, _keep) = async_channel::unbounded();
        let url = start(&settings, Arc::new(Mutex::new("t".to_string())), requests).unwrap();
        assert!(!url.contains("0.0.0.0"), "{url}");
    }

    #[test]
    fn the_tailscale_link_keeps_the_port_and_token() {
        let ip: IpAddr = "100.101.102.103".parse().unwrap();
        assert!(is_tailscale(&ip));
        assert!(!is_tailscale(&"192.168.31.54".parse().unwrap()));
        assert!(!is_tailscale(&"100.128.0.1".parse().unwrap()));
        assert_eq!(
            rehost("http://192.168.31.54:8777/?token=abc", ip).as_deref(),
            Some("http://100.101.102.103:8777/?token=abc")
        );
        // Loopback does not listen on the tunnel, so it gets no remote link.
        assert!(tailscale_url("http://127.0.0.1:8777/?token=abc", "127.0.0.1").is_none());
    }

    #[test]
    fn the_page_and_the_state_need_the_token() {
        let served = round_trip(&get("/"), TOKEN);
        assert!(served.starts_with("HTTP/1.1 200"), "{served}");
        assert!(served.contains("text/html"), "{served}");

        // The wrong token is refused, and the page is not leaked with the
        // refusal.
        let refused = round_trip(&get("/"), "not-the-token");
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
        assert!(!refused.contains("<html"), "{refused}");
    }

    #[test]
    fn a_query_token_answers_once_and_becomes_a_cookie() {
        let response = round_trip("GET /?token=abc HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", TOKEN);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("Set-Cookie: syro_token=abc; HttpOnly"),
            "{response}"
        );

        // A wrong one is refused and, being wrong, is not handed back as a
        // cookie to try again with.
        let refused = round_trip(
            "GET /?token=nope HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            TOKEN,
        );
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
        assert!(!refused.contains("Set-Cookie"), "{refused}");
    }

    #[test]
    fn state_answers_json() {
        let response = round_trip(&get("/api/state"), TOKEN);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("application/json"), "{response}");
        assert!(response.ends_with(r#"{"worktrees":[]}"#), "{response}");
    }

    #[test]
    fn a_terminal_reads_its_text_and_takes_the_keys() {
        let response = round_trip(&get("/api/terminal/terminal-7/snapshot"), TOKEN);
        assert!(response.ends_with("text of terminal-7"), "{response}");

        let response = round_trip(
            &post(
                "/api/terminal/terminal-7/input",
                r#"{"keys":["enter","y"]}"#,
            ),
            TOKEN,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        // Enter is a carriage return, and it arrived unmangled.
        assert!(response.ends_with("terminal-7 <- \ry"), "{response:?}");

        let response = round_trip(
            &post("/api/terminal/terminal-7/input", r#"{"text":"继续\n"}"#),
            TOKEN,
        );
        assert!(response.ends_with("terminal-7 <- 继续\n"), "{response:?}");
    }

    #[test]
    fn a_request_the_page_got_wrong_is_refused_with_a_reason() {
        let response = round_trip(
            &post("/api/terminal/terminal-7/input", r#"{"keys":["hover"]}"#),
            TOKEN,
        );
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("a key name is not known"), "{response}");

        let response = round_trip(
            &post("/api/terminal/terminal-7/input", r#"{"nothing":true}"#),
            TOKEN,
        );
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");

        let response = round_trip(
            &post("/api/terminal/terminal-7/resize", r#"{"cols":100}"#),
            TOKEN,
        );
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");

        let response = round_trip(&get("/api/nope"), TOKEN);
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    }

    #[test]
    fn a_worktree_is_created_and_removed_by_name_and_id() {
        let response = round_trip(
            &post("/api/worktrees", r#"{"repo":"syro","name":"fix-build"}"#),
            TOKEN,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");

        let response = round_trip(
            &post("/api/worktrees/remove", r#"{"id":"/w/fix-build"}"#),
            TOKEN,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");

        // A create with no name is a create the app cannot do.
        let response = round_trip(&post("/api/worktrees", r#"{"repo":"syro"}"#), TOKEN);
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }

    #[test]
    fn the_agents_are_listed_and_one_can_start_a_terminal() {
        let response = round_trip(&get("/api/agents"), TOKEN);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("Claude Code"), "{response}");

        let response = round_trip(
            &post(
                "/api/terminals",
                r#"{"worktree":"/w/fix-build","agent":"claude"}"#,
            ),
            TOKEN,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");

        // A terminal with no agent is a plain shell, which is what a missing key
        // means rather than an error.
        let response = round_trip(
            &post("/api/terminals", r#"{"worktree":"/w/fix-build"}"#),
            TOKEN,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");

        // But a request that names no worktree has nowhere to open anything.
        let response = round_trip(&post("/api/terminals", r#"{"agent":"claude"}"#), TOKEN);
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }

    /// A real PTY. `Subscription` is only made by a terminal, so the tests that
    /// stream have to have one — which is better anyway: these bytes come from a
    /// child, not from a channel a test filled by hand.
    fn spawn_session(script: &str) -> syro_core::term::TermSession {
        syro_core::term::TermSession::spawn(syro_core::term::TermSpec {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            ..syro_core::term::TermSpec::shell(
                std::env::temp_dir(),
                syro_core::term::TermSize::new(80, 24),
            )
        })
        .expect("spawn a shell")
    }

    /// A session shared with the fake main thread, so a handler can hand out its
    /// screen and its stream.
    fn shared(
        session: syro_core::term::TermSession,
    ) -> std::sync::Arc<std::sync::Mutex<Option<syro_core::term::TermSession>>> {
        std::sync::Arc::new(std::sync::Mutex::new(Some(session)))
    }

    /// A terminal that prints, then keeps writing a dot every so often: enough for
    /// the stream to have something to send while the test reads it.
    fn printing() -> syro_core::term::TermSession {
        let session = spawn_session(
            "printf 'from the pty\\r\\n'; i=0; while [ $i -lt 200 ]; do printf '.'; sleep 0.02; i=$((i+1)); done",
        );
        // The paint is read from the grid, so the child's first line has to be in
        // it before the stream starts.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !session.snapshot().text().contains("from the pty") {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never printed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        session
    }

    #[test]
    fn a_stream_paints_the_screen_and_then_sends_the_bytes() {
        let held = shared(printing());
        let response = round_trip_until(
            &get("/api/terminal/terminal-7/stream"),
            TOKEN,
            {
                let held = std::sync::Arc::clone(&held);
                move |action| match action {
                    Action::Stream { .. } => {
                        let guard = held.lock().unwrap();
                        let session = guard.as_ref().expect("a live terminal");
                        Payload::Stream {
                            text: session.snapshot().text(),
                            bytes: session.subscribe(),
                        }
                    }
                    _ => Payload::Ok,
                }
            },
            // Stopped as soon as both halves have been seen.
            |response| response.contains("event: bytes") && response.contains("event: paint"),
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("Content-Type: text/event-stream"),
            "{response}"
        );
        // The paint is a clear-screen and the screen, hex-encoded so that its
        // newlines survive a line-based protocol.
        assert!(response.contains(&hex(b"from the pty")), "{response}");
        // The clear-screen, which is part of every paint; hex-encoded like the
        // rest of the payload, so this is the escape's own bytes spelled out.
        assert!(response.contains(&hex(b"\x1b[2J")), "{response}");
        // And the child's later bytes arrive as their own events.
        assert!(response.contains("event: bytes\ndata: "), "{response}");
        // The child is stopped and the stream with it: a test that leaves a shell
        // writing in the background is a test that slows every other one down.
        held.lock().unwrap().take();
    }

    /// A reader that fell behind missed frames with no way to know which: the
    /// stream has to give it the grid again, or its screen stays wrong.
    #[test]
    fn a_reader_that_fell_behind_is_repainted_from_the_grid() {
        // One byte per read, with a gap in between, so the 64-frame queue really
        // does fill while nothing is reading it.
        let held = shared(spawn_session(
            "i=0; while [ $i -lt 400 ]; do printf 'x'; sleep 0.005; i=$((i+1)); done; sleep 30",
        ));
        let response = round_trip_until(
            &get("/api/terminal/terminal-7/stream"),
            TOKEN,
            {
                let held = std::sync::Arc::clone(&held);
                move |action| match action {
                    Action::Stream { .. } => {
                        let guard = held.lock().unwrap();
                        let session = guard.as_ref().expect("a live terminal");
                        let text = session.snapshot().text();
                        let bytes = session.subscribe();
                        drop(guard);
                        // Long enough for the queue to overflow unread: this is
                        // exactly the reader that fell behind.
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        Payload::Stream { text, bytes }
                    }
                    Action::Snapshot { .. } => {
                        let guard = held.lock().unwrap();
                        let session = guard.as_ref().expect("a live terminal");
                        Payload::Text(session.snapshot().text())
                    }
                    _ => Payload::Ok,
                }
            },
            // Two paints: the first screen, and the repair after the drop.
            |response| response.matches("event: paint").count() >= 2,
        );
        assert_eq!(
            response.matches("event: paint").count(),
            2,
            "the reader that fell behind was not repainted: {response}"
        );
        // The repair is the same kind of event as the first paint, and it came
        // from the grid rather than from the bytes that were dropped.
        assert!(response.contains(&hex(b"\x1b[2J")), "{response}");
        // Taking the session stops the child and ends the stream with it.
        held.lock().unwrap().take();
    }

    /// The state the page reads: pushed when it changes, quiet when it does not.
    #[test]
    fn the_state_stream_sends_the_state_and_only_when_it_changes() {
        // The fake app answers with a different state each time it is asked, so a
        // stream that sent every tick would produce far more than two events.
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let response = round_trip_until(
            &get("/api/state/stream"),
            TOKEN,
            {
                let asked = std::sync::Arc::clone(&asked);
                move |action| match action {
                    Action::State => {
                        let n = asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Payload::Json(serde_json::json!({"worktrees": [], "n": n}))
                    }
                    _ => Payload::Ok,
                }
            },
            |response| response.matches("event: state").count() >= 2,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("Content-Type: text/event-stream"),
            "{response}"
        );
        // Both states arrived, in order, one line each: a line-based protocol
        // cannot carry the newlines a pretty-printed body would have.
        let first = response.find(r#"data: {"worktrees":[],"n":0}"#);
        let second = response.find(r#"data: {"worktrees":[],"n":1}"#);
        assert!(first.is_some() && second.is_some(), "{response}");
        assert!(first < second, "{response}");
        // And nothing was sent between them: the second tick found the same state
        // the first one had already been told about.
        assert_eq!(
            response.matches("event: state").count(),
            2,
            "the stream repeated a state it had already sent: {response}"
        );
    }

    /// The page carries the app's language, not the browser's guess at it.
    #[test]
    fn the_page_is_served_in_the_apps_language() {
        assert!(page_for("zh-CN").contains(r#""zh-CN""#));
        assert!(!page_for("zh-CN").contains(LOCALE_SLOT));
        assert!(page_for("en").contains(r#""en""#));
        // Served, not just built: the slot must not reach a browser.
        let response = round_trip(&get("/"), TOKEN);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains("__syro_locale__"), "{response}");
        assert!(!response.contains(LOCALE_SLOT), "{response}");
    }

    /// The terminal library is served from the binary, not from a CDN.
    #[test]
    fn the_full_terminal_library_is_served_from_the_binary() {
        // Each file is checked for something only that file has, so a route that
        // served the page for all three would fail rather than pass.
        for (path, marker) in [
            ("/xterm.js", "Terminal"),
            ("/xterm-addon-fit.js", "FitAddon"),
            ("/xterm.css", ".xterm"),
            ("/icon.png", "PNG"),
        ] {
            let response = round_trip(&get(path), TOKEN);
            assert!(response.starts_with("HTTP/1.1 200"), "{path}: {response}");
            assert!(
                response.contains(marker),
                "{path} does not look like itself"
            );
        }
        // Without a token, no asset: the page is not a public file server.
        let refused = round_trip(&get("/xterm.js"), "not-the-token");
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
    }

    #[test]
    fn hex_carries_every_byte_and_refuses_what_is_not_hex() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(hex_decode(&hex(&bytes)).unwrap(), bytes);
        assert_eq!(hex(&[0x0f, 0xa0]), "0fa0");
        // Odd length, and a pair that is not hex: both are refused rather than
        // half-decoded.
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
    }

    #[test]
    fn a_cookie_is_read_out_of_a_header_with_more_than_one() {
        assert_eq!(
            cookie_token("a=b; syro_token=abc; c=d").as_deref(),
            Some("abc")
        );
        assert_eq!(cookie_token("syro_token=abc").as_deref(), Some("abc"));
        assert_eq!(cookie_token("other=abc"), None);
    }

    #[test]
    fn query_parameters_are_named_not_positional() {
        assert_eq!(
            query_param("token=abc&x=1", "token").as_deref(),
            Some("abc")
        );
        assert_eq!(
            query_param("x=1&token=abc", "token").as_deref(),
            Some("abc")
        );
        assert_eq!(query_param("x=1", "token"), None);
        assert_eq!(query_param("", "token"), None);
    }

    #[test]
    fn a_token_is_read_back_from_where_it_was_written() {
        let dir = std::env::temp_dir().join(format!(
            "syro-web-token-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("web-token");
        // The directory is not made by the caller: the panel makes it, because
        // on a fresh machine nothing under `~/.config/syro` exists yet.
        let fresh = token_at(&path).unwrap();
        assert_eq!(fresh.len(), 32);
        assert!(path.is_file(), "the token was not written");
        // Reading it again is the same token, which is what makes a bookmarked
        // URL survive a restart.
        assert_eq!(token_at(&path).unwrap(), fresh);
        // And a token someone else wrote is used as it is.
        std::fs::write(&path, "  given\n").unwrap();
        assert_eq!(token_at(&path).unwrap(), "given");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_random_token_is_long_and_not_repeated() {
        let a = random_hex();
        let b = random_hex();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
