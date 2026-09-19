//! `tf_tree top --web` end to end through the shipped binary
//! (`docs/PHASE5.md` §7).
//!
//! Fetches what the page fetches and validates it with [`json`], so a missing
//! comma in `tick_json` fails here. Not `--attach`: runs in the default build,
//! against `top`'s in-process fixture (24 frames, 24 edges, 500 stamps per
//! dynamic edge).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

/// Whether `s` is one complete, strictly well-formed JSON value (as
/// `JSON.parse` sees it), with no trailing bytes.
///
/// Hand-written to keep `serde_json` out of the dependency budget.
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
        // An unquoted key is what a `format!` typo produces.
        if b.get(*i) != Some(&b'"') || !string(b, i) || !eat(b, i, b':') || !value(b, i) {
            return false;
        }
        if eat(b, i, b',') {
            // A trailing comma before `}`.
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
            // A raw control byte is illegal in a JSON string (`json_escape`).
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

/// Kills the child on drop, so a failed assertion leaks no server.
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

/// The document a real arena produces is valid JSON and populated.
#[test]
fn the_served_document_is_valid_json_over_a_real_arena() {
    let mut s = start(2, 50);
    let index = get(s.port, "/");
    assert!(index.starts_with("HTTP/1.1 200 OK\r\n"), "{index}");

    let resp = get(s.port, "/api/tick");
    let (head, body) = split(&resp);
    assert!(head.contains("Content-Type: application/json"), "{head}");
    assert!(json(body), "the served document is not valid JSON:\n{body}");

    // Non-degenerate: every optional field is populated.
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
    // The two `null` spellings survive `JSON.parse`, not bare `NaN` or `4294967295`.
    assert!(
        body.contains("\"observed_hz\":null"),
        "first tick has no rate"
    );
    assert!(body.contains("\"selected\":null"), "no --edge was given");

    let out = s.child.wait().expect("wait");
    assert!(out.success(), "the bounded run must exit 0");
}

/// Two polls inside one interval see the same tick; the next interval
/// advances it.
#[test]
fn polls_inside_one_interval_share_a_tick_and_the_next_advances_it() {
    // 500 ms: above the 50 ms floor and two loopback round trips.
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

/// The validator is strict where `JSON.parse` is strict.
#[test]
fn the_json_validator_rejects_what_json_parse_rejects() {
    assert!(json("{}"));
    // The nested shape `tick_json` emits, with a `\u001b` escape.
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

/// The served page is byte-identical to the embedded file (`Content-Length`
/// agrees with the body).
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

/// `--edge` resolves against the arena and reaches the document. The page half
/// is `web::tests::the_page_seeds_its_selection_from_the_served_selected`.
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
    // `gps_link` is not the page's fallback `edges[0]`.
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
