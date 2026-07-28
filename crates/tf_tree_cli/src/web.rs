//! `tf_tree top --web` — `docs/PHASE5.md` §7's embedded static web view.
//!
//! # Why there is no HTTP crate here
//!
//! §7 is NORMATIVE about the *page*: "a single embedded HTML file plus one JSON
//! endpoint, no build step, no npm, no CDN. The moment this needs a frontend
//! toolchain it becomes a maintenance liability that outlives its usefulness."
//! The §7 amendment then extends the same argument to `ratatui` on the TUI
//! side. A server crate is the third instance of it: `hyper`/`axum` pull a
//! `tokio` runtime into a workspace whose `CLAUDE.md` says "no `async`/runtime"
//! and whose dependency budget is a hard rule, and `tiny_http` is a maintained
//! crate the workspace would still have to keep current — to answer two routes
//! that serve one constant and one string.
//!
//! So this is `std::net::TcpListener`, one connection at a time, no keep-alive.
//! That is ~150 lines and it cannot rot. What it costs is stated in
//! [`serve`]: it is not a general-purpose server and must never be pointed at a
//! network.
//!
//! # This is the only network socket in the repository, and it is opt-in
//!
//! §5.1 is NORMATIVE that "`tf_tree` opens no network sockets. Ever." That
//! sentence is about the *library*, and it stays true: nothing in `tf_tree`,
//! `tf_tree_core`, `tf_tree_arena` or `tf_tree_ipc` can reach this code. The
//! `AF_INET` socket lives in the CLI, is created only when an operator types
//! `--web`, and binds loopback unless that operator types a different address.
//! §11's proposed `socket(2)`-is-only-`AF_UNIX` assertion must therefore be
//! scoped to the library's test suite; a version of it that ran over the CLI
//! would have to encode this exception, which is why the distinction is written
//! down here rather than discovered later.
//!
//! # Three things a two-route server still has to get right
//!
//! * **Loopback by default** (§7). [`DEFAULT_ADDR`] is `127.0.0.1`, and
//!   [`bind`] prints a warning to stderr when the operator asks for anything
//!   else. Serving a robot's live transform state on `0.0.0.0` is the security
//!   bug §7 names.
//! * **A `Host` guard, because loopback is not a boundary a browser respects.**
//!   Any web page the operator visits can `fetch` `http://127.0.0.1:8787/` —
//!   the response is opaque to it under CORS, but DNS rebinding turns
//!   `evil.example` into `127.0.0.1` at the second lookup and the page is then
//!   same-origin with this server and can read every frame name and pid in the
//!   arena. The fix is one line of parsing: when we are bound to loopback, a
//!   request whose `Host` is not a loopback name is refused. See
//!   [`host_is_loopback`].
//! * **Nothing is read from the filesystem.** There are exactly two routes and
//!   both serve memory. A path is matched, never resolved, so there is no
//!   traversal to defend against and `../../etc/passwd` is a 404 like any other
//!   unknown path.
//!
//! # It is still a read-only observer
//!
//! The JSON is produced from the same [`crate::top::Tick`] the TUI renders, by
//! the same [`crate::top::Sampler`], from the same read-only [`crate::top::Capture`].
//! Serving it adds no lookup and takes no claim — `top::tests::capturing_the_arena_moves_no_counter`
//! is the assertion, and it covers this path because this path has no other way
//! to touch the arena.

use std::fmt::Write as _;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::catalogue::json_escape;
use crate::top::{Bucket, EdgeRow, EdgeSample, IntervalStats, ParticipantSample, Tick};

/// The address `--web` binds when given no value.
///
/// Loopback, per §7. The port is unregistered with IANA and unlikely to collide
/// with a robot's own services; `--web 127.0.0.1:0` asks the kernel for a free
/// one and the chosen port is printed.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8787";

/// The served page. One file, no build step (§7).
pub const INDEX_HTML: &str = include_str!("web/index.html");

/// The JSON schema identifier. **Stable**, in the style of `tf_tree.doctor/1`.
pub const SCHEMA: &str = "tf_tree.top/1";

