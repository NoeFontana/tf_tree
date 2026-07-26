//! `tf_tree::open()` against real processes.
//!
//! This is the test the whole Phase 2 completion milestone exists for: **a
//! process that is not a child obtains the arena and reads the same transforms
//! as its creator.** Until `docs/decisions/0005` steps 1-5 landed, the only
//! transport was fd inheritance, so this could not be written at all.
//!
//! Real processes, not threads. The kernel is what makes the claims true —
//! that a `SIGKILL`ed participant's socket closes, that its lock byte is
//! released without its cooperation — and none of that is stageable in one
//! process. Children open the lock file and the socket **by path**: an
//! inherited descriptor shares the open file description, so every contention
//! assertion would pass vacuously.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// A scratch runtime directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_rv-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A spawned helper, killed on drop so a failing assertion cannot leave a
/// process holding the arena in `/tmp`.
struct Kid(Child, Option<BufReader<std::process::ChildStdout>>);

impl Kid {
    fn spawn(dir: &PathBuf, args: &[&str]) -> Kid {
        let exe = env!("CARGO_BIN_EXE_rendezvous_child");
        let child = Command::new(exe)
            .args(args)
            .env("TF_TREE_RUNTIME_DIR", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn rendezvous_child");
        Kid(child, None)
    }

    /// The child's next line. It flushes before it parks, so this returning is
    /// proof the step actually happened — no sleeps, no polling.
    fn line(&mut self) -> String {
        let reader = self
            .1
            .get_or_insert_with(|| BufReader::new(self.0.stdout.take().expect("piped stdout")));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read child line");
        line.trim_end().to_string()
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// **The milestone's headline: a foreign process joins and agrees.**
///
/// Not merely "attach succeeded" — the joiner performs a real lookup and must
/// get the transform the creator published, **bit for bit**. Attaching to the
/// wrong segment, or to a segment whose header this build misreads, would still
/// let `open()` return `Ok`; only comparing the numbers rules that out.
#[test]
fn a_foreign_process_joins_and_reads_the_same_transform() {
    let scratch = Scratch::new("join");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let published = owner.line();
    assert!(published.starts_with("owning "), "got {published}");
    let owner_value = published.strip_prefix("owning ").unwrap().to_string();

    let mut joiner = Kid::spawn(&scratch.0, &["join"]);
    let joined = joiner.line();
    assert!(
        joined.starts_with("joined "),
        "the joiner did not attach: {joined}"
    );
    let joiner_value = joined.strip_prefix("joined ").unwrap().to_string();

    assert_eq!(
        joiner_value, owner_value,
        "the joiner read a different transform than the owner published"
    );
}

/// `create = Never` fails fast when nothing is serving, rather than waiting.
///
/// The intended behaviour for a supervised consumer: a notebook or a
/// visualiser started before the robot must say so, not create an empty arena
/// that the real publisher then refuses to join (§3.2, D18).
#[test]
fn a_consumer_that_will_not_create_fails_fast_on_an_empty_machine() {
    let scratch = Scratch::new("never");
    let mut kid = Kid::spawn(&scratch.0, &["join"]);
    let line = kid.line();
    assert!(
        line.starts_with("error"),
        "expected a fast failure, got {line}"
    );
    assert!(
        line.contains("no arena"),
        "the error should name the absent arena: {line}"
    );
}
