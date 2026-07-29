//! `tf_tree top --web` end to end, through `clap` and the shipped binary
//! (`docs/PHASE5.md` §7).
//!
//! The unit tests in `src/web.rs` cover routing, the `Host` guard and the
//! response headers — but every one of them hands [`tf_tree_cli::web::serve`] a
//! stub closure that returns a constant. **Nothing there ever runs
//! `tick_json` against a real arena**, which is the half a browser actually
//! consumes: ~9 KB of hand-formatted JSON with one nested object per edge, one
//! per participant, one per histogram bucket, and `null` in six places. The
//! failure mode of hand-formatted JSON is a missing comma, and a missing comma
//! is invisible to every assertion in that file.
//!
//! So this file runs the process, fetches what the page fetches, and parses it
//! with [`json`] — a validator, not a matcher, so an unbalanced brace or a
//! trailing comma anywhere in the document fails here rather than as a blank
//! page on somebody's robot.
//!
//! **Deliberately not `--attach`**: like `tests/top.rs`, this runs in the
//! default build with no `shm`, so it is in `cargo nextest run --workspace` —
//! the gate that runs on every commit. The arena is `top`'s in-process fixture,
//! which is non-degenerate on purpose: 24 frames, 24 edges, and dynamic edges
//! retaining 500 stamps each, so `stats`, `histogram` and the participant table
//! are all populated rather than `null`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

// ---------------------------------------------------------------------------
// A JSON validator
// ---------------------------------------------------------------------------

/// Whether `s` is one complete, well-formed JSON value and nothing else.
///
/// Hand-written for the same reason `catalogue::render_json` is: pulling
/// `serde_json` in as a dev-dependency to check ~9 KB of output would put a
/// derive-macro tree in the lockfile of a workspace whose dependency budget is a
/// stated hard rule. This is a validator and not a parser — it builds no tree,
/// because every *value* assertion below is better made against the raw text,
/// where the assertion names the key it is about.
///
/// It is deliberately strict where `JSON.parse` is strict, since `JSON.parse` is
/// the real consumer: no trailing commas, no bare `NaN`/`Infinity`, no unquoted
/// keys, no single quotes, and no trailing bytes after the top-level value.
fn json(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0usize;
    if !value(b, &mut i) {
        return false;
    }
    ws(b, &mut i);
    i == b.len()
}

fn ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
        *i += 1;
    }
}

fn eat(b: &[u8], i: &mut usize, c: u8) -> bool {
    ws(b, i);
    if *i < b.len() && b[*i] == c {
        *i += 1;
        true
    } else {
        false
    }
}

fn value(b: &[u8], i: &mut usize) -> bool {
    ws(b, i);
    let Some(&c) = b.get(*i) else { return false };
    match c {
        b'{' => object(b, i),
        b'[' => array(b, i),
        b'"' => string(b, i),
        b't' => lit(b, i, b"true"),
        b'f' => lit(b, i, b"false"),
        b'n' => lit(b, i, b"null"),
        b'-' | b'0'..=b'9' => number(b, i),
        _ => false,
    }
}

fn lit(b: &[u8], i: &mut usize, want: &[u8]) -> bool {
    if b[*i..].starts_with(want) {
        *i += want.len();
        true
    } else {
        false
    }
}

fn object(b: &[u8], i: &mut usize) -> bool {
    *i += 1; // '{'
    if eat(b, i, b'}') {
        return true;
    }
    loop {
        ws(b, i);
        // An unquoted key is the shape a `format!` typo produces, and
        // `JSON.parse` rejects it.
        if b.get(*i) != Some(&b'"') || !string(b, i) || !eat(b, i, b':') || !value(b, i) {
            return false;
        }
        if eat(b, i, b',') {
            // A trailing comma before `}` — the other `format!` typo.
            continue;
        }
        return eat(b, i, b'}');
    }
}

fn array(b: &[u8], i: &mut usize) -> bool {
    *i += 1; // '['
    if eat(b, i, b']') {
        return true;
    }
    loop {
        if !value(b, i) {
            return false;
        }
        if eat(b, i, b',') {
            continue;
        }
        return eat(b, i, b']');
    }
}