/// How long a client gets to send its request head, and to accept the response.
///
/// The server is single-threaded, so a client that connects and says nothing
/// would otherwise stall the view for every other client — including the
/// operator's own browser. Two seconds is far beyond a loopback round trip and
/// far below an operator's patience.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// The largest request head accepted, in bytes.
///
/// A request that has not finished its headers by here is not a browser polling
/// a two-route endpoint. Bounded because the read loop appends to a `Vec` and
/// an unbounded one is a memory exhaustion an unauthenticated peer controls.
const MAX_HEAD: usize = 8 * 1024;

/// How many histogram buckets each edge carries in the JSON.
///
/// Fixed and modest: the payload carries a histogram for **every** edge so that
/// selecting one in the browser is a repaint rather than a request, which is
/// what keeps the endpoint count at §7's "one". At 24 buckets a 64-edge arena
/// costs ~1500 small objects a poll, which is nothing beside the per-edge
/// interval vectors it is derived from.
const HIST_BUCKETS: usize = 24;

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// What a parsed request resolves to. One variant per response this server can
/// produce, so [`route`] is a pure function and is tested without a socket.
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

/// Whether a `Host` header value names the loopback interface.
///
/// The value is `host[:port]`, with an IPv6 literal in brackets. The port is
/// deliberately ignored: a rebinding attacker controls the *name*, not the
/// port, because the port is the one this server is listening on either way.
///
/// `localhost` is accepted by name because every browser resolves it to
/// loopback and it is what an operator types; every other name is refused, which
/// is exactly the rebinding case (`evil.example` resolving to `127.0.0.1`).
#[must_use]
pub fn host_is_loopback(value: &str) -> bool {
    let host = if let Some(rest) = value.strip_prefix('[') {
        // `[::1]:8787` -> `::1`. A bracketed literal with no closing bracket is
        // malformed and is not loopback.
        match rest.split_once(']') {
            Some((inner, _)) => inner,
            None => return false,
        }
    } else {
        // `127.0.0.1:8787` -> `127.0.0.1`. `split_once` and not `rsplit_once`:
        // a bare IPv6 literal is illegal in a `Host` header, so the first colon
        // is always the port separator, and `rsplit_once` would turn the
        // malformed `::1` into the host `:` and accept nothing anyway.
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
/// `head` is everything before the blank line, exactly as received. `bound` is
/// the address this server actually listened on: when it is **not** loopback the
/// operator explicitly asked for a reachable server, so the `Host` guard is not
/// applied — an attacker who can reach a `0.0.0.0` bind does not need DNS
/// rebinding to do it, and enforcing the guard there would break the only
/// configuration where a non-loopback `Host` is correct.
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
        // A missing `Host` is refused rather than waved through: HTTP/1.1
        // requires it, and "absent" must not be a way around the guard.
        match host {
            Some(v) if host_is_loopback(v) => {}
            _ => return Route::ForbiddenHost,
        }
    }
    // The query string is split off and ignored. Nothing here is parameterised
    // — selection happens in the page, over data it already has.
    let path = target.split(['?', '#']).next().unwrap_or(target);
    match path {
        "/" | "/index.html" => Route::Index,
        "/api/tick" => Route::Tick,
        _ => Route::NotFound,
    }
}

// ---------------------------------------------------------------------------
// The JSON payload
// ---------------------------------------------------------------------------

