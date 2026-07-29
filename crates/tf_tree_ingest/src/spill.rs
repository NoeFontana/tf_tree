//! §3.1's spill-to-run-file and k-way merge.
//!
//! # The one case grouping cannot serve
//!
//! [`crate::ingest::fill`] bounds pass two by splitting the edges into groups
//! that each fit `--max-memory` and re-reading the recording once per group.
//! That works for every recording whose *largest single edge* fits the cap, and
//! it is strictly better than a run file when it applies: no temporary file to
//! leak, to fill a different filesystem, or to leave behind when the process is
//! killed.
//!
//! It cannot work when one edge alone exceeds the cap, because grouping's
//! smallest unit is an edge. That case is what this module is for, and it is
//! what §3.1 means by "spill to a temporary run-file with a k-way merge beyond
//! that". Until this module existed the case was refused with an error naming
//! the edge; that error variant is gone, because the case is now served.
//!
//! # Shape
//!
//! Textbook external merge sort:
//!
//! 1. **Spill.** Read the recording, keeping only the oversized edge, into a
//!    buffer sized to the cap. Every time it fills, **stable**-sort it and
//!    append it to the run file as one *run*.
//! 2. **Reduce.** While there are more runs than [`fan_in`] allows, merge them
//!    [`fan_in`] at a time into a fresh file, each merge producing one longer
//!    run. The old file is dropped — and therefore deleted — at the end of every
//!    pass, so disk use is bounded by two consecutive files rather than by the
//!    number of passes.
//! 3. **Merge.** Merge what is left and push it into the arena in stamp order.
//!
//! Step 2 is what makes `--max-memory` a bound rather than an aspiration. A
//! single-pass merge has to hold at least one sample per run resident, so with
//! more runs than `cap / 64` it exceeds the cap *by construction*, and the cap
//! is precisely the promise this module exists to keep. The regime is not
//! hypothetical for a small cap: 600 samples at a 1 KiB cap already produce more
//! runs than a single pass can hold.
//!
//! # Ties break by run index, and that is what keeps "last wins" meaning it
//!
//! §3.2 resolves a duplicate `(edge, stamp)` to the **last occurrence in the
//! recording**. In memory that falls out of a stable sort. Here it has to
//! survive being cut into runs, so two rules hold together: each run is
//! stable-sorted internally, and runs are written and merged in recording order,
//! so for two equal stamps the lower run index is the earlier sample. The heap
//! therefore orders by `(stamp, run index)`, and because a reduce pass merges a
//! *contiguous* window of runs into one, the property is preserved across every
//! pass: the merged stream is exactly the stream a single stable sort of the
//! whole edge would have produced.
//!
//! # The file is unlinked as soon as it exists, where the platform allows
//!
//! On Unix a file unlinked while open keeps its descriptor valid and its blocks
//! are reclaimed by the kernel when the process dies, however it dies. That is
//! the property that makes a spill file safe to hand a user who is already
//! worried about a 4-hour recording: `SIGKILL` during ingest leaves nothing
//! behind. Where the unlink fails (Windows refuses it for an open file), the
//! [`Drop`] impl removes the path instead, which covers everything except a hard
//! kill.

use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::IngestError;

/// One buffered sample: an `i64` stamp beside the canonical `[f64; 7]` pose.
///
/// Kept structurally identical to what `ingest::fill` buffers in memory, so the
/// two paths cost the same per sample and `ingest::SAMPLE_BYTES` describes both.
pub(crate) type Sample = (i64, [f64; 7]);

/// Encoded width of one [`Sample`] on disk: `i64` + 7 × `f64`, little-endian.
///
/// A constant rather than `size_of::<Sample>()`, for the reason
/// `FROZEN_HEADER_SIZE` is a constant in `tf_tree_arena::frozen`: a file layout
/// that follows a compiler's padding decision is a file layout nothing checks.
/// It happens to equal `size_of::<Sample>()` today, and a test below asserts
/// that, so the two cannot drift apart silently.
pub(crate) const ENCODED: usize = 8 + 7 * 8;

/// The smallest cap this path can honour, and the value a smaller one is raised
/// to.
///
/// Below it the floors do not fit: a merge holds at least one sample per run and
/// a reduce pass holds two staging buffers, and there is a cap under which those
/// three alone exceed it. Rather than let the arithmetic quietly go negative,
/// the cap is raised here — and the ingest report prints the *measured* peak, so
/// a user who asked for 200 bytes is told what was really used rather than what
/// they asked for.
const MIN_CAP: u64 = 16 * ENCODED as u64;

