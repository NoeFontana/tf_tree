//! §3.1's spill-to-run-file and k-way merge.
//!
//! [`crate::ingest::fill`] groups edges so each group fits `--max-memory`; that
//! fails only when one edge alone exceeds the cap, which this module serves
//! (§3.1's "spill to a temporary run-file with a k-way merge").
//!
//! External merge sort:
//!
//! 1. **Spill.** Read only the oversized edge into a cap-sized buffer; each time
//!    it fills, **stable**-sort it and append it as one *run*.
//! 2. **Reduce.** While runs exceed [`fan_in`], merge [`fan_in`] at a time into a
//!    fresh file and drop (delete) the old one, so disk use is two files. A
//!    single-pass merge holds one sample per run and would exceed the cap by
//!    construction (600 samples at a 1 KiB cap already do).
//! 3. **Merge** what is left into the arena in stamp order.
//!
//! **Ties break by run index**, keeping §3.2's "last occurrence wins": each run is
//! stable-sorted, runs are written and merged in recording order, the heap orders
//! by `(stamp, run index)`, and a reduce merges a *contiguous* window of runs, so
//! the merged stream equals one stable sort of the whole edge. The reduce half is
//! gated by `tests/ingest.rs::a_reduce_pass_keeps_the_last_occurrence`.
//!
//! **The file is unlinked as soon as it exists** (Unix), so `SIGKILL` leaves
//! nothing behind; where the unlink fails, [`Drop`] removes the path.

use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::IngestError;

/// One buffered sample: an `i64` stamp beside the canonical `[f64; 7]` pose.
///
/// Structurally identical to what `ingest::fill` buffers, so
/// `ingest::SAMPLE_BYTES` describes both paths.
pub(crate) type Sample = (i64, [f64; 7]);

/// Encoded width of one [`Sample`] on disk: `i64` + 7 × `f64`, little-endian.
///
/// A constant, not `size_of::<Sample>()`, so the layout does not follow compiler
/// padding (as `FROZEN_HEADER_SIZE`); a test asserts they agree.
pub(crate) const ENCODED: usize = 8 + 7 * 8;

/// The smallest cap this path can honour; a smaller request is raised to it,
/// because the merge and reduce floors would otherwise exceed it. The report
/// prints the *planned* bound, not a measured peak (no counting allocator
/// without `unsafe`); see `ingest::plan_groups`.
///
/// The cap covers every buffer that holds samples, including the stable sort's
/// scratch. It does **not** cover the run index ([`RunFile::index_bytes`], 16
/// bytes per run), which crosses the cap at roughly `cap² / 2048` samples;
/// [`crate::ingest::FillStats::peak_run_index_bytes`] reports it separately.
const MIN_CAP: u64 = 16 * ENCODED as u64;

/// Ceiling on one encode/decode staging buffer.
const MAX_STAGING: u64 = 64 * 1024;

/// The share of the cap the merge sample windows may use; the rest pays for
/// staging (no `unsafe` conversion between the memory and disk views). With
/// `2 × staging ≤ cap / 4` from [`staging_of`], `windows + staging ≤ cap` holds
/// for every cap at or above [`MIN_CAP`] and every run count up to [`fan_in`];
/// `budget_fits_the_cap` checks the grid.
const WINDOW_SHARE_NUM: u64 = 3;
const WINDOW_SHARE_DEN: u64 = 4;

/// The cap actually in force for a requested one.
///
/// Crate-visible so [`crate::ingest::plan_groups`] decides "does this edge fit?"
/// against the number the spill path honours.
pub(crate) fn cap_of(user: u64) -> u64 {
    user.max(MIN_CAP)
}

fn staging_of(cap: u64) -> u64 {
    (cap / 8).clamp(ENCODED as u64, MAX_STAGING)
}

fn clamp_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// `(samples per run, staging bytes)` for the spill phase.
///
/// `2 * ENCODED`: [`sort_run`] is stable and allocates up to one extra copy of
/// the buffer, live beside it and the staging buffer. It costs twice the runs;
/// see `ingest::plan_groups` for the reserve and reported peak.
pub(crate) fn spill_budget(user_cap: u64) -> (usize, usize) {
    let cap = cap_of(user_cap);
    let staging = staging_of(cap);
    let run = ((cap - staging) / (2 * ENCODED as u64)).max(1);
    (clamp_usize(run), clamp_usize(staging))
}

/// How many runs one merge may consume at once.
///
/// Chosen so [`merge_window_samples`] never floors at one sample; above it a
/// reduce pass is required.
pub(crate) fn fan_in(user_cap: u64) -> usize {
    let cap = cap_of(user_cap);
    let windows = cap * WINDOW_SHARE_NUM / WINDOW_SHARE_DEN / ENCODED as u64;
    // `- 1`: the decode staging is one window wide (see `merge_window_samples`).
    clamp_usize(windows.saturating_sub(1).max(2))
}