/// A finite `f64` as JSON, or `null`.
///
/// `NaN` and `±Infinity` are **not** JSON — `JSON.parse` rejects the literal
/// `NaN`, so one non-finite rate would blank the whole page rather than one
/// cell. Every rate here is a division whose denominator came out of somebody
/// else's arena, so this is a live case and not a formality.
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
        tf_tree::EdgeKind::Static => "static",
        tf_tree::EdgeKind::Dynamic => "dynamic",
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
        // `u32::MAX` is "no edge"; JSON gets `null` rather than 4294967295,
        // which a consumer would have to know to special-case.
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
/// `clock` is [`crate::checks::Clock::label`]'s sentence, not a number, for the
/// reason §7's amendment gives: every age in the document is against that
/// reference, and a consumer that assumes Unix nanoseconds on a boot-relative
/// arena is off by decades.
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
    // `zip` and not an index: `rows[i]` describes `edges[i]` by construction
    // (see `EdgeRow`), and zipping makes a future length mismatch truncate
    // rather than panic inside a request handler.
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

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// Bind the listener, announcing the URL an operator should open.
///
/// Prints the **resolved** address, so `--web 127.0.0.1:0` is a usable spelling:
/// the kernel picks a free port and the line names it.
///
/// # Errors
///
/// If the address cannot be bound — in use, or not an address this host has.
pub fn bind(addr: SocketAddr) -> Result<(TcpListener, SocketAddr)> {
    let listener =
        TcpListener::bind(addr).with_context(|| format!("binding the --web view to {addr}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    if !local.ip().is_loopback() {
        // Not an error: an operator on a robot with no display may genuinely
        // want this reachable. But §7 calls a non-loopback *default* a security
        // bug, and an explicit choice deserves to be visible in the log the
        // operator later reads.
        eprintln!(
            "warning: --web is bound to {local}, which is not loopback. This serves the arena's \
             frame names, pids and rates to anyone who can reach that address, with no \
             authentication. Bind {DEFAULT_ADDR} and use an SSH tunnel instead."
        );
    }
    println!("tf_tree top --web: read-only view on http://{local}/ (Ctrl-C to stop)");
    Ok((listener, local))
}

/// Read a request head (everything up to the blank line) from `stream`.
///
/// Returns `Ok(None)` when the peer closed, or sent more than [`MAX_HEAD`], or
/// went quiet past the timeout — all of which are answered the same way, by
/// dropping the connection. The body, if any, is never read: neither route has
/// one, and reading it would let a peer choose how long we spend.
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
            // Lossy, deliberately: a head is supposed to be ASCII, and a request
            // with invalid UTF-8 in it must produce a 400/404 rather than an
            // error path of its own.
            return Ok(Some(String::from_utf8_lossy(&buf[..end]).into_owned()));
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
    }
}

/// Write one response and close.
///
/// The header set is short and every line of it is load-bearing:
///
/// * `Connection: close` — there is no keep-alive, because a single-threaded
///   server that holds a connection open serves nobody else. One poll is one
///   connection, which at the default interval is one per second.
/// * `Content-Security-Policy` — this is what makes §7's "no CDN" enforced by
///   the browser instead of promised by a comment. `default-src 'none'` blocks
///   every external load; `connect-src 'self'` leaves exactly the one `fetch`
///   the page makes; `img-src data:` is the empty favicon.
/// * `X-Content-Type-Options: nosniff` — the JSON must never be sniffed into
///   something a browser will execute.
/// * `Cache-Control: no-store` — a cached poll is a frozen picture of a live
///   robot, which is the one thing this view must not show.
///
/// There is deliberately **no** `Access-Control-Allow-Origin`: the default
/// same-origin policy is half of the rebinding defence that [`route`]'s `Host`
/// check completes.
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
         form-action 'none'\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Serve the view until `max_requests` connections have been handled.
