//! `tf_tree top --web` — `docs/PHASE5.md` §7's embedded static web view.
//!
//! No HTTP crate: §7 is NORMATIVE that the page is one embedded HTML file plus
//! one JSON endpoint (no build step, npm or CDN), and a server crate would pull a
//! runtime into a workspace that forbids `async` (D14). `std::net::TcpListener`,
//! a scoped thread per connection, no keep-alive; never point it at a network.
//!
//! The only `AF_INET` socket in the repository, opt-in and loopback by default;
//! §5.1's "no network sockets" is about the library. `scripts/no-network.sh` uses
//! `crates/tf_tree_cli/tests/web.rs` as its positive control.
//!
//! * Loopback by default (§7): [`DEFAULT_ADDR`], with [`exposure_warning`] otherwise.
//! * A `Host` guard against DNS rebinding: bound to loopback, a non-loopback
//!   `Host` is refused ([`host_is_loopback`]).
//! * Nothing is read from the filesystem: two routes, served from memory.
//!
//! Read-only: the JSON comes from the TUI's [`crate::top::Tick`]
//! (`top::tests::capturing_the_arena_moves_no_counter`).

use std::fmt::Write as _;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::catalogue::json_escape;
use crate::top::{Bucket, EdgeRow, EdgeSample, IntervalStats, ParticipantSample, Tick};

/// The address `--web` binds when given no value (loopback, §7).
pub const DEFAULT_ADDR: &str = "127.0.0.1:8787";

/// The served page. One file, no build step (§7).
pub const INDEX_HTML: &str = include_str!("web/index.html");

/// The JSON schema identifier. **Stable**, in the style of `tf_tree.doctor/1`.
pub const SCHEMA: &str = "tf_tree.top/1";

/// How long a client gets to send its request head, and to accept the response.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// The largest request head accepted, in bytes.
const MAX_HEAD: usize = 8 * 1024;

/// How many histogram buckets each edge carries in the JSON.
const HIST_BUCKETS: usize = 24;

/// How many connections may be in flight at once; past it one is dropped unread.
const MAX_CONNECTIONS: usize = 64;

/// What a parsed request resolves to; [`route`] is pure and tested without a socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// `GET /` or `GET /index.html` — the embedded page.
    Index,
    /// `GET /api/tick` — the one JSON endpoint.
    Tick,
    /// A well-formed request for something that does not exist.
    NotFound,
    /// A request we could not parse a method and path out of.
    BadRequest,
    /// Anything but `GET`.
    MethodNotAllowed,
    /// Bound to loopback, and the `Host` header is not a loopback name.
    ForbiddenHost,
}

impl Route {
    /// The status line and content type this route answers with.
    fn status(self) -> (&'static str, &'static str) {
        match self {
            Route::Index => ("200 OK", "text/html; charset=utf-8"),
            Route::Tick => ("200 OK", "application/json"),
            Route::NotFound => ("404 Not Found", "text/plain; charset=utf-8"),
            Route::BadRequest => ("400 Bad Request", "text/plain; charset=utf-8"),
            Route::MethodNotAllowed => ("405 Method Not Allowed", "text/plain; charset=utf-8"),
            Route::ForbiddenHost => ("403 Forbidden", "text/plain; charset=utf-8"),
        }
    }
}

/// Whether a `Host` header value (`host[:port]`, port ignored) names loopback;
/// `localhost` by name, every other name refused.
#[must_use]
pub fn host_is_loopback(value: &str) -> bool {
    let host = if let Some(rest) = value.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, _)) => inner,
            None => return false,
        }
    } else {
        value.split_once(':').map_or(value, |(h, _)| h)
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Resolve a request head to a [`Route`].
///
/// The `Host` guard applies only when `bound` is loopback.
#[must_use]
pub fn route(head: &str, bound: SocketAddr) -> Route {
    let mut lines = head.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Route::BadRequest;
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Route::BadRequest;
    };
    if method != "GET" {
        return Route::MethodNotAllowed;
    }
    if bound.ip().is_loopback() {
        let host = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.trim());
        // A missing `Host` is refused: absence must not bypass the guard.
        match host {
            Some(v) if host_is_loopback(v) => {}
            _ => return Route::ForbiddenHost,
        }
    }
    let path = target.split(['?', '#']).next().unwrap_or(target);
    match path {
        "/" | "/index.html" => Route::Index,
        "/api/tick" => Route::Tick,
        _ => Route::NotFound,
    }
}