/// Samples each run's read window may hold when merging `runs` of them.
///
/// Divided by `runs + 1`: the shared decode staging is one window wide and
/// resident with every window during a refill.
pub(crate) fn merge_window_samples(user_cap: u64, runs: usize) -> usize {
    let cap = cap_of(user_cap);
    let divisor = (runs as u64).saturating_add(1);
    clamp_usize((cap * WINDOW_SHARE_NUM / WINDOW_SHARE_DEN / ENCODED as u64 / divisor).max(1))
}

/// Sort one run's samples by stamp, **stably**.
///
/// Two equal stamps must leave this call in arrival order, because the merge's
/// duplicate collapse keeps the last (§3.2); an unstable sort would make one
/// recording ingest to different `.tft` files. One name gives the rule one test,
/// `tests/ingest.rs::the_per_run_sort_is_stable_so_last_wins_inside_a_run`, over
/// runs long enough that stable and unstable sorts differ.
pub(crate) fn sort_run(buf: &mut [Sample]) {
    buf.sort_by_key(|(s, _)| *s);
}

fn io(e: &std::io::Error) -> IngestError {
    IngestError::Spill {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    }
}

/// Encode one sample into exactly [`ENCODED`] little-endian bytes.
///
/// Built on the stack and appended in one `extend_from_slice`: the per-sample
/// path must be cheap.
fn encode(s: Sample) -> [u8; ENCODED] {
    let mut b = [0u8; ENCODED];
    b[..8].copy_from_slice(&s.0.to_le_bytes());
    for (i, v) in s.1.iter().enumerate() {
        b[8 + i * 8..16 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// Decode one sample from exactly [`ENCODED`] bytes — the inverse of [`encode`].
///
/// Every caller feeds it a `chunks_exact(ENCODED)` element, so the fixed-width
/// copies cannot mismatch; a slice, not `&[u8; ENCODED]`, which would need a
/// fallible `try_into`.
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

/// Distinguishes one spill file from every other one this process opens.
///
/// Process-wide, not slot-derived: a slot-derived tag collides across two
/// concurrent [`crate::ingest::fill`]s (or deterministically where the unlink
/// cannot run), and `truncate(true)` would empty the other's inode, giving wrong
/// poses with no error.
static NEXT_TAG: AtomicU64 = AtomicU64::new(0);

/// The name for one spill file: this process, and a tag no other [`RunFile`]
/// in it will take.
///
/// `Relaxed`: only uniqueness is used.
fn spill_path(dir: &Path) -> PathBuf {
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("tf_tree_ingest_spill_{}_{tag}", std::process::id()))
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
/// One file per pass, not per run: the merge seeks anyway, and N files cost N
/// descriptors and unlink races.
pub(crate) struct RunFile {
    file: File,
    /// Kept for its `Drop`, which removes the file where the unlink failed.
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
    /// Opened for reading as well as writing: the merge reads back through this
    /// descriptor. Names come from [`spill_path`].
    pub(crate) fn create(dir: &Path, staging: usize) -> Result<RunFile, IngestError> {
        let path = spill_path(dir);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| io(&e))?;
        // Best-effort; `TempPath` is the fallback.
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
        // `written + staging.len()`: unflushed staged bytes precede this run.
        self.open_run = Some((self.written + self.staging.len() as u64, 0));
    }

    /// Append one sample to the open run.
    pub(crate) fn append(&mut self, s: Sample) -> Result<(), IngestError> {
        if self.staging.len() + ENCODED > self.staging.capacity() {
            self.flush()?;
        }
        self.staging.extend_from_slice(&encode(s));
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

    /// Bytes the run index (`Vec<RunSpan>`) holds resident, by `capacity`. The
    /// one allocation `--max-memory` does not bound (see [`MIN_CAP`]), so it is
    /// reported apart from `peak_buffer_bytes`.
    pub(crate) fn index_bytes(&self) -> u64 {
        self.runs.capacity() as u64 * core::mem::size_of::<RunSpan>() as u64
    }

    /// A merged, ascending stream over `spans`, `window` samples resident per
    /// run.
    ///
    /// Takes spans as a parameter (the merger borrows the descriptor mutably, and
    /// a reduce merges a subset); snapshot with [`spans`](RunFile::spans).
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

    /// The head stamp without copying the pose; [`Merger::seed`] needs only that.
    fn peek_stamp(&self) -> Option<i64> {
        self.buf.get(self.pos).map(|s| s.0)
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
        if let Some(stamp) = self.readers[i].peek_stamp() {
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

    /// Bytes this merge holds resident: every window plus the decode staging.
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

    /// The on-disk width equals the in-memory width.
    ///
    /// Mutant: `ENCODED = 8 + 6 * 8` fails here at `56 != 64`.
    #[test]
    fn encoded_width_matches_the_buffered_width() {
        assert_eq!(ENCODED, core::mem::size_of::<Sample>());
    }

    /// A run round-trips bit for bit, including a negative stamp and extreme poses.
    ///
    /// Mutant: encode the pose with `to_be_bytes` fails on the first component.
    #[test]
    fn a_run_round_trips_exactly() {
        let dir = scratch("roundtrip");
        let mut f = RunFile::create(&dir, 128).unwrap();
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

    /// Runs merge ascending, equal stamps in run order (§3.2's "last wins").
    ///
    /// Mutant: build readers from `spans.iter().rev()` fails with `[2.0, 1.0]`.
    #[test]
    fn runs_merge_ascending_with_ties_in_run_order() {
        let dir = scratch("merge");
        let mut f = RunFile::create(&dir, 128).unwrap();
        f.write_run(&[sample(10, 0.0), sample(20, 1.0), sample(40, 4.0)])
            .unwrap();
        f.write_run(&[sample(20, 2.0), sample(30, 3.0), sample(50, 5.0)])
            .unwrap();
        assert_eq!(f.runs(), 2);
        // A one-sample window forces refills, as a real cap does.
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

    /// An empty run is not recorded.
    ///
    /// Mutant: record the span unconditionally fails `runs() == 1` at 2.
    #[test]
    fn an_empty_run_is_not_recorded() {
        let dir = scratch("empty");
        let mut f = RunFile::create(&dir, 128).unwrap();
        f.begin_run();
        f.end_run().unwrap();
        f.write_run(&[sample(1, 1.0)]).unwrap();
        assert_eq!(f.runs(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two spill files in one process never share a name, across threads too
    /// ([`NEXT_TAG`]).
    ///
    /// Mutant: `fetch_add(0, …)` fails at `1 unique path(s), wanted 256`.
    #[test]
    fn spill_paths_are_unique_within_the_process() {
        use std::collections::BTreeSet;
        const THREADS: usize = 8;
        const EACH: usize = 32;
        let dir = std::env::temp_dir();
        let mut all: BTreeSet<PathBuf> = BTreeSet::new();
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| s.spawn(|| (0..EACH).map(|_| spill_path(&dir)).collect::<Vec<_>>()))
                .collect();
            for h in handles {
                all.extend(h.join().unwrap());
            }
        });
        assert_eq!(
            all.len(),
            THREADS * EACH,
            "{} unique path(s), wanted {}",
            all.len(),
            THREADS * EACH
        );
    }

    /// A run opened while staging holds unflushed bytes starts after them.
    /// Pins an invariant no caller violates today.
    ///
    /// Mutant: `Some((self.written, 0))` fails the span assertion,
    /// `[(0, 1)]` vs `[(64, 1)]`.
    #[test]
    fn a_run_opened_over_unflushed_bytes_starts_after_them() {
        let dir = scratch("unflushed");
        // Staging wide enough that neither sample forces a flush.
        let mut f = RunFile::create(&dir, 8 * ENCODED).unwrap();
        f.begin_run();
        f.append(sample(1, 1.0)).unwrap();
        // Abandons the first run unclosed; the next must not claim its bytes.
        f.begin_run();
        f.append(sample(2, 2.0)).unwrap();
        f.end_run().unwrap();
        let spans = f.spans();
        assert_eq!(spans, vec![(ENCODED as u64, 1)]);
        let mut m = f.merge_runs(&spans, 4).unwrap();
        assert_eq!(drain(&mut m), vec![sample(2, 2.0)]);
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The budget fits the cap, spill and merge phases, over a grid of caps and
    /// run counts up to [`fan_in`]. This assertion *is* `--max-memory`.
    ///
    /// Mutant 1: divide by `runs` in `merge_window_samples` fails at cap 1024,
    /// runs 1 (1 664 B). Mutant 2: `staging_of` returning `cap / 4` fails at
    /// 1 280 B; with `1 × staging` it survives, hiding the second buffer.
    #[test]
    fn budget_fits_the_cap() {
        for user_cap in [1u64, 200, 1024, 4096, 1 << 20, 4 << 30] {
            let cap = cap_of(user_cap);
            let (run, staging) = spill_budget(user_cap);
            assert!(run >= 1 && staging >= ENCODED);
            // Twice the run: `sort_run` allocates (see `spill_budget`).
            assert!(
                2 * run as u64 * ENCODED as u64 + staging as u64 <= cap,
                "spill phase over cap {cap}"
            );
            let f = fan_in(user_cap);
            assert!(f >= 2, "a fan-in below two cannot reduce anything");
            for runs in [1usize, 2, f / 2 + 1, f] {
                let w = merge_window_samples(user_cap, runs);
                assert!(w >= 1);
                // `runs + 1` windows plus two write-staging buffers (a reduce
                // has a read file and a write file open, staging never released).
                let resident = (runs as u64 + 1) * w as u64 * ENCODED as u64 + 2 * staging_of(cap);
                assert!(
                    resident <= cap,
                    "cap {cap}, runs {runs}: {resident} B resident"
                );
            }
        }
    }
}