/// Ceiling on one encode/decode staging buffer.
const MAX_STAGING: u64 = 64 * 1024;

/// The share of the cap the sample windows may use during a merge. The rest
/// pays for the staging buffers, which are real memory: samples are
/// `(i64, [f64; 7])` in memory and little-endian bytes on disk, and nothing can
/// convert between the two views without `unsafe`, which this crate forbids.
///
/// Three quarters, against `2 × staging ≤ cap / 4` from [`staging_of`], is what
/// makes `windows + staging ≤ cap` hold for every cap at or above [`MIN_CAP`]
/// and every run count at or below [`fan_in`]. `budget_fits_the_cap` checks it
/// over the whole grid rather than trusting the algebra.
const WINDOW_SHARE_NUM: u64 = 3;
const WINDOW_SHARE_DEN: u64 = 4;

fn cap_of(user: u64) -> u64 {
    user.max(MIN_CAP)
}

fn staging_of(cap: u64) -> u64 {
    (cap / 8).clamp(ENCODED as u64, MAX_STAGING)
}

fn clamp_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// `(samples per run, staging bytes)` for the spill phase.
pub(crate) fn spill_budget(user_cap: u64) -> (usize, usize) {
    let cap = cap_of(user_cap);
    let staging = staging_of(cap);
    let run = ((cap - staging) / ENCODED as u64).max(1);
    (clamp_usize(run), clamp_usize(staging))
}

/// How many runs one merge may consume at once.
///
/// Chosen so that [`merge_window_samples`] never has to floor at one sample:
/// below this many runs the window arithmetic still has room, above it the
/// merge would exceed the cap and a reduce pass is required instead.
pub(crate) fn fan_in(user_cap: u64) -> usize {
    let cap = cap_of(user_cap);
    let windows = cap * WINDOW_SHARE_NUM / WINDOW_SHARE_DEN / ENCODED as u64;
    // `- 1` for the same reason `merge_window_samples` divides by `runs + 1`:
    // the decode staging is one window wide and is live at the same instant.
    clamp_usize(windows.saturating_sub(1).max(2))
}

/// Samples each run's read window may hold when merging `runs` of them.
///
/// Divided by `runs + 1`, not `runs`: the shared decode staging is one window
/// wide and is resident alongside every window during a refill. Dividing by
/// `runs` overshoots by a factor of `(runs + 1) / runs` — 50 % at two runs — and
/// that is exactly how the first revision of this module broke its own cap.
pub(crate) fn merge_window_samples(user_cap: u64, runs: usize) -> usize {
    let cap = cap_of(user_cap);
    let divisor = (runs as u64).saturating_add(1);
    clamp_usize((cap * WINDOW_SHARE_NUM / WINDOW_SHARE_DEN / ENCODED as u64 / divisor).max(1))
}

fn io(e: &std::io::Error) -> IngestError {
    IngestError::Spill {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    }
}

/// Decode one sample from exactly [`ENCODED`] bytes.
///
/// Every caller feeds it a `chunks_exact(ENCODED)` element, so the fixed-width
/// `copy_from_slice`s below cannot mismatch. That is why this takes a slice and
/// not an `Option`: an unreachable error variant is worse than a length
/// invariant stated once and held at its one call site.
fn decode(b: &[u8]) -> Sample {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[..8]);
    let stamp = i64::from_le_bytes(w);
    let mut pose = [0.0f64; 7];
    for (i, out) in pose.iter_mut().enumerate() {
        w.copy_from_slice(&b[8 + i * 8..16 + i * 8]);
        *out = f64::from_le_bytes(w);
    }
    (stamp, pose)
}

/// A path that removes itself, holding `None` once the file has been unlinked.
struct TempPath(Option<PathBuf>);