fn string(b: &[u8], i: &mut usize) -> bool {
    *i += 1; // '"'
    while let Some(&c) = b.get(*i) {
        match c {
            b'"' => {
                *i += 1;
                return true;
            }
            b'\\' => {
                let Some(&e) = b.get(*i + 1) else {
                    return false;
                };
                match e {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => *i += 2,
                    b'u' => {
                        if b.len() < *i + 6 || !b[*i + 2..*i + 6].iter().all(u8::is_ascii_hexdigit)
                        {
                            return false;
                        }
                        *i += 6;
                    }
                    _ => return false,
                }
            }
            // A raw control byte is illegal in a JSON string, and a frame name
            // is arbitrary UTF-8 — this is the case `json_escape` exists for.
            0x00..=0x1f => return false,
            _ => *i += 1,
        }
    }
    false
}

fn number(b: &[u8], i: &mut usize) -> bool {
    let start = *i;
    if b.get(*i) == Some(&b'-') {
        *i += 1;
    }
    let digits = *i;
    while matches!(b.get(*i), Some(b'0'..=b'9')) {
        *i += 1;
    }
    if *i == digits {
        return false;
    }
    if b.get(*i) == Some(&b'.') {
        *i += 1;
        let frac = *i;
        while matches!(b.get(*i), Some(b'0'..=b'9')) {
            *i += 1;
        }
        if *i == frac {
            return false;
        }
    }
    if matches!(b.get(*i), Some(b'e' | b'E')) {
        *i += 1;
        if matches!(b.get(*i), Some(b'+' | b'-')) {
            *i += 1;
        }
        let exp = *i;
        while matches!(b.get(*i), Some(b'0'..=b'9')) {
            *i += 1;
        }
        if *i == exp {
            return false;
        }
    }
    *i > start
}

// ---------------------------------------------------------------------------
// Driving the server
// ---------------------------------------------------------------------------

/// Kills the child on the way out, so a failed assertion does not leave a
/// server holding a port for the rest of the run.
///
/// `--iterations` bounds a *successful* run, which is not the case that leaks.
struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start `tf_tree top --web 127.0.0.1:0` and learn the port it chose.
///
/// Port 0 rather than a fixed one: two tests running concurrently under nextest
/// would collide on any constant, and the whole point of printing the resolved
/// address is that `:0` is a usable spelling.
fn start(iterations: u32, interval_ms: u32) -> Server {
    start_with(iterations, interval_ms, &[])
}

/// [`start`] plus whatever other flags the test is about.
fn start_with(iterations: u32, interval_ms: u32, extra: &[&str]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args([
            "top",
            "--web",
            "127.0.0.1:0",
            "--iterations",
            &iterations.to_string(),
            "--interval",
            &interval_ms.to_string(),
        ])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tf_tree top --web");

    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut line)
        .expect("read the announced URL");
    let port = line
        .split("http://127.0.0.1:")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no URL on the first stdout line: {line:?}"));
    assert_ne!(port, 0, "the printed port must be the resolved one: {line}");
    Server { child, port }
}

/// One `GET`, one connection, the whole response as text.
fn get(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .expect("write request");
    let mut out = Vec::new();
    s.read_to_end(&mut out).expect("read response");
    String::from_utf8_lossy(&out).into_owned()
}

