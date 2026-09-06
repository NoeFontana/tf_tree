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
//! So this is `std::net::TcpListener`, a scoped thread per connection, no
//! keep-alive. The socket half of it — [`bind`], `read_head`, `respond`,
//! `handle`, [`serve`] and the classifiers they lean on — is **152 lines of
//! code** (this file from `bind` to the test module, blank and comment lines
//! excluded), and it cannot rot. The rest of the module is the JSON document,
//! which a server crate would not have written for us. What it costs is stated
//! in [`serve`]: it is not a general-purpose server and must never be pointed at
//! a network.
//!
//! # This is the only network socket in the repository, and it is opt-in
//!
//! §5.1 is NORMATIVE that "`tf_tree` opens no network sockets. Ever." That
//! sentence is about the *library*, and it stays true: nothing in `tf_tree`,
//! `tf_tree_core`, `tf_tree_arena` or `tf_tree_ipc` can reach this code. The
//! `AF_INET` socket lives in the CLI, is created only when an operator types
//! `--web`, and binds loopback unless that operator types a different address.
//! §11's `socket(2)`-is-only-`AF_UNIX` assertion must therefore be scoped to
//! the library's test suite; a version of it that ran over the CLI would have
//! to encode this exception, which is why the distinction is written down here
//! rather than discovered later.
//!
//! **That assertion exists since 2026-09-04** — `just no-network`,
//! `scripts/no-network.sh` — and this paragraph called it *proposed* until it
//! did. It traces the five published crates' test binaries and requires every
//! `socket(2)` in them to name `AF_UNIX`. The exception above is not encoded in
//! its scanner: it is the recipe's **positive control**, a separate `strace`
//! run over `crates/tf_tree_cli/tests/web.rs` that must find an `AF_INET`
//! socket or the whole check refuses. So a scan that had been scoped so
//! narrowly it could no longer see a network socket fails, rather than passing
//! for the same reason a correct one does — and if this module ever stopped
//! binding a listener, the check would say so.
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
/// A peer that connects and says nothing holds a thread and one of
/// [`MAX_CONNECTIONS`] slots until this fires, so it is what bounds the cost of
/// a silent socket. Two seconds is far beyond a loopback round trip — and beyond
/// a round trip through the SSH tunnel §7 recommends — and far below an
/// operator's patience.
///
/// It is *not* what keeps the view answering: that is the thread per connection
/// in [`serve`]. Before those existed this timeout was the only bound, and it
/// bounded the outage per peer rather than in aggregate — five silent sockets
/// still cost ten seconds.
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

/// How many connections may be in flight at once.
///
/// Each one costs a thread and at most [`IO_TIMEOUT`], so this is the bound on
/// what an unauthenticated local peer can make this process hold. Sixty-four is
/// far above what a browser polling once a second and a `curl` or two need, and
/// far below a thread count that matters on a robot's compute box. Past it a
/// connection is dropped without being read, which is the honest answer: the
/// alternative is queueing, and a queued poll is a stale poll.
const MAX_CONNECTIONS: usize = 64;

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
    if let Some(warning) = exposure_warning(local) {
        eprintln!("{warning}");
    }
    println!("tf_tree top --web: read-only view on http://{local}/ (Ctrl-C to stop)");
    Ok((listener, local))
}

/// The stderr line a non-loopback bind earns, or `None` for loopback.
///
/// Not an error: an operator on a robot with no display may genuinely want this
/// reachable. But §7 calls a non-loopback *default* a security bug, and an
/// explicit choice deserves to be visible in the log the operator later reads.
///
/// **This is a function and not three lines inside [`bind`] so that it is
/// testable without a socket.** §7's amendment leans on this warning as the
/// reason an explicit `0.0.0.0` is acceptable at all; a load-bearing part of a
/// security argument that no test can reach is a claim, not a mitigation.
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
/// * `Connection: close` — there is no keep-alive. A held-open connection is a
///   thread and one of [`MAX_CONNECTIONS`] slots for as long as the browser
///   feels like keeping it, in exchange for saving a loopback handshake. One
///   poll is one connection, which at the default interval is one per second.
/// * `Content-Security-Policy` — this is what makes §7's "no CDN" enforced by
///   the browser instead of promised by a comment. `default-src 'none'` blocks
///   every external load; `connect-src 'self'` leaves exactly the one `fetch`
///   the page makes; `img-src data:` is the empty favicon. `frame-ancestors
///   'none'` is listed **separately and not left to `default-src`**: it is not a
///   fetch directive, so it has no fallback, and without it any origin may
///   `<iframe>` this view. Same-origin policy still stops that page reading the
///   frame, so what it costs us is clickjacking on a page with no actions —
///   cheap to close, and the threat model here is a hostile page in the
///   operator's own browser.
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
         form-action 'none'; frame-ancestors 'none'\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Whether an `accept(2)` failure is about one peer rather than the listener.