/// A finite `f64` as JSON, or `null` (`NaN`/`±Infinity` are not JSON).
fn num(v: Option<f64>) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.6}"),
        _ => "null".to_owned(),
    }
}

/// An `Option<i64>` as JSON, or `null`.
fn int(v: Option<i64>) -> String {
    v.map_or_else(|| "null".to_owned(), |x| x.to_string())
}

fn stats_json(s: Option<IntervalStats>) -> String {
    match s {
        None => "null".to_owned(),
        Some(s) => format!(
            "{{\"n\":{},\"min_ns\":{},\"median_ns\":{},\"p99_ns\":{},\"max_ns\":{},\
             \"non_monotonic\":{}}}",
            s.n, s.min_ns, s.median_ns, s.p99_ns, s.max_ns, s.non_monotonic
        ),
    }
}

fn hist_json(buckets: &[Bucket]) -> String {
    let mut s = String::from("[");
    for (i, b) in buckets.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"lo_ns\":{},\"hi_ns\":{},\"count\":{}}}",
            b.lo_ns, b.hi_ns, b.count
        );
    }
    s.push(']');
    s
}

fn edge_json(e: &EdgeSample, r: &EdgeRow) -> String {
    let kind = match e.kind {
        tf_tree::unstable::EdgeKind::Static => "static",
        tf_tree::unstable::EdgeKind::Dynamic => "dynamic",
        _ => "other",
    };
    format!(
        "{{\"id\":{},\"label\":\"{}\",\"kind\":\"{kind}\",\"capacity\":{},\"head\":{},\
         \"occupancy\":{},\"retained\":{},\"claimed\":{},\"owner_pid\":{},\
         \"oldest_stamp\":{},\"newest_stamp\":{},\"age_ns\":{},\"rate_hz\":{},\
         \"observed_hz\":{},\"delta_head\":{},\"delta_errors\":{},\"errors_total\":{},\
         \"lookups_ok\":{},\"worst_extrap_gap_ns\":{},\"stats\":{},\"histogram\":{}}}",
        e.id,
        json_escape(&e.label),
        e.capacity,
        e.head,
        e.occupancy(),
        e.retained,
        e.claimed,
        e.owner_pid,
        int(e.oldest_stamp),
        int(e.newest_stamp),
        int(r.age_ns),
        num(r.stats.and_then(|s| s.rate_hz())),
        num(r.observed_hz),
        r.delta_head,
        r.delta_errors,
        e.counters.errors(),
        e.counters.lookups_ok,
        e.counters.worst_extrap_gap_ns,
        stats_json(r.stats),
        hist_json(&crate::top::histogram(&e.intervals, HIST_BUCKETS)),
    )
}

fn participant_json(p: &ParticipantSample) -> String {
    format!(
        "{{\"slot\":{},\"pid\":{},\"mode\":{},\"comm\":\"{}\",\"in_arena\":{},\"alive\":{},\
         \"attached_at_nanos\":{},\"errors_total\":{},\"lookups_ok\":{},\"last_err_edge\":{}}}",
        p.slot,
        p.pid,
        p.mode
            .map_or_else(|| "null".to_owned(), |m| format!("\"{m}\"")),
        json_escape(&p.comm),
        p.in_arena,
        p.alive,
        p.attached_at_nanos,
        p.counters.errors(),
        p.counters.lookups_ok,
        // `u32::MAX` is "no edge".
        if p.last_err_edge == u32::MAX {
            "null".to_owned()
        } else {
            p.last_err_edge.to_string()
        },
    )
}