impl Drop for TempPath {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// One run's extent in the file: `(byte offset, sample count)`.
pub(crate) type RunSpan = (u64, u64);

/// A temporary file holding sorted runs, back to back.
///
/// **One file for all the runs of a pass**, not a file per run: the merge seeks
/// between runs anyway, so N files would buy nothing and cost N descriptors, N
/// unlink races and N chances to leave something behind.
pub(crate) struct RunFile {
    file: File,
    /// Kept only for its `Drop`, which is what removes the file on a platform
    /// where the unlink at creation could not.
    _path: TempPath,
    runs: Vec<RunSpan>,
    /// Bytes written so far, which is also the offset of the next run.
    written: u64,
    staging: Vec<u8>,
    /// `(start offset, samples so far)` of the run currently being appended.
    open_run: Option<RunSpan>,
}

impl RunFile {
    /// Create a run file in `dir`, unlinking it immediately where possible.
    ///
    /// Opened for **reading as well as writing**: the merge reads the runs back
    /// through this same descriptor. `File::create` alone yields a write-only
    /// descriptor and every read fails `EBADF` — which is exactly how the first
    /// revision of this module failed its own round-trip test.
    ///
    /// The name carries the process id and a caller-supplied tag so two ingests
    /// in one process cannot collide during the window between `create` and the
    /// unlink, short as it is.
    pub(crate) fn create(dir: &Path, tag: usize, staging: usize) -> Result<RunFile, IngestError> {
        let path = dir.join(format!("tf_tree_ingest_spill_{}_{tag}", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| io(&e))?;
        // Best-effort: on Unix this leaves an unnamed file the kernel reclaims
        // unconditionally. Where it fails, `TempPath` is the fallback, so the
        // error is deliberately not propagated.
        let path = TempPath(if std::fs::remove_file(&path).is_ok() {
            None
        } else {
            Some(path)
        });
        Ok(RunFile {
            file,
            _path: path,
            runs: Vec::new(),
            written: 0,
            staging: Vec::with_capacity(staging.max(ENCODED)),
            open_run: None,
        })
    }

    /// Start a run. Samples appended after this are one run until [`end_run`].
    ///
    /// [`end_run`]: RunFile::end_run
    pub(crate) fn begin_run(&mut self) {
        // `written + staging.len()`, not `written`: the staging buffer may hold
        // bytes that have not reached the file yet, and they precede this run.
        // `written` alone is right only while every caller pairs `begin_run`
        // with `end_run`, and a span that silently overlaps the previous run is
        // not the failure to leave to a convention.
        self.open_run = Some((self.written + self.staging.len() as u64, 0));
    }

    /// Append one sample to the open run.
    pub(crate) fn append(&mut self, s: Sample) -> Result<(), IngestError> {
        if self.staging.len() + ENCODED > self.staging.capacity() {
            self.flush()?;
        }
        self.staging.extend_from_slice(&s.0.to_le_bytes());
        for v in s.1 {
            self.staging.extend_from_slice(&v.to_le_bytes());
        }
        if let Some((_, n)) = &mut self.open_run {
            *n += 1;
        }
        Ok(())
    }

    /// Close the open run, recording it. An empty run is dropped rather than
    /// recorded, so the merge never sees a run it cannot seed.
    pub(crate) fn end_run(&mut self) -> Result<(), IngestError> {
        self.flush()?;
        if let Some(span) = self.open_run.take() {
            if span.1 > 0 {
                self.runs.push(span);
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IngestError> {
        if self.staging.is_empty() {
            return Ok(());
        }
        self.file.write_all(&self.staging).map_err(|e| io(&e))?;
        self.written += self.staging.len() as u64;
        self.staging.clear();
        Ok(())
    }

    /// Append an already-sorted slice as one run.
    pub(crate) fn write_run(&mut self, samples: &[Sample]) -> Result<(), IngestError> {
        self.begin_run();
        for &s in samples {
            self.append(s)?;
        }
        self.end_run()
    }

    /// How many runs this file holds.
    pub(crate) fn runs(&self) -> usize {
        self.runs.len()
    }

    /// The runs' extents, for handing back to [`merge_runs`](RunFile::merge_runs).
    pub(crate) fn spans(&self) -> Vec<RunSpan> {
        self.runs.clone()
    }

    /// Bytes this file occupies, for the ingest report.
    pub(crate) fn bytes(&self) -> u64 {
        self.written
    }

    /// A merged, ascending stream over `spans`, `window` samples resident per
    /// run.
    ///
    /// Takes the spans as a parameter rather than reading `self.runs`: the
    /// merger borrows the descriptor mutably, and a slice of `self.runs` alive
    /// across that borrow would not compile — nor should it, since a reduce pass
    /// merges a subset. Callers snapshot with [`spans`](RunFile::spans) first.
    pub(crate) fn merge_runs(
        &mut self,
        spans: &[RunSpan],
        window: usize,
    ) -> Result<Merger<'_>, IngestError> {
        self.flush()?;
        let readers: Vec<RunReader> = spans
            .iter()
            .map(|&(off, count)| RunReader {
                next_off: off,
                remaining: count,
                buf: Vec::with_capacity(window),
                pos: 0,
            })
            .collect();
        let mut m = Merger {
            file: &mut self.file,
            readers,
            staging: vec![0u8; window * ENCODED],
            window,
            heap: BinaryHeap::new(),
        };
        for i in 0..m.readers.len() {
            m.seed(i)?;
        }
        Ok(m)
    }
}

/// One run's read cursor: a bounded window into a contiguous span of the file.
struct RunReader {
    next_off: u64,
    remaining: u64,
    buf: Vec<Sample>,
    pos: usize,
}

impl RunReader {
    fn peek(&self) -> Option<Sample> {
        self.buf.get(self.pos).copied()
    }

    /// Refill the window if it is spent. Leaves `pos` at the first unread sample.
    fn refill(
        &mut self,
        file: &mut File,
        staging: &mut [u8],
        window: usize,
    ) -> Result<(), IngestError> {
        if self.pos < self.buf.len() || self.remaining == 0 {
            return Ok(());
        }
        let take = clamp_usize(self.remaining.min(window as u64));
        let bytes = take * ENCODED;
        file.seek(SeekFrom::Start(self.next_off))
            .map_err(|e| io(&e))?;
        let slot = &mut staging[..bytes];
        file.read_exact(slot).map_err(|e| io(&e))?;
        self.buf.clear();
        self.buf.extend(slot.chunks_exact(ENCODED).map(decode));
        self.pos = 0;
        self.next_off += bytes as u64;
        self.remaining -= take as u64;
        Ok(())
    }
}

/// The merged, ascending stream over a set of runs.
///
/// Ties break by run index, which is what preserves "last occurrence in the
/// recording wins" across the cut into runs — see the module docs.
pub(crate) struct Merger<'a> {
    file: &'a mut File,
    readers: Vec<RunReader>,
    staging: Vec<u8>,
    window: usize,
    /// `(stamp, run index)`, min-first. `Reverse` because `BinaryHeap` is a
    /// max-heap and the merge wants the smallest stamp.
    heap: BinaryHeap<core::cmp::Reverse<(i64, usize)>>,
}

impl Merger<'_> {
    fn seed(&mut self, i: usize) -> Result<(), IngestError> {
        self.readers[i].refill(self.file, &mut self.staging, self.window)?;
        if let Some((stamp, _)) = self.readers[i].peek() {
            self.heap.push(core::cmp::Reverse((stamp, i)));
        }
        Ok(())
    }

    /// The next sample in `(stamp, run index)` order, or `None` at the end.
    pub(crate) fn next_sample(&mut self) -> Result<Option<Sample>, IngestError> {
        let Some(core::cmp::Reverse((_, i))) = self.heap.pop() else {
            return Ok(None);
        };
        let item = self.readers[i].peek();
        self.readers[i].pos += 1;
        self.seed(i)?;
        Ok(item)
    }

    /// Bytes this merge holds resident: every run's window plus the shared
    /// decode staging. Reported rather than assumed, because it is the half of
    /// `--max-memory` this module has to keep honest.
    pub(crate) fn resident_bytes(&self) -> u64 {
        let windows = self.readers.len() as u64 * self.window as u64 * ENCODED as u64;
        windows + self.staging.len() as u64
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tf_tree_spill_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample(stamp: i64, k: f64) -> Sample {
        (stamp, [1.0, 0.0, 0.0, 0.0, k, k * 2.0, k * 3.0])
    }

    fn drain(m: &mut Merger<'_>) -> Vec<Sample> {
        let mut out = Vec::new();
        while let Some(s) = m.next_sample().unwrap() {
            out.push(s);
        }
        out
    }

    /// The on-disk width and the in-memory width are the same number, so
    /// `ingest::SAMPLE_BYTES` describes both paths.
    ///
    /// Mutant: change `ENCODED` to `8 + 6 * 8` — applied, and this failed at
    /// `56 != 64`. `a_run_round_trips_exactly` and
    /// `runs_merge_ascending_with_ties_in_run_order` failed with it, inside
    /// `decode`'s fixed-width `copy_from_slice`, which is the length invariant
    /// that function's doc comment relies on.
    #[test]
    fn encoded_width_matches_the_buffered_width() {
        assert_eq!(ENCODED, core::mem::size_of::<Sample>());
    }

    /// A run written and read back yields every field bit for bit, including a
    /// negative stamp and pose components at both ends of `f64`'s range.
    ///
    /// Mutant: encode the pose with `to_be_bytes` while decoding with
    /// `from_le_bytes` — applied, and the comparison failed on the first pose
    /// component.
    #[test]
    fn a_run_round_trips_exactly() {
        let dir = scratch("roundtrip");
        let mut f = RunFile::create(&dir, 0, 128).unwrap();
        let run = vec![
            (
                -9_000_000_007i64,
                [0.5, -0.5, 0.5, -0.5, 1e-17, -3.25, 6.02e23],
            ),
            sample(4, 1.0),
        ];
        f.write_run(&run).unwrap();
        let spans = f.spans();
        let mut m = f.merge_runs(&spans, 4).unwrap();
        assert_eq!(drain(&mut m), run);
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Several runs merge into one ascending stream, and equal stamps come out
    /// in run order — which is recording order, which is what makes §3.2's
    /// "last wins" mean the last occurrence in the file.
    ///
    /// Mutant: build the readers from `spans.iter().rev()`, so the run indices —
    /// and therefore the tie break — run backwards — applied, and the tie
    /// assertion failed with `[2.0, 1.0]` instead of `[1.0, 2.0]`. (The obvious
    /// mutant, `Reverse((stamp, usize::MAX - i))`, is not a valid one: `i` is
    /// also the reader index, so it panics before it can compare anything.)
    #[test]
    fn runs_merge_ascending_with_ties_in_run_order() {
        let dir = scratch("merge");
        let mut f = RunFile::create(&dir, 1, 128).unwrap();
        // Run 0 and run 1 both carry stamp 20, with different payloads.
        f.write_run(&[sample(10, 0.0), sample(20, 1.0), sample(40, 4.0)])
            .unwrap();
        f.write_run(&[sample(20, 2.0), sample(30, 3.0), sample(50, 5.0)])
            .unwrap();
        assert_eq!(f.runs(), 2);
        // A window of one sample forces a refill on nearly every step, which is
        // the path a real cap takes and the one an all-in-memory window hides.
        let spans = f.spans();
        let mut m = f.merge_runs(&spans, 1).unwrap();
        let got = drain(&mut m);
        drop(m);
        let stamps: Vec<i64> = got.iter().map(|s| s.0).collect();
        assert_eq!(stamps, vec![10, 20, 20, 30, 40, 50]);
        let ties: Vec<f64> = got.iter().filter(|s| s.0 == 20).map(|s| s.1[4]).collect();
        assert_eq!(ties, vec![1.0, 2.0], "ties must come out in run order");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty run is not recorded, so the merge never seeds a reader that has
    /// nothing to give.
    ///
    /// Mutant: record the span unconditionally in `end_run` — applied, and the
    /// `runs() == 1` assertion failed at 2. (The merge itself survives an empty
    /// run, because `seed` pushes nothing; the count is what would have lied to
    /// the report and to `fan_in`.)
    #[test]
    fn an_empty_run_is_not_recorded() {
        let dir = scratch("empty");
        let mut f = RunFile::create(&dir, 2, 128).unwrap();
        f.begin_run();
        f.end_run().unwrap();
        f.write_run(&[sample(1, 1.0)]).unwrap();
        assert_eq!(f.runs(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The budget fits the cap** — spill phase and merge phase, over a grid of
    /// caps and run counts up to [`fan_in`].
    ///
    /// This is the assertion `--max-memory` *is*. The reduce pass exists because
    /// a single-pass merge cannot satisfy it: it holds at least one sample per
    /// run, so beyond `fan_in` runs it exceeds the cap however the window is
    /// chosen.
    ///
    /// Mutant: divide by `runs` instead of `runs + 1` in
    /// `merge_window_samples` — applied, and this failed at cap 1024, runs 1:
    /// 1 664 B resident against a 1 024 B cap. That mutant *was* the code for an
    /// hour, and `tests/ingest.rs::a_tiny_cap_reduces_in_several_passes` found
    /// it first, at 1 280 B against the same cap.
    #[test]
    fn budget_fits_the_cap() {
        for user_cap in [1u64, 200, 1024, 4096, 1 << 20, 4 << 30] {
            let cap = cap_of(user_cap);
            let (run, staging) = spill_budget(user_cap);
            assert!(run >= 1 && staging >= ENCODED);
            assert!(
                run as u64 * ENCODED as u64 + staging as u64 <= cap,
                "spill phase over cap {cap}"
            );
            let f = fan_in(user_cap);
            assert!(f >= 2, "a fan-in below two cannot reduce anything");
            for runs in [1usize, 2, f / 2 + 1, f] {
                let w = merge_window_samples(user_cap, runs);
                assert!(w >= 1);
                // `runs + 1` windows — one per run plus the decode staging —
                // and, during a reduce pass, the write staging on top.
                let resident = (runs as u64 + 1) * w as u64 * ENCODED as u64 + staging_of(cap);
                assert!(
                    resident <= cap,
                    "cap {cap}, runs {runs}: {resident} B resident"
                );
            }
        }
    }
}