///
/// The distinction decides whether the operator keeps their view. `accept` can
/// fail for reasons that leave the listening socket perfectly healthy:
///
/// * **`ECONNABORTED`** — the peer sent a RST between the `SYN` and our
///   `accept`. A port scanner does this all day.
/// * **`EINTR`** — a signal arrived while we were blocked.
/// * **`EMFILE`/`ENFILE`** — this process, or the machine, is momentarily out
///   of file descriptors. The next `accept` after something closes succeeds.
///
/// Treating any of those as fatal means a background scanner can kill
/// `tf_tree top --web` on a robot mid-session. Anything else — a listener that
/// has been closed, an `EBADF` — is not survivable and is propagated, because a
/// loop that retried it would spin forever printing.
#[must_use]
pub fn accept_is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
    ) || is_descriptor_exhaustion(e)
}

/// `EMFILE`/`ENFILE`, which `std` maps to no [`ErrorKind`] of its own.
///
/// The raw numbers are Linux/POSIX `errno` values and are only consulted on
/// unix, where this server is used; on any other target the `ErrorKind` arms of
/// [`accept_is_transient`] are the whole classifier.
#[must_use]
fn is_descriptor_exhaustion(e: &std::io::Error) -> bool {
    cfg!(unix) && matches!(e.raw_os_error(), Some(23 | 24))
}