/// Render one tick as the `tf_tree.top/1` document.
///
/// # Schema — stable
///
/// ```text
/// {
///   "schema": "tf_tree.top/1", "tool_version": string,
///   "tick": u64, "elapsed_ms": f64, "poll_ms": u64,
///   "source": string, "clock": string, "arena_now_nanos": i64|null,
///   "arena_bytes": u64, "frames": u64, "counters_compiled_in": bool,
///   "shared": bool, "self_slot": u32|null, "selected": u32|null,
///   "occupancy":    [ { "what": string, "used": u32, "capacity": u32 } ],
///   "edges":        [ { ... see `edge_json` ... } ],
///   "participants": [ { ... see `participant_json` ... } ],
///   "feed":         [ { "tick": u64, "severity": "info"|"warn"|"error",
///                       "id": "TFTNNN"|null, "subject": string,
///                       "message": string } ]
/// }
/// ```
///
/// `clock` is [`crate::checks::Clock::label`]'s sentence (§7 amendment).
#[must_use]
pub fn tick_json(tick: &Tick, poll: Duration, selected: Option<u32>) -> String {
    let c = &tick.capture;
    let mut s = String::with_capacity(4096);
    let _ = write!(
        s,
        "{{\"schema\":\"{SCHEMA}\",\"tool_version\":\"{}\",\"tick\":{},\
         \"elapsed_ms\":{:.3},\"poll_ms\":{},\"source\":\"{}\",\"clock\":\"{}\",\
         \"arena_now_nanos\":{},\"arena_bytes\":{},\"frames\":{},\
         \"counters_compiled_in\":{},\"shared\":{},\"self_slot\":{},\"selected\":{},",
        json_escape(env!("CARGO_PKG_VERSION")),
        tick.tick,
        tick.elapsed.as_secs_f64() * 1e3,
        poll.as_millis(),
        json_escape(c.source),
        json_escape(c.clock.map_or("no stamps in any ring", |k| k.label())),
        int(c.arena_now()),
        c.arena_bytes,
        c.frames,
        c.counters_compiled_in,
        c.shared,
        c.self_slot
            .map_or_else(|| "null".to_owned(), |v| v.to_string()),
        selected.map_or_else(|| "null".to_owned(), |v| v.to_string()),
    );

    s.push_str("\"occupancy\":[");
    for (i, (what, used, cap)) in c.occupancy.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"what\":\"{}\",\"used\":{used},\"capacity\":{cap}}}",
            json_escape(what)
        );
    }
    s.push_str("],\"edges\":[");
    for (i, (e, r)) in c.edges.iter().zip(tick.rows.iter()).enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&edge_json(e, r));
    }
    s.push_str("],\"participants\":[");
    for (i, p) in c.participants.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&participant_json(p));
    }
    s.push_str("],\"feed\":[");
    for (i, ev) in tick.feed.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"tick\":{},\"severity\":\"{}\",\"id\":{},\"subject\":\"{}\",\"message\":\"{}\"}}",
            ev.tick,
            ev.severity.json(),
            ev.id
                .map_or_else(|| "null".to_owned(), |t| format!("\"{}\"", t.id())),
            json_escape(&ev.subject),
            json_escape(&ev.message),
        );
    }
    s.push_str("]}");
    s
}

/// Bind the listener, printing the resolved address.
///
/// # Errors
///
/// If the address cannot be bound — in use, or not an address this host has.
pub fn bind(addr: SocketAddr) -> Result<(TcpListener, SocketAddr)> {
    let listener =
        TcpListener::bind(addr).with_context(|| format!("binding the --web view to {addr}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    if let Some(warning) = exposure_warning(local) {
        eprintln!("{warning}");
    }
    println!("tf_tree top --web: read-only view on http://{local}/ (Ctrl-C to stop)");
    Ok((listener, local))
}

/// The stderr line a non-loopback bind earns (§7's amendment), or `None` for loopback.
#[must_use]
pub fn exposure_warning(local: SocketAddr) -> Option<String> {
    if local.ip().is_loopback() {
        return None;
    }
    Some(format!(
        "warning: --web is bound to {local}, which is not loopback. This serves the arena's \
         frame names, pids and rates to anyone who can reach that address, with no \
         authentication. Bind {DEFAULT_ADDR} and use an SSH tunnel instead."
    ))
}

/// Read a request head (everything up to the blank line) from `stream`.
///
/// `Ok(None)` when the peer closed, sent more than [`MAX_HEAD`], or timed out.
fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(Some(String::from_utf8_lossy(&buf[..end]).into_owned()));
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
    }
}