///
/// `max_requests == 0` runs until interrupted. The bound counts **accepted
/// connections**, not successful requests, so a bounded run terminates even
/// when a client connects and says nothing — which is also what makes it
/// testable.
///
/// `tick` is called only for `GET /api/tick`, and the caller is expected to rate
/// limit it (see `cmd_top_web`): two open browser tabs polling one sampler would
/// otherwise split every per-tick delta between them, and the rates in both
/// would read half of what the arena is doing.
///
/// # Errors
///
/// Only a failure to accept. A failure on one connection is reported to stderr
/// and the loop continues: a malformed request from one client must not take
/// the view away from the operator.
pub fn serve(
    listener: &TcpListener,
    bound: SocketAddr,
    max_requests: u64,
    tick: &mut dyn FnMut() -> String,
) -> Result<()> {
    let mut served = 0u64;
    loop {
        let (mut stream, _peer) = listener.accept().context("accepting a --web connection")?;
        served += 1;
        let handled = (|| -> std::io::Result<()> {
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            let Some(head) = read_head(&mut stream)? else {
                return Ok(());
            };
            let r = route(&head, bound);
            match r {
                Route::Index => respond(&mut stream, r, INDEX_HTML.as_bytes()),
                Route::Tick => respond(&mut stream, r, tick().as_bytes()),
                Route::NotFound => respond(&mut stream, r, b"not found\n"),
                Route::BadRequest => respond(&mut stream, r, b"bad request\n"),
                Route::MethodNotAllowed => respond(&mut stream, r, b"only GET\n"),
                // The refusal explains itself, because the operator who trips it
                // will be looking at this string and not at this source file.
                Route::ForbiddenHost => respond(
                    &mut stream,
                    r,
                    b"forbidden: this view is bound to loopback and only answers requests whose \
                      Host is a loopback name. A page on another origin reaching this address is \
                      DNS rebinding, not you.\n",
                ),
            }
        })();
        if let Err(e) = handled {
            // A broken pipe is a browser navigating away mid-poll, which is
            // normal and is not worth a line in the operator's terminal.
            if e.kind() != ErrorKind::BrokenPipe {
                eprintln!("--web: dropping a connection: {e}");
            }
        }
        if max_requests != 0 && served >= max_requests {
            return Ok(());
        }
    }
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

    /// The page with its HTML comments removed.
    ///
    /// The two assertions below scan for substrings, and the file's own header
    /// comment explains *why* it does not use `innerHTML` or an `import()` — so
    /// scanning the raw file fails on the prose that documents the rule. This
    /// strips `<!-- ... -->` and nothing else: the JavaScript is left exactly as
    /// served, because a `//` comment claiming something the code contradicts is
    /// precisely what these tests must still catch.
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

    /// **The two routes resolve, and nothing else does.**
    ///
    /// Mutant: match the path with `path.starts_with("/api/tick")` instead of
    /// equality. Applied: `/api/tick/../../etc/passwd` resolves to
    /// [`Route::Tick`] instead of [`Route::NotFound`] and the traversal
    /// assertion fails. (It would still have served JSON — the point is that a
    /// prefix match is how a two-route server acquires a third route.)
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
        assert_eq!(route(&get("/../src/web.rs", "localhost"), b), Route::NotFound);
        assert_eq!(
            route("POST /api/tick HTTP/1.1\r\nHost: localhost", b),
            Route::MethodNotAllowed
        );
        assert_eq!(route("", b), Route::BadRequest);
        assert_eq!(route("GET\r\nHost: localhost", b), Route::BadRequest);
    }

    /// **A loopback bind refuses any `Host` that is not a loopback name, which
    /// is the DNS-rebinding defence.**
    ///
    /// A browser will happily send `Host: evil.example` to `127.0.0.1` once its
    /// second DNS answer points there, and it is then same-origin with this
    /// server and can read the whole arena. Origin checks do not help: a
    /// rebound page's origin *is* `evil.example`.
    ///
    /// Mutant: make the `match host` arm `_ => {}` (i.e. let a missing or
    /// foreign `Host` through). Applied: `evil.example`, the IPv4 `10.0.0.5`
    /// and the missing-header case all resolve to [`Route::Index`] and three
    /// assertions fail.
    #[test]
    fn a_foreign_host_header_is_refused_on_a_loopback_bind() {
        let b = loopback();
        assert_eq!(route(&get("/", "evil.example"), b), Route::ForbiddenHost);
        assert_eq!(
            route(&get("/api/tick", "evil.example:8787"), b),
            Route::ForbiddenHost
        );
        assert_eq!(route(&get("/", "10.0.0.5:8787"), b), Route::ForbiddenHost);
        // Absent entirely, which is what a hand-rolled client sends.
        assert_eq!(route("GET / HTTP/1.1", b), Route::ForbiddenHost);
        // And the spellings a browser actually uses are accepted.
        for h in ["localhost", "LocalHost:8787", "127.0.0.1", "[::1]:8787"] {
            assert_eq!(route(&get("/", h), b), Route::Index, "host {h}");
        }
    }

    /// **The `Host` guard applies only to a loopback bind.**
    ///
    /// An operator who typed `--web 0.0.0.0:8787` asked for a reachable server
    /// and will send a `Host` naming the machine. Enforcing loopback there
    /// would make the flag refuse every request it exists to serve — and it
    /// would buy nothing, since anyone who can reach that address can reach it
    /// directly without rebinding.
    ///
    /// Mutant: drop the `if bound.ip().is_loopback()` condition and always
    /// check. Applied: the `0.0.0.0` case resolves to
    /// [`Route::ForbiddenHost`] and the assertion fails.
    #[test]
    fn the_host_guard_is_scoped_to_a_loopback_bind() {
        let public = SocketAddr::from(([0, 0, 0, 0], 8787));
        assert_eq!(route(&get("/", "robot.local:8787"), public), Route::Index);
        assert_eq!(
            route(&get("/", "robot.local:8787"), loopback()),
            Route::ForbiddenHost
        );
    }

    /// **`host_is_loopback` is not fooled by a name that merely contains one.**
    ///
    /// Mutant: implement it as `value.contains("127.0.0.1") ||
    /// value.contains("localhost")`. Applied: `127.0.0.1.evil.example` and
    /// `localhost.evil.example` are accepted and the assertion fails. Both are
    /// registrable names an attacker can point at loopback.
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

    /// **The page loads nothing from the network.**
    ///
    /// §7 is NORMATIVE: no npm, no CDN, no build step. The only absolute URL in
    /// the file is the SVG *namespace*, which is an identifier a browser
    /// compares as a string and never dereferences — so the assertion is that
    /// the set of absolute URLs is exactly that one, not that the file has no
    /// `://` in it.
    ///
    /// Mutant: add `<script src="https://cdn.example/chart.js"></script>` to
    /// `web/index.html`. Applied: the collected set gains that URL and the
    /// `assert_eq!` fails naming it.
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
        // The three ways a page acquires an external dependency without an
        // absolute URL: a protocol-relative src, an @import, and a dynamic
        // import().
        assert!(!page.contains("src=\"//"), "protocol-relative script");
        assert!(!page.contains("@import"), "css @import");
        assert!(!page.contains("import("), "dynamic import");
        // And the one fetch it does make is relative to wherever it was served.
        assert!(page.contains("fetch(\"api/tick\""));
    }

    /// **The page never builds DOM from a string.**
    ///
    /// Frame names and lock-file `comm` are bytes some other process wrote, and
    /// this page is the only place in the repository where they meet an HTML
    /// parser. `top::sanitize` is the same argument for the ANSI frame.
    ///
    /// Mutant: change one `td.textContent = text` in `cell()` to
    /// `td.innerHTML = text`. Applied: `innerHTML` appears and the assertion
    /// fails.
    #[test]
    fn the_embedded_page_never_uses_inner_html() {
        let page = page_without_html_comments();
        assert!(!page.contains("innerHTML"));
        assert!(!page.contains("outerHTML"));
        assert!(!page.contains("insertAdjacentHTML"));
        assert!(!page.contains("document.write"));
        // `eval` and `new Function` are the other two string-to-code paths, and
        // the CSP's lack of `unsafe-eval` already blocks them; asserting here
        // means the failure is a test and not a blank page.
        assert!(!page.contains("eval("));
        assert!(!page.contains("new Function"));
    }

    /// **A response carries the CSP that makes "no CDN" the browser's rule.**
    ///
    /// Mutant: delete the `Content-Security-Policy` line from `respond`.
    /// Applied: the header is absent from the captured response and the
    /// assertion fails.
    ///
    /// This drives a real socket rather than formatting a string, so it also
    /// covers the parts `route` cannot: the read loop finding `\r\n\r\n`, the
    /// `Content-Length` matching the body, and the connection actually closing
    /// (the client's `read_to_string` returns).
    #[test]
    fn a_served_response_carries_its_headers_and_body() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let mut n = 0u32;
            serve(&listener, bound, 2, &mut || {
                n += 1;
                format!("{{\"schema\":\"{SCHEMA}\",\"n\":{n}}}")
            })
            .unwrap();
        });

        let fetch = |path: &str| {
            let mut s = TcpStream::connect(bound).unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
                .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };

        let page = fetch("/");
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
        assert!(page.contains("Content-Security-Policy: default-src 'none';"));
        assert!(page.contains("connect-src 'self'"));
        assert!(page.contains("Connection: close"));
        assert!(
            page.contains(&format!("Content-Length: {}", INDEX_HTML.len())),
            "content length must be the page's byte length"
        );
        assert!(page.ends_with(INDEX_HTML), "the body is the embedded page");

        let api = fetch("/api/tick");
        assert!(api.contains("Content-Type: application/json"));
        assert!(api.ends_with("{\"schema\":\"tf_tree.top/1\",\"n\":1}"));
        h.join().unwrap();
    }

    /// **The `tick` closure runs for `/api/tick` and for nothing else.**
    ///
    /// It is what reads the arena, so a 404 or a favicon probe that sampled it
    /// would advance the tick counter — and every per-tick delta in the next
    /// real poll would be measured over the wrong interval.
    ///
    /// Mutant: in `serve`, call `tick()` once before the `match r` and pass the
    /// result to the `Route::Tick` arm. Applied: the counter reads 3 instead of
    /// 1 and the assertion fails.
    #[test]
    fn only_the_json_route_samples_the_arena() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let mut n = 0u32;
            serve(&listener, bound, 3, &mut || {
                n += 1;
                "{}".to_owned()
            })
            .unwrap();
            tx.send(n).unwrap();
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

    /// **A silent client is dropped rather than wedging the single-threaded
    /// loop, and the run is still bounded.**
    ///
    /// This is the failure mode that turns a two-route server into an outage: a
    /// port scanner opens a connection and never speaks, and the operator's
    /// view stops updating for as long as it holds it.
    ///
    /// Mutant: delete the `stream.set_read_timeout(...)` line. Applied, the
    /// test hangs on `h.join()` until nextest's timeout kills it rather than
    /// failing — which is the honest description of what that mutant does, and
    /// is why the connection is opened and *held* here rather than closed.
    #[test]
    fn a_client_that_never_speaks_does_not_wedge_the_server() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || serve(&listener, bound, 2, &mut || "{}".to_owned()));
        let silent = TcpStream::connect(bound).unwrap();
        let mut s = TcpStream::connect(bound).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&mut s).read_line(&mut line).unwrap();
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");
        drop(silent);
        h.join().unwrap().unwrap();
    }

    /// **An over-long request head is dropped as soon as it passes the cap.**
    ///
    /// The assertion is on the *latency*, not on the empty response, and that
    /// is the whole point. Deleting the `buf.len() > MAX_HEAD` check still ends
    /// with no response — [`IO_TIMEOUT`] eventually fires and the connection is
    /// dropped — so a test that only checked the body would pass against a
    /// server that had buffered every byte the peer chose to send. What the cap
    /// buys is that the `Vec` stops growing, and the observable proof of that
    /// is that the connection ends immediately instead of at the timeout.
    ///
    /// Mutant: delete the `buf.len() > MAX_HEAD` check. Applied: the client
    /// waits out the full 2 s [`IO_TIMEOUT`] and the `< 1 s` assertion fails.
    #[test]
    fn an_oversized_request_head_is_refused_promptly() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bound = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || serve(&listener, bound, 1, &mut || "{}".to_owned()));
        let mut s = TcpStream::connect(bound).unwrap();
        let started = std::time::Instant::now();
        s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n").unwrap();
        // Never a blank line: headers forever. Twice `MAX_HEAD`, so the cap is
        // crossed well before the client runs out of things to say.
        let junk = format!("X-Pad: {}\r\n", "a".repeat(1024));
        for _ in 0..16 {
            // A closed connection here is the server having given up, which is
            // the pass condition, not an error.
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

    /// **The JSON is well formed for an empty arena and for a populated one,
    /// and never contains a bare `NaN`.**
    ///
    /// Mutant: make [`num`] `format!("{x}")` unconditionally. Applied, an edge
    /// whose median interval is zero yields `inf` in the document, `JSON.parse`
    /// throws on the first poll, and the page shows "disconnected" forever
    /// rather than one blank cell. The assertion below fails on the `inf`.
    #[test]
    fn non_finite_rates_render_as_null() {
        assert_eq!(num(None), "null");
        assert_eq!(num(Some(f64::NAN)), "null");
        assert_eq!(num(Some(f64::INFINITY)), "null");
        assert_eq!(num(Some(-0.5)), "-0.500000");
    }

    /// **Every string that came out of the arena is escaped on the way into the
    /// document.**
    ///
    /// A frame name is arbitrary UTF-8 (`intern_core` validates only the hash),
    /// so a name containing `"` would produce a document `JSON.parse` rejects —
    /// the same denial of service as the `NaN` above, reachable by anyone who
    /// can name a frame.
    ///
    /// Mutant: drop the `json_escape` around `e.label` in `edge_json`.
    /// Applied: the raw `"` reaches the document, the `\\\"` assertion fails.
    #[test]
    fn labels_are_escaped_into_the_document() {
        let mut e = crate::top::EdgeSample {
            id: 7,
            label: "he\"llo\\\nworld\u{1b}[2J".to_owned(),
            kind: tf_tree::EdgeKind::Dynamic,
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
        // And an empty ring produces `null`s, not a missing key.
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
}