/// One connection, start to finish: deadlines, head, route, response.
///
/// The deadlines are set here and not on the listener because they are
/// per-socket. The read half is the load-bearing one: a peer that connects and
/// never sends a request line holds this thread until [`IO_TIMEOUT`], and
/// [`read_head`] turns the resulting `WouldBlock`/`TimedOut` into "drop the
/// connection".
///
/// **The write deadline is defence in depth and is deliberately untested.** Both
/// bodies — the ~11 KB page and ~9 KB of JSON — fit in a default Linux send
/// buffer, so `write_all` returns without waiting for a peer that never reads
/// and there is no cheap way to drive the other case from a test. It is set
/// because "fits today" is a property of two sizes that both grow, and a
/// handler blocked in `write_all` forever would hold a [`MAX_CONNECTIONS`] slot
/// for the life of the process. The asymmetry with the read half is stated here
/// so that it reads as a decision and not as an oversight.
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
        // The refusal explains itself, because the operator who trips it will be
        // looking at this string and not at this source file.
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
/// `max_requests == 0` runs until interrupted. The bound counts **accepted
/// connections**, not successful requests, so a bounded run terminates even
/// when a client connects and says nothing — which is also what makes it
/// testable. It returns once every connection it accepted has finished.
///
/// # One thread per connection, capped at `MAX_CONNECTIONS`
///
/// The accept loop hands each socket to a scoped thread and goes straight back
/// to `accept`. **This is not throughput, it is availability.** Handling
/// connections inline costs a full `IO_TIMEOUT` per peer that connects and
/// says nothing, and those costs add: five silent sockets blanked the operator's
/// view for ten seconds, linear in the number of peers and bounded by nothing.
/// A local port scanner or a stuck `curl` loop is enough, and it lands at
/// exactly the moment somebody is watching a fault. With a thread per
/// connection, a silent peer costs one thread for two seconds and delays nobody.
///
/// `std::thread::scope` and not `spawn`: the threads borrow `tick` and the
/// listener, so the compiler is what guarantees none of them outlives this call.
/// That is also why `serve` returning means every handler has finished — a
/// bounded run cannot leave a response half-written.
///
/// Past `MAX_CONNECTIONS` in flight a connection is closed unread rather than
/// queued, and the first time that happens is reported once. A cap is not
/// optional: threads are the resource an unauthenticated peer would otherwise
/// allocate without limit.
///
/// `tick` is `&dyn Fn` rather than `&mut dyn FnMut` because handlers share it,
/// and it is called only for `GET /api/tick`. The caller is expected to rate
/// limit it *and* to serialise it (see `cmd_top_web`): two browser tabs polling
/// one sampler would otherwise split every per-tick delta between them, and the
/// rates in both would read half of what the arena is doing.
///
/// # Errors
///
/// Only a failure to accept that says the *listener* is finished — see
/// [`accept_is_transient`]. A failure on one connection is reported to stderr
/// and the loop continues: a malformed request from one client must not take
/// the view away from the operator.
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
                    // Out of descriptors is the one transient failure that
                    // repeats immediately, so it would otherwise be a hot loop
                    // printing a line per iteration. Everything else here is one
                    // peer's doing and the next `accept` blocks normally.
                    if is_descriptor_exhaustion(&e) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    // Deliberately *not* counted against `max_requests`: the
                    // bound is a number of connections handled, and no
                    // connection was.
                    continue;
                }
                Err(e) => return Err(e).context("accepting a --web connection"),
            };
            served += 1;

            // `fetch_add` and not load-then-add: the handlers decrement from
            // their own threads, so a check that is not part of the increment
            // can be overtaken between the two.
            if live.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                live.fetch_sub(1, Ordering::AcqRel);
                // Once, not per refusal: whatever is opening sockets faster than
                // this can retire them would otherwise own the operator's
                // terminal as thoroughly as it owns the port.
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
                        // A broken pipe is a browser navigating away mid-poll,
                        // which is normal and is not worth a line in the
                        // operator's terminal.
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
    /// Applied: the header is absent from the response *head* and the first
    /// assertion fails.
    ///
    /// Second mutant: delete only the `; frame-ancestors 'none'` token,
    /// leaving the rest of the policy. Applied: the `frame-ancestors`
    /// assertion fails and no other does — which is the point of asserting it
    /// separately, since `default-src 'none'` does **not** cover framing.
    ///
    /// **Every header assertion is made against `head`, never against the whole
    /// response, and that is not tidiness.** An earlier revision searched the
    /// full text, and the mutant above *survived* it: `web/index.html`'s own
    /// file comment quotes the header it is documenting
    /// (``Content-Security-Policy: default-src 'none'; connect-src 'self'``),
    /// so the served body satisfied the assertion with the header gone. A
    /// header test that a page's prose can pass is not a header test.
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
            // An atomic and not a `mut` capture: handlers share `tick`, so it is
            // `&dyn Fn` and the count has to live behind interior mutability.
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
        // `frame-ancestors` has no `default-src` fallback — it is not a fetch
        // directive — so its absence is not covered by the assertion above.
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
            let n = std::sync::atomic::AtomicU32::new(0);
            serve(&listener, bound, 3, &|| {
                n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                "{}".to_owned()
            })
            .unwrap();
            // `serve` returns only once every handler it spawned has finished,
            // so this load cannot race a still-running `fetch_add`.
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

    /// **A silent client is dropped rather than held forever, and a bounded run
    /// still returns.**
    ///
    /// This is the failure mode that turns a two-route server into an outage: a
    /// port scanner opens a connection and never speaks. The thread per
    /// connection is what keeps the *view* answering (see
    /// `silent_peers_do_not_delay_the_operators_poll`); what this pins is that
    /// [`IO_TIMEOUT`] eventually retires the socket, so a silent peer does not
    /// hold a thread and a [`MAX_CONNECTIONS`] slot for the life of the process.
    ///
    /// Mutant: delete the `stream.set_read_timeout(...)` line in `handle`.
    /// Applied: the silent handler never finishes, `std::thread::scope` cannot
    /// join it, `serve` never returns and the `recv_timeout` below fails naming
    /// it.
    ///
    /// **The deadlines on the client side are the finding, not decoration.**
    /// Called bare, `read_line` and `join` turn that mutant into a *hang*, and
    /// there is no `.config/nextest.toml` in this repository to convert a hang
    /// into a failure — `just test` would wedge with no diagnostic instead of
    /// reporting a regression. A gate that never returns is a gate that does
    /// not run.
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
        // Well above the 2 s [`IO_TIMEOUT`], and well below any patience a
        // human has for a hung test.
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&mut s)
            .read_line(&mut line)
            .expect("a real request must be answered while a silent peer is held");
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");
        // **`silent` is deliberately still open here.** Closing it first would
        // end its handler by EOF, and the read timeout — the thing under test —
        // would never have to fire.
        assert!(
            rx.recv_timeout(Duration::from_secs(20))
                .expect("IO_TIMEOUT must retire a silent connection so `serve` can return"),
            "serve returned an error"
        );
        drop(silent);
        h.join().unwrap();
    }

    /// **Silent peers cost the operator's poll nothing, however many there
    /// are.**
    ///
    /// Handling connections inline made every silent socket cost a full
    /// [`IO_TIMEOUT`] *in series*: measured on this host, one `/api/tick` took
    /// 0.008 s alone and 10.047 s behind five sockets that connected and said
    /// nothing — linear in the number of peers and bounded by nothing. A local
    /// port scanner or a stuck `curl` loop blanks the view at the moment
    /// somebody is watching a fault, and a per-connection deadline does not fix
    /// it, it only sets the slope.
    ///
    /// Mutant: in `serve`, call `handle(&mut stream, bound, tick)` inline where
    /// the `scope.spawn` is. Applied: the poll below takes ~10 s and the
    /// deadline assertion fails, naming the elapsed time.
    ///
    /// The threshold is 2 s — one whole [`IO_TIMEOUT`] — rather than something
    /// tight: what is being asserted is that the cost does not accumulate, and
    /// a loaded CI box must not be able to fail this by being slow.
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

        // Held open for the whole test: these are the peers that say nothing.
        let held: Vec<TcpStream> = (0..SILENT)
            .map(|_| TcpStream::connect(bound).unwrap())
            .collect();
        // Every one of them must have been accepted before the real request is
        // sent, or the measurement is of an empty server. `accept` is what the
        // loop does with no thread involved, so this settles in microseconds.
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

    /// **Past [`MAX_CONNECTIONS`] a connection is dropped, and the loop keeps
    /// answering.**
    ///
    /// A thread per connection is a resource an unauthenticated local peer
    /// allocates, so it has to be capped; what the cap must not do is take the
    /// view away from the operator, which is the outage it exists to prevent.
    ///
    /// Mutant: delete the `live.fetch_add(...) >= MAX_CONNECTIONS` branch, so
    /// every connection is spawned. Applied: the assertion that the excess peer
    /// gets no response fails — it is answered like any other. Second mutant:
    /// drop the `live.fetch_sub` in the handler, so the count only ever rises.
    /// Applied: the final request is refused too and the last assertion fails.
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

        // Exactly the cap, all silent, all held.
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
        // Closing a socket with unread bytes still in its receive queue sends a
        // RST, so the peer sees `ECONNRESET` rather than a clean EOF. Either is
        // "no response"; the assertion is on the bytes, which is the property.
        let _ = over.read_to_end(&mut sink);
        assert!(
            sink.is_empty(),
            "a connection past the cap must be closed unread, not answered"
        );

        // And once the silent ones retire, the next request is served normally.
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
        let h = std::thread::spawn(move || serve(&listener, bound, 1, &|| "{}".to_owned()));
        let mut s = TcpStream::connect(bound).unwrap();
        let started = std::time::Instant::now();
        s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
            .unwrap();
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

    /// **A non-finite rate renders as `null` rather than as a bare `NaN`.**
    ///
    /// `NaN` and `±Infinity` are not JSON: `JSON.parse` throws on the whole
    /// document, so one bad cell blanks the entire page and the browser shows
    /// "disconnected" forever.
    ///
    /// **Today no caller can reach that branch, and the honest claim is
    /// therefore about the guard and not about a live bug.** Both inputs are
    /// already guarded upstream: [`IntervalStats::rate_hz`] returns `None`
    /// unless `median_ns > 0` (so its quotient is at most `1e9`), and
    /// `EdgeRow::observed_hz` is `None` unless `secs > 0.0`. What this test pins
    /// is that adding a *third* rate — a mean interval, a ratio of two counters,
    /// an error rate whose denominator is `lookups_ok` — cannot introduce a
    /// division by zero into the document without going through [`num`].
    ///
    /// Mutant: make [`num`] `format!("{x}")` unconditionally. Applied: the
    /// `NaN` and `INFINITY` assertions below fail. (No *integration* test dies,
    /// which is exactly the point above.)
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

    /// **A non-loopback bind produces a warning that says what it exposed.**
    ///
    /// §7's amendment offers this warning as the reason an explicit `0.0.0.0`
    /// is acceptable rather than refused, which makes it part of a security
    /// argument — and nothing reached it while it was three lines inside
    /// [`bind`], because `bind` needs a socket and `eprintln!` needs a captured
    /// stderr.
    ///
    /// Mutant: delete the `if local.ip().is_loopback() { return None; }` guard
    /// in [`exposure_warning`], so every bind warns. Applied: the two loopback
    /// assertions fail. Inverse mutant: return `None` unconditionally — the
    /// `0.0.0.0` assertion fails, which is the case that matters, since that is
    /// the mutant a reviewer found surviving the whole suite.
    #[test]
    fn a_non_loopback_bind_warns_and_a_loopback_one_does_not() {
        assert_eq!(exposure_warning(loopback()), None);
        assert_eq!(
            exposure_warning(SocketAddr::from(([127, 0, 0, 9], 1))),
            None
        );
        let w = exposure_warning(SocketAddr::from(([0, 0, 0, 0], 8787)))
            .expect("a wildcard bind must warn");
        // The three things the operator needs from it: what was bound, what it
        // gives away, and what to do instead.
        assert!(w.contains("0.0.0.0:8787"), "{w}");
        assert!(w.contains("no authentication"), "{w}");
        assert!(w.contains(DEFAULT_ADDR), "{w}");
        let w = exposure_warning(SocketAddr::from(([10, 0, 0, 5], 80)))
            .expect("a routable bind must warn");
        assert!(w.contains("10.0.0.5:80"), "{w}");
    }

    /// **An `accept(2)` failure that is about one peer does not end the view.**
    ///
    /// `ECONNABORTED` is what a port scanner that RSTs between `SYN` and
    /// `accept` produces, and `EMFILE` is a transient descriptor shortage;
    /// neither says the listening socket is broken, and treating either as
    /// fatal lets a background scanner kill `tf_tree top --web` on a robot
    /// mid-session.
    ///
    /// **What this pins is the classifier, not the loop.** `ECONNABORTED`
    /// cannot be provoked deterministically from a test on Linux, so `serve`'s
    /// use of [`accept_is_transient`] is by inspection; making the predicate a
    /// named function is what puts the decision somewhere a test can reach at
    /// all.
    ///
    /// Mutant: drop the `ConnectionAborted` arm. Applied: the first assertion
    /// fails. Second mutant: make [`is_descriptor_exhaustion`] `false`.
    /// Applied: the `EMFILE`/`ENFILE` assertions fail.
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
        // And the ones that mean the listener itself is finished. Retrying
        // these would be an unkillable hot loop printing a line per iteration.
        assert!(!accept_is_transient(&Error::from(ErrorKind::InvalidInput)));
        assert!(!accept_is_transient(&Error::from(
            ErrorKind::PermissionDenied
        )));
        assert!(!accept_is_transient(&Error::from(ErrorKind::Other)));
    }

    /// **The page reads the server's `selected`, so `--edge` reaches the
    /// browser.**
    ///
    /// `tick_json` has served `"selected"` since the view landed and the page
    /// ignored it: selection was initialised to `null` and pinned to
    /// `d.edges[0]` by the first `renderHistogram`. `--edge 5` served
    /// `"selected":5` and drew edge 1. Every server-side test passed, because
    /// the whole defect lived in one JavaScript identifier that was never
    /// mentioned.
    ///
    /// Mutant: delete the `seed(d);` call at the top of `paint`, or the
    /// `if (d.selected !== null ...)` assignment inside `seed`. Applied: the
    /// corresponding assertion below fails.
    ///
    /// Second mutant: make `seed` re-read `d.selected` on every document (drop
    /// the `seeded` guard). Applied: the "read once" assertion fails — and in a
    /// browser the page would drag the selection back to `--edge`'s row one
    /// poll after every click.
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
        // Read once: the flag says where to *start*, and the click handler owns
        // it afterwards.
        assert!(page.contains("if (seeded) return;"), "seeding must be once");
        assert!(page.contains("seeded = true;"));
        // And the fallback that makes an unknown or absent id harmless is still
        // there, since `--edge` naming a tombstoned edge must not blank the
        // pane.
        assert!(page.contains("|| d.edges[0]"), "the fallback must remain");
    }
}