/// Write one response and close.
///
/// Every header is load-bearing (`Connection: close`, `Content-Security-Policy`
/// incl. `frame-ancestors`, `nosniff`, `Cache-Control: no-store`). There is
/// deliberately no `Access-Control-Allow-Origin`: same-origin is half of
/// [`route`]'s rebinding defence.
fn respond(stream: &mut TcpStream, route: Route, body: &[u8]) -> std::io::Result<()> {
    let (status, content_type) = route.status();
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; \
         script-src 'unsafe-inline'; connect-src 'self'; img-src data:; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Whether an `accept(2)` failure is about one peer (`ECONNABORTED`, `EINTR`,
/// `EMFILE`/`ENFILE`) rather than the listener; only the latter ends the view.
#[must_use]
pub fn accept_is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
    ) || is_descriptor_exhaustion(e)
}

/// `EMFILE`/`ENFILE`, which `std` maps to no [`ErrorKind`] (raw `errno`, unix only).
#[must_use]
fn is_descriptor_exhaustion(e: &std::io::Error) -> bool {
    cfg!(unix) && matches!(e.raw_os_error(), Some(23 | 24))
}

/// One connection, start to finish: deadlines, head, route, response.
///
/// The write deadline is deliberately untested: both bodies fit a default send buffer.
fn handle(
    stream: &mut TcpStream,
    bound: SocketAddr,
    tick: &dyn Fn() -> String,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let Some(head) = read_head(stream)? else {
        return Ok(());
    };
    let r = route(&head, bound);
    match r {
        Route::Index => respond(stream, r, INDEX_HTML.as_bytes()),
        Route::Tick => respond(stream, r, tick().as_bytes()),
        Route::NotFound => respond(stream, r, b"not found\n"),
        Route::BadRequest => respond(stream, r, b"bad request\n"),
        Route::MethodNotAllowed => respond(stream, r, b"only GET\n"),
        Route::ForbiddenHost => respond(
            stream,
            r,
            b"forbidden: this view is bound to loopback and only answers requests whose \
              Host is a loopback name. A page on another origin reaching this address is \
              DNS rebinding, not you.\n",
        ),
    }
}