/// Split a response into its head and its body.
fn split(resp: &str) -> (&str, &str) {
    resp.split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in:\n{resp}"))
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// **The document a real arena produces is valid JSON, and it is populated.**
///
/// This is the assertion no unit test in `src/web.rs` can make: they all stub
/// the sampler, so `tick_json`'s ~9 KB of hand-written commas and braces —
/// nested three deep, with `null` in six places — is never once parsed.
///
/// Mutant: in `web::tick_json`, delete the `s.push(',')` that separates two
/// edges (the one in the `c.edges.iter().zip(...)` loop). Applied: the body is
/// `...}{...` between the first two edges, `json()` returns false and the first
/// assertion fails naming the body. `JSON.parse` fails the same way, and the
/// page a browser would show says "disconnected" forever.
///
/// Second mutant: pass `0` for `HIST_BUCKETS`. Applied: every `histogram` is
/// `[]`, and the "the fixture must produce bars" assertion fails — which is
/// what keeps this test from passing against an empty document.
#[test]
fn the_served_document_is_valid_json_over_a_real_arena() {
    let mut s = start(2, 50);
    let index = get(s.port, "/");
    assert!(index.starts_with("HTTP/1.1 200 OK\r\n"), "{index}");

    let resp = get(s.port, "/api/tick");
    let (head, body) = split(&resp);
    assert!(head.contains("Content-Type: application/json"), "{head}");
    assert!(json(body), "the served document is not valid JSON:\n{body}");

    // Non-degenerate: the fixture is 24 frames / 24 edges with 500 retained
    // stamps on each dynamic edge, so every optional field is *populated* here.
    // A document of nothing but `null`s would satisfy `json()` and prove
    // nothing.
    assert!(body.contains("\"schema\":\"tf_tree.top/1\""), "{body}");
    assert!(body.contains("\"kind\":\"dynamic\""), "{body}");
    assert!(
        body.contains("\"stats\":{\"n\":"),
        "stats must be populated"
    );
    assert!(
        body.contains("\"histogram\":[{\"lo_ns\":"),
        "the fixture must produce bars"
    );
    assert!(body.contains("\"occupancy\":[{\"what\":"), "{body}");
    // And the two `null` spellings that must survive a round trip through
    // `JSON.parse` rather than appearing as bare `NaN` or `4294967295`.
    assert!(
        body.contains("\"observed_hz\":null"),
        "first tick has no rate"
    );
    assert!(body.contains("\"selected\":null"), "no --edge was given");

    let out = s.child.wait().expect("wait");
    assert!(out.success(), "the bounded run must exit 0");
}

/// **Two polls inside one interval see the same tick, and the next interval
/// advances it.**
///
/// Both halves matter and they fail in opposite directions. One [`Sampler`]
/// holds all the per-tick state, so *without* the cache two browser tabs at
/// 1 Hz take alternate observations and every rate on both pages reads half of
/// what the arena is doing — wrong, silently, with no error anywhere. *With* a
/// cache that never expires, the view is frozen, which is the one thing a live
/// view must not be.
///
/// Mutant: in `cmd_top_web`, delete the `if now.duration_since(*at) < interval`
/// early return. Applied: the second poll reports `"tick":2` and the
/// same-tick assertion fails.
///
/// Second mutant: return the cached document unconditionally (drop the
/// duration comparison). Applied: the third poll still reports `"tick":1` and
/// the advance assertion fails.
#[test]
fn polls_inside_one_interval_share_a_tick_and_the_next_advances_it() {
    // 500 ms, comfortably above the 50 ms floor and above the time two
    // loopback round trips take, so the first two polls are inside one tick
    // whatever the machine is doing.
    let interval_ms = 500;
    let mut s = start(3, interval_ms);

    let first = get(s.port, "/api/tick");
    let second = get(s.port, "/api/tick");
    assert!(first.contains("\"tick\":1"), "{first}");
    assert!(
        second.contains("\"tick\":1"),
        "a poll inside the interval must be answered from the previous document"
    );

    std::thread::sleep(std::time::Duration::from_millis(
        u64::from(interval_ms) + 200,
    ));
    let third = get(s.port, "/api/tick");
    assert!(
        third.contains("\"tick\":2"),
        "the cache must expire at the interval, or the view is frozen:\n{third}"
    );
    let (_, body) = split(&third);
    assert!(json(body), "{body}");

    let out = s.child.wait().expect("wait");
    assert!(out.success());
}

/// **The validator is strict where `JSON.parse` is strict.**
///
/// Without this the test above is only as good as a validator nobody checked,
/// and the cheapest way to make a hand-written one pass everything is to write
/// one that accepts everything.
///
/// Mutant: make `json` `s.starts_with('{')`. Applied: six of the seven
/// rejection cases below are accepted and their assertions fail (`[1,2,]` is
/// the one that survives it, since it does not start with `{` — which is the
/// point of keeping an array case in the list).
#[test]
fn the_json_validator_rejects_what_json_parse_rejects() {
    assert!(json("{}"));
    // The nested shape `tick_json` emits, including the `\u001b` escape
    // `json_escape` produces for an ESC that arrived inside a frame name.
    assert!(json(
        r#"{"a":[1,-2.5,1e9,null,true],"b":{"c":"he\"llo\\\u001b[2J"}}"#
    ));
    assert!(json(" [ ] "));

    assert!(!json(r#"{"a":1,}"#), "trailing comma in an object");
    assert!(!json(r#"[1,2,]"#), "trailing comma in an array");
    assert!(!json(r#"{a:1}"#), "unquoted key");
    assert!(!json(r#"{"a":NaN}"#), "bare NaN — the non-finite rate case");
    assert!(!json(r#"{"a":1}{"b":2}"#), "two values, one document");
    assert!(!json(r#"{"a":1"#), "unterminated object");
    assert!(!json("{\"a\":\"raw\nnewline\"}"), "raw control byte");
}

/// **The page the server hands out is byte-identical to the embedded file.**
///
/// `Content-Length` and the body are computed separately, and a mismatch is the
/// classic hand-rolled-HTTP bug: a browser hangs waiting for bytes that never
/// come, or truncates the script mid-function and shows a blank page.
///
/// Mutant: in `web::respond`, write `body.len() + 1` as the `Content-Length`.
/// Applied: `read_to_end` returns when the server closes, the body is one byte
/// short of what the header promised, and the length assertion fails. (A real
/// browser would instead report a network error, which is why this is checked
/// against the number and not against a rendered page.)
#[test]
fn the_page_is_served_whole_and_matches_the_embedded_file() {
    let mut s = start(1, 50);
    let resp = get(s.port, "/");
    let (head, body) = split(&resp);
    assert!(
        head.contains(&format!(
            "Content-Length: {}",
            tf_tree_cli::web::INDEX_HTML.len()
        )),
        "{head}"
    );
    assert_eq!(body, tf_tree_cli::web::INDEX_HTML);
    assert_eq!(body.len(), tf_tree_cli::web::INDEX_HTML.len());
    let out = s.child.wait().expect("wait");
    assert!(out.success());
}

/// **`--edge` resolves against the arena and reaches the document.**
///
/// The flag is documented as seeding the page's selection, and until this test
/// existed the whole path was untested from either end: `cmd_top_web` resolved
/// the label and the page ignored the field. Replacing the resolution with
/// `let selected: Option<u32> = None;` passed the entire suite.
///
/// `gps_link` is deliberately not the first edge — the fixture puts it at id 10
/// of 23 — because the page's fallback when nothing is selected is `edges[0]`,
/// so a fixture that selected edge 1 would pass with the seeding deleted.
///
/// Mutant: in `cmd_top_web`, replace the `selected_at_start.and_then(...)`
/// block with `None`. Applied: the document carries `"selected":null` and the
/// first assertion fails. Second mutant: `--edge` matched with `==` on the
/// label instead of `contains` (`top::select_edge_index`) — applied, no edge
/// matches `gps_link`, `"selected":null` again, same failure.
///
/// The page half of this — that `index.html` reads `d.selected` — is pinned by
/// `web::tests::the_page_seeds_its_selection_from_the_served_selected`, since
/// there is no JavaScript engine in this workspace to run the page in.
#[test]
fn the_edge_flag_seeds_the_documents_selection() {
    let mut s = start_with(1, 50, &["--edge", "gps_link"]);
    let resp = get(s.port, "/api/tick");
    let (_, body) = split(&resp);
    assert!(json(body), "{body}");
    assert!(
        body.contains("\"selected\":10"),
        "--edge gps_link must resolve to the fixture's edge 10:\n{body}"
    );
    // Non-degenerate in the direction that matters: the edge it names is really
    // in the document, and it is not the one the page would have fallen back to.
    assert!(
        body.contains("\"id\":10,\"label\":\"base_link->gps_link"),
        "{body}"
    );
    assert!(
        body.starts_with('{') && body.contains("\"id\":1,\"label\":"),
        "{body}"
    );

    let out = s.child.wait().expect("wait");
    assert!(out.success());
}