/// Serve the view until `max_requests` connections have been accepted.
///
/// `max_requests == 0` runs until interrupted; otherwise the bound counts accepted
/// connections. One scoped thread per connection, capped at `MAX_CONNECTIONS`
/// (past it a connection is closed unread and reported once). Returns once every
/// accepted connection has finished.
///
/// `tick` is shared by handlers; the caller must rate-limit and serialise it
/// (see `cmd_top_web`), or two tabs split every per-tick delta.
///
/// # Errors
///
/// Only an `accept` failure that ends the listener ([`accept_is_transient`]).
pub fn serve(
    listener: &TcpListener,
    bound: SocketAddr,
    max_requests: u64,
    tick: &(dyn Fn() -> String + Sync),
) -> Result<()> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let live = AtomicUsize::new(0);
    let warned = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let mut served = 0u64;
        loop {
            let (mut stream, _peer) = match listener.accept() {
                Ok(v) => v,
                Err(e) if accept_is_transient(&e) => {
                    eprintln!("--web: accept failed, still listening: {e}");
                    if is_descriptor_exhaustion(&e) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    // Not counted against `max_requests`: no connection was handled.
                    continue;
                }
                Err(e) => return Err(e).context("accepting a --web connection"),
            };
            served += 1;

            if live.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                live.fetch_sub(1, Ordering::AcqRel);
                if !warned.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "--web: more than {MAX_CONNECTIONS} connections in flight; dropping the \
                         excess unread. Something is opening sockets to this port faster than a \
                         browser does."
                    );
                }
                drop(stream);
            } else {
                let live = &live;
                scope.spawn(move || {
                    let handled = handle(&mut stream, bound, tick);
                    live.fetch_sub(1, Ordering::AcqRel);
                    if let Err(e) = handled {
                        // A broken pipe is a browser navigating away.
                        if e.kind() != ErrorKind::BrokenPipe {
                            eprintln!("--web: dropping a connection: {e}");
                        }
                    }
                });
            }

            if max_requests != 0 && served >= max_requests {
                return Ok(());
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::io::BufRead as _;
    use std::net::Ipv4Addr;

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 8787))
    }

    fn get(path: &str, host: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: */*")
    }

    /// The page with `<!-- ... -->` removed; the JavaScript is left as served.
    fn page_without_html_comments() -> String {
        let mut out = String::with_capacity(INDEX_HTML.len());
        let mut rest = INDEX_HTML;
        while let Some(i) = rest.find("<!--") {
            out.push_str(&rest[..i]);
            match rest[i..].find("-->") {
                Some(j) => rest = &rest[i + j + 3..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// The two routes resolve, and nothing else does.
    #[test]
    fn only_the_two_documented_paths_resolve() {
        let b = loopback();
        assert_eq!(route(&get("/", "127.0.0.1:8787"), b), Route::Index);
        assert_eq!(route(&get("/index.html", "localhost"), b), Route::Index);
        assert_eq!(route(&get("/api/tick", "localhost:8787"), b), Route::Tick);
        // A query string is split off, not matched.
        assert_eq!(route(&get("/api/tick?t=3", "localhost"), b), Route::Tick);
        assert_eq!(route(&get("/api", "localhost"), b), Route::NotFound);
        assert_eq!(
            route(&get("/api/tick/../../etc/passwd", "localhost"), b),
            Route::NotFound
        );
        assert_eq!(
            route(&get("/../src/web.rs", "localhost"), b),
            Route::NotFound
        );
        assert_eq!(
            route("POST /api/tick HTTP/1.1\r\nHost: localhost", b),
            Route::MethodNotAllowed
        );
        assert_eq!(route("", b), Route::BadRequest);
        assert_eq!(route("GET\r\nHost: localhost", b), Route::BadRequest);
    }

    /// A loopback bind refuses any non-loopback `Host` (DNS rebinding).
    #[test]
    fn a_foreign_host_header_is_refused_on_a_loopback_bind() {
        let b = loopback();
        assert_eq!(route(&get("/", "evil.example"), b), Route::ForbiddenHost);
        assert_eq!(
            route(&get("/api/tick", "evil.example:8787"), b),
            Route::ForbiddenHost
        );
        assert_eq!(route(&get("/", "10.0.0.5:8787"), b), Route::ForbiddenHost);
        assert_eq!(route("GET / HTTP/1.1", b), Route::ForbiddenHost);
        for h in ["localhost", "LocalHost:8787", "127.0.0.1", "[::1]:8787"] {
            assert_eq!(route(&get("/", h), b), Route::Index, "host {h}");
        }
    }

    /// The `Host` guard applies only to a loopback bind (see [`route`]).
    #[test]
    fn the_host_guard_is_scoped_to_a_loopback_bind() {
        let public = SocketAddr::from(([0, 0, 0, 0], 8787));
        assert_eq!(route(&get("/", "robot.local:8787"), public), Route::Index);
        assert_eq!(
            route(&get("/", "robot.local:8787"), loopback()),
            Route::ForbiddenHost
        );
    }

    /// `host_is_loopback` is not fooled by a name that merely contains one.
    #[test]
    fn a_loopback_name_must_be_the_whole_host() {
        assert!(host_is_loopback("127.0.0.1"));
        assert!(host_is_loopback("127.1.2.3:1"));
        assert!(host_is_loopback("[::1]"));
        assert!(!host_is_loopback("127.0.0.1.evil.example"));
        assert!(!host_is_loopback("localhost.evil.example"));
        assert!(!host_is_loopback("[::1"));
        assert!(!host_is_loopback(""));
        assert!(!host_is_loopback("0.0.0.0"));
    }

    /// The page loads nothing from the network (§7 NORMATIVE).
    #[test]
    fn the_embedded_page_references_nothing_external() {
        let page = page_without_html_comments();
        let mut urls: Vec<&str> = Vec::new();
        let mut rest = page.as_str();
        while let Some(i) = rest.find("://") {
            let start = rest[..i].rfind(|c: char| c.is_whitespace() || c == '"' || c == '\'');
            let from = start.map_or(0, |p| p + 1);
            let end = rest[from..]
                .find(['"', '\'', ' ', ')', '\n'])
                .map_or(rest.len(), |p| from + p);
            urls.push(&rest[from..end]);
            rest = &rest[i + 3..];
        }
        assert_eq!(
            urls,
            ["http://www.w3.org/2000/svg"],
            "the page must reference nothing it would fetch"
        );
        // External dependencies without an absolute URL: `//` src, @import, import().
        assert!(!page.contains("src=\"//"), "protocol-relative script");
        assert!(!page.contains("@import"), "css @import");
        assert!(!page.contains("import("), "dynamic import");
        assert!(page.contains("fetch(\"api/tick\""));
    }

    /// The page never builds DOM from a string (`top::sanitize` is the ANSI-side twin).
    #[test]
    fn the_embedded_page_never_uses_inner_html() {
        let page = page_without_html_comments();
        assert!(!page.contains("innerHTML"));
        assert!(!page.contains("outerHTML"));
        assert!(!page.contains("insertAdjacentHTML"));
        assert!(!page.contains("document.write"));
        assert!(!page.contains("eval("));
        assert!(!page.contains("new Function"));
    }

    /// A response carries the CSP that makes "no CDN" the browser's rule.
    #[test]
    fn a_served_response_carries_its_headers_and_body() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let n = std::sync::atomic::AtomicU32::new(0);
            serve(&listener, bound, 2, &|| {
                let seq = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                format!("{{\"schema\":\"{SCHEMA}\",\"n\":{seq}}}")
            })
            .unwrap();
        });

        let fetch = |path: &str| {
            let mut s = TcpStream::connect(bound).unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
                .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            let (head, body) = out.split_once("\r\n\r\n").expect("a header terminator");
            (head.to_owned(), body.to_owned())
        };

        let (head, body) = fetch("/");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(
            head.contains("Content-Security-Policy: default-src 'none';"),
            "{head}"
        );
        assert!(head.contains("connect-src 'self'"), "{head}");
        assert!(head.contains("frame-ancestors 'none'"), "{head}");
        assert!(head.contains("Connection: close"), "{head}");
        assert!(
            head.contains(&format!("Content-Length: {}", INDEX_HTML.len())),
            "content length must be the page's byte length: {head}"
        );
        assert_eq!(body, INDEX_HTML, "the body is the embedded page");

        let (head, body) = fetch("/api/tick");
        assert!(head.contains("Content-Type: application/json"), "{head}");
        assert_eq!(body, "{\"schema\":\"tf_tree.top/1\",\"n\":1}");
        h.join().unwrap();
    }

    /// The `tick` closure runs for `/api/tick` and nothing else.
    #[test]
    fn only_the_json_route_samples_the_arena() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let n = std::sync::atomic::AtomicU32::new(0);
            serve(&listener, bound, 3, &|| {
                n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                "{}".to_owned()
            })
            .unwrap();
            // `serve` returns only after every handler finished.
            tx.send(n.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap();
        });
        for path in ["/", "/favicon.ico", "/api/tick"] {
            let mut s = TcpStream::connect(bound).unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
                .unwrap();
            let mut sink = Vec::new();
            s.read_to_end(&mut sink).unwrap();
        }
        assert_eq!(rx.recv().unwrap(), 1, "only /api/tick may sample");
        h.join().unwrap();
    }

    /// A silent client is dropped after [`IO_TIMEOUT`] and a bounded run still returns.
    #[test]
    fn a_client_that_never_speaks_does_not_wedge_the_server() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let r = serve(&listener, bound, 2, &|| "{}".to_owned());
            tx.send(r.is_ok()).unwrap();
        });
        let silent = TcpStream::connect(bound).unwrap();
        let mut s = TcpStream::connect(bound).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&mut s)
            .read_line(&mut line)
            .expect("a real request must be answered while a silent peer is held");
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");
        // `silent` stays open: closing it would end the handler by EOF, not the timeout.
        assert!(
            rx.recv_timeout(Duration::from_secs(20))
                .expect("IO_TIMEOUT must retire a silent connection so `serve` can return"),
            "serve returned an error"
        );
        drop(silent);
        h.join().unwrap();
    }

    /// Silent peers cost the operator's poll nothing, however many there are.
    #[test]
    fn silent_peers_do_not_delay_the_operators_poll() {
        const SILENT: usize = 5;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let r = serve(&listener, bound, (SILENT + 1) as u64, &|| "{}".to_owned());
            tx.send(r.is_ok()).unwrap();
        });

        let held: Vec<TcpStream> = (0..SILENT)
            .map(|_| TcpStream::connect(bound).unwrap())
            .collect();
        // All must be accepted before the real request, or the server is empty.
        std::thread::sleep(Duration::from_millis(100));

        let started = std::time::Instant::now();
        let mut s = TcpStream::connect(bound).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        s.write_all(b"GET /api/tick HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&mut s)
            .read_line(&mut line)
            .expect("the poll must be answered");
        let waited = started.elapsed();
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");
        assert!(
            waited < IO_TIMEOUT,
            "{SILENT} silent peers must not delay a poll; it waited {waited:?}, and the failure \
             mode this pins is that the cost is {SILENT} x {IO_TIMEOUT:?}"
        );

        assert!(
            rx.recv_timeout(Duration::from_secs(30))
                .expect("serve must return once the silent peers time out"),
            "serve returned an error"
        );
        drop(held);
        h.join().unwrap();
    }

    /// Past [`MAX_CONNECTIONS`] a connection is dropped and the loop keeps answering.
    #[test]
    fn the_connection_cap_drops_the_excess_and_keeps_serving() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let total = (MAX_CONNECTIONS + 2) as u64;
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let r = serve(&listener, bound, total, &|| "{}".to_owned());
            tx.send(r.is_ok()).unwrap();
        });

        let held: Vec<TcpStream> = (0..MAX_CONNECTIONS)
            .map(|_| TcpStream::connect(bound).unwrap())
            .collect();
        std::thread::sleep(Duration::from_millis(200));

        // The one past the cap: accepted by the kernel, then closed unread.
        let mut over = TcpStream::connect(bound).unwrap();
        over.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        over.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut sink = Vec::new();
        // Unread bytes make the close a RST; either way the property is no bytes.
        let _ = over.read_to_end(&mut sink);
        assert!(
            sink.is_empty(),
            "a connection past the cap must be closed unread, not answered"
        );

        // Once the silent ones retire, requests are served again.
        drop(held);
        let mut s = TcpStream::connect(bound).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&mut s)
            .read_line(&mut line)
            .expect("the loop must still be answering after the cap was hit");
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");

        assert!(
            rx.recv_timeout(Duration::from_secs(30))
                .expect("serve must return"),
            "serve returned an error"
        );
        h.join().unwrap();
    }

    /// An over-long request head is dropped at the cap, not at [`IO_TIMEOUT`].
    #[test]
    fn an_oversized_request_head_is_refused_promptly() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || serve(&listener, bound, 1, &|| "{}".to_owned()));
        let mut s = TcpStream::connect(bound).unwrap();
        let started = std::time::Instant::now();
        s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
            .unwrap();
        let junk = format!("X-Pad: {}\r\n", "a".repeat(1024));
        for _ in 0..16 {
            if s.write_all(junk.as_bytes()).is_err() {
                break;
            }
        }
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
        let waited = started.elapsed();
        assert!(sink.is_empty(), "an over-long head must get no response");
        assert!(
            waited < Duration::from_secs(1),
            "the cap must end the connection at once, not at the {IO_TIMEOUT:?} timeout \
             (waited {waited:?})"
        );
        h.join().unwrap().unwrap();
    }

    /// A non-finite rate renders as `null` (see [`num`]).
    #[test]
    fn non_finite_rates_render_as_null() {
        assert_eq!(num(None), "null");
        assert_eq!(num(Some(f64::NAN)), "null");
        assert_eq!(num(Some(f64::INFINITY)), "null");
        assert_eq!(num(Some(-0.5)), "-0.500000");
    }

    /// Every string out of the arena is escaped.
    #[test]
    fn labels_are_escaped_into_the_document() {
        let mut e = crate::top::EdgeSample {
            id: 7,
            label: "he\"llo\\\nworld\u{1b}[2J".to_owned(),
            kind: tf_tree::unstable::EdgeKind::Dynamic,
            capacity: 8,
            head: 3,
            claimed: true,
            owner_pid: 9,
            oldest_stamp: Some(1),
            newest_stamp: Some(3),
            retained: 3,
            intervals: vec![1, 1],
            counters: crate::top::CounterSample::default(),
        };
        let row = EdgeRow {
            stats: crate::top::interval_stats(&e.intervals),
            delta_head: 0,
            observed_hz: None,
            age_ns: Some(4),
            delta_errors: 0,
        };
        let doc = edge_json(&e, &row);
        assert!(doc.contains("he\\\"llo\\\\\\nworld\\u001b[2J"), "{doc}");
        // An empty ring produces `null`s, not a missing key.
        e.intervals.clear();
        e.retained = 0;
        e.newest_stamp = None;
        let row = EdgeRow {
            stats: None,
            delta_head: 0,
            observed_hz: None,
            age_ns: None,
            delta_errors: 0,
        };
        let doc = edge_json(&e, &row);
        assert!(doc.contains("\"stats\":null"), "{doc}");
        assert!(doc.contains("\"rate_hz\":null"), "{doc}");
        assert!(doc.contains("\"newest_stamp\":null"), "{doc}");
        assert!(doc.contains("\"histogram\":[]"), "{doc}");
    }

    /// A non-loopback bind produces a warning that says what it exposed.
    #[test]
    fn a_non_loopback_bind_warns_and_a_loopback_one_does_not() {
        assert_eq!(exposure_warning(loopback()), None);
        assert_eq!(
            exposure_warning(SocketAddr::from(([127, 0, 0, 9], 1))),
            None
        );
        let w = exposure_warning(SocketAddr::from(([0, 0, 0, 0], 8787)))
            .expect("a wildcard bind must warn");
        assert!(w.contains("0.0.0.0:8787"), "{w}");
        assert!(w.contains("no authentication"), "{w}");
        assert!(w.contains(DEFAULT_ADDR), "{w}");
        let w = exposure_warning(SocketAddr::from(([10, 0, 0, 5], 80)))
            .expect("a routable bind must warn");
        assert!(w.contains("10.0.0.5:80"), "{w}");
    }

    /// An `accept(2)` failure about one peer does not end the view (pins the classifier).
    #[test]
    fn a_transient_accept_error_is_not_fatal_but_a_broken_listener_is() {
        use std::io::Error;
        assert!(accept_is_transient(&Error::from(
            ErrorKind::ConnectionAborted
        )));
        assert!(accept_is_transient(&Error::from(ErrorKind::Interrupted)));
        assert!(accept_is_transient(&Error::from(
            ErrorKind::ConnectionReset
        )));
        if cfg!(unix) {
            assert!(accept_is_transient(&Error::from_raw_os_error(24)), "EMFILE");
            assert!(accept_is_transient(&Error::from_raw_os_error(23)), "ENFILE");
        }
        assert!(!accept_is_transient(&Error::from(ErrorKind::InvalidInput)));
        assert!(!accept_is_transient(&Error::from(
            ErrorKind::PermissionDenied
        )));
        assert!(!accept_is_transient(&Error::from(ErrorKind::Other)));
    }

    /// The page reads the server's `selected`, so `--edge` reaches the browser.
    #[test]
    fn the_page_seeds_its_selection_from_the_served_selected() {
        let page = page_without_html_comments();
        assert!(
            page.contains("d.selected"),
            "the page must read the `selected` field `tick_json` serves"
        );
        assert!(
            page.contains("seed(d);"),
            "`paint` must seed the selection before it renders"
        );
        assert!(page.contains("if (seeded) return;"), "seeding must be once");
        assert!(page.contains("seeded = true;"));
        // The fallback keeps a tombstoned `--edge` from blanking the pane.
        assert!(page.contains("|| d.edges[0]"), "the fallback must remain");
    }
}
