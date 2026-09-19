//! The ingest report — `docs/PHASE5.md` §3.2.
//!
//! `to_json` writes the file beside the `.tft`, `summary` the terminal text.

use std::fmt::Write as _;
use std::path::Path;

use crate::ingest::{FillStats, Frames, Survey};

/// The JSON document's schema tag; bumped only for a breaking change.
pub const REPORT_SCHEMA: &str = "tf_tree.ingest/2";

/// What one ingest did, as data.
#[derive(Clone, Debug)]
pub struct IngestReport {
    /// The recording that was read.
    pub source: String,
    /// `tf_tree`'s version.
    pub tool_version: &'static str,
    /// Frames interned.
    pub frames: usize,
    /// Static edges declared.
    pub static_edges: usize,
    /// Dynamic edges declared.
    pub dynamic_edges: usize,
    /// Transforms read from the recording, before any drop.
    pub transforms_read: u64,
    /// Samples pushed into the arena.
    pub samples_pushed: u64,
    /// The recording's overall span, `(oldest, newest)` in nanoseconds.
    pub span_ns: Option<(i64, i64)>,
    /// Pass-two statistics.
    pub fill: FillStats,
    /// Per-edge rows, in survey order.
    pub edges: Vec<EdgeRow>,
    /// Everything §3.2 asks to be counted.
    pub anomalies: crate::Anomalies,
    /// Frame-name remappings applied (§5.6).
    pub remaps: Vec<(String, String)>,
    /// Dynamic edges that ended with no samples.
    pub edges_without_samples: Vec<String>,
    /// §3.2's static-conflict row, with both values, one row per contradicted edge.
    pub static_conflict_details: Vec<StaticConflictRow>,
}

/// One contradicted static edge in the report, with frame names resolved.
#[derive(Clone, Debug)]
pub struct StaticConflictRow {
    /// Parent frame name.
    pub parent: String,
    /// Child frame name.
    pub child: String,
    /// Which topic declared [`existing`](StaticConflictRow::existing).
    pub declared_by: String,
    /// Which topic offered [`offered`](StaticConflictRow::offered).
    pub contradicted_by: String,
    /// The value on file, which wins. Canonical order — `[qw qx qy qz tx ty tz]`.
    pub existing: [f64; 7],
    /// The value that was refused, in the same order.
    pub offered: [f64; 7],
}

/// One edge's row in the report.
#[derive(Clone, Debug)]
pub struct EdgeRow {
    /// Parent frame name.
    pub parent: String,
    /// Child frame name.
    pub child: String,
    /// The topic it was seen on.
    pub topic: String,
    /// Whether it is static.
    pub is_static: bool,
    /// Samples the source contained, after pass one's drops.
    pub samples: u64,
    /// Oldest stamp in the source, not the manifest's ring-retained `oldest_ns`.
    pub source_oldest_ns: Option<i64>,
    /// Newest stamp in the source.
    pub source_newest_ns: Option<i64>,
    /// Mean publish rate over the source span; `None` below two samples.
    pub rate_hz: Option<f64>,
}

impl IngestReport {
    /// Build a report from a completed survey and fill.
    #[must_use]
    pub fn new(path: &Path, survey: &Survey, frames: &Frames, fill: FillStats) -> IngestReport {
        let order = crate::ingest::canonical_order(survey, frames);
        let edges: Vec<EdgeRow> = order
            .iter()
            .map(|&i| &survey.edges[i])
            .map(|e| {
                let rate = match (e.source_oldest_ns, e.source_newest_ns) {
                    (Some(lo), Some(hi)) if hi > lo && e.samples > 1 => {
                        let secs = (hi - lo) as f64 / 1e9;
                        Some((e.samples - 1) as f64 / secs)
                    }
                    _ => None,
                };
                EdgeRow {
                    parent: frames.name(e.parent).to_owned(),
                    child: frames.name(e.child).to_owned(),
                    topic: e.topic.clone(),
                    is_static: e.is_static(),
                    samples: e.samples,
                    source_oldest_ns: e.source_oldest_ns,
                    source_newest_ns: e.source_newest_ns,
                    rate_hz: rate,
                }
            })
            .collect();
        let mut anomalies = survey.anomalies.clone();
        // Duplicates are only knowable after the sort.
        anomalies.duplicate_stamps = fill.duplicates;
        IngestReport {
            source: path.display().to_string(),
            tool_version: env!("CARGO_PKG_VERSION"),
            frames: frames.len(),
            static_edges: edges.iter().filter(|e| e.is_static).count(),
            dynamic_edges: edges.iter().filter(|e| !e.is_static).count(),
            transforms_read: survey.transforms_read,
            samples_pushed: fill.pushed,
            span_ns: survey.span_ns(),
            fill,
            edges_without_samples: survey
                .edges_without_samples()
                .into_iter()
                .map(|i| {
                    format!(
                        "{} -> {}",
                        frames.name(survey.edges[i].parent),
                        frames.name(survey.edges[i].child)
                    )
                })
                .collect(),
            edges,
            anomalies,
            remaps: survey.remaps.clone(),
            static_conflict_details: survey
                .static_conflict_details
                .iter()
                .map(|c| StaticConflictRow {
                    parent: frames.name(c.parent).to_owned(),
                    child: frames.name(c.child).to_owned(),
                    declared_by: c.declared_by.clone(),
                    contradicted_by: c.contradicted_by.clone(),
                    existing: c.existing,
                    offered: c.offered,
                })
                .collect(),
        }
    }

    /// The JSON document that sits next to the `.tft`.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(1024 + self.edges.len() * 192);
        s.push('{');
        push_kv_str(&mut s, "schema", REPORT_SCHEMA);
        s.push(',');
        push_kv_str(&mut s, "tf_tree", self.tool_version);
        s.push(',');
        push_kv_str(&mut s, "source", &self.source);
        let _ = write!(
            s,
            ",\"frames\":{},\"static_edges\":{},\"dynamic_edges\":{},\
             \"transforms_read\":{},\"samples_pushed\":{},\
             \"passes\":{},\"peak_buffer_bytes\":{},\
             \"peak_run_index_bytes\":{},\
             \"spilled_runs\":{},\"spilled_bytes\":{}",
            self.frames,
            self.static_edges,
            self.dynamic_edges,
            self.transforms_read,
            self.samples_pushed,
            self.fill.passes,
            self.fill.peak_buffer_bytes,
            self.fill.peak_run_index_bytes,
            self.fill.spilled_runs,
            self.fill.spilled_bytes,
        );
        s.push_str(",\"span_ns\":");
        match self.span_ns {
            Some((lo, hi)) => {
                let _ = write!(s, "[{lo},{hi}]");
            }
            None => s.push_str("null"),
        }

        s.push_str(",\"anomalies\":{");
        let a = &self.anomalies;
        let _ = write!(
            s,
            "\"zero_stamp_drops\":{},\"future_stamps\":{},\
             \"worst_future_offset_ns\":{},\"out_of_order\":{},\
             \"clock_resets\":{},\"static_conflicts\":{},\
             \"duplicate_stamps\":{},\"stripped_slash_names\":{},\
             \"empty_names\":{},\"filtered_channels\":{},\"non_cdr_channels\":{}",
            a.zero_stamp_drops,
            a.future_stamps,
            a.worst_future_offset_ns,
            a.out_of_order,
            a.clock_resets,
            a.static_conflicts,
            a.duplicate_stamps,
            a.stripped_slash_names,
            a.empty_names,
            a.filtered_channels,
            a.non_cdr_channels,
        );
        let _ = write!(s, ",\"truncated\":{}", a.truncated);
        let _ = write!(s, ",\"bad_chunks\":{}", a.bad_chunks);
        let _ = write!(s, ",\"chunks_over_limit\":{}", a.chunks_over_limit);
        let _ = write!(
            s,
            ",\"oversized_records_skipped\":{}",
            a.oversized_records_skipped
        );
        s.push_str(",\"bad_chunk_span_ns\":");
        match a.bad_chunk_span_ns {
            Some((lo, hi)) => {
                let _ = write!(s, "[{lo},{hi}]");
            }
            None => s.push_str("null"),
        }
        s.push_str(",\"first_reset_at_ns\":");
        match a.first_reset_at_ns {
            Some(v) => {
                let _ = write!(s, "{v}");
            }
            None => s.push_str("null"),
        }
        s.push('}');

        s.push_str(",\"edges\":[");
        for (i, e) in self.edges.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('{');
            push_kv_str(&mut s, "parent", &e.parent);
            s.push(',');
            push_kv_str(&mut s, "child", &e.child);
            s.push(',');
            push_kv_str(&mut s, "topic", &e.topic);
            let _ = write!(s, ",\"static\":{},\"samples\":{}", e.is_static, e.samples);
            s.push_str(",\"source_oldest_ns\":");
            push_opt_i64(&mut s, e.source_oldest_ns);
            s.push_str(",\"source_newest_ns\":");
            push_opt_i64(&mut s, e.source_newest_ns);
            s.push_str(",\"rate_hz\":");
            match e.rate_hz {
                // Non-finite has no JSON spelling.
                Some(r) if r.is_finite() => {
                    let _ = write!(s, "{r:.6}");
                }
                _ => s.push_str("null"),
            }
            s.push('}');
        }
        s.push(']');

        s.push_str(",\"remaps\":[");
        for (i, (from, to)) in self.remaps.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('[');
            push_json_string(&mut s, from);
            s.push(',');
            push_json_string(&mut s, to);
            s.push(']');
        }
        s.push(']');

        s.push_str(",\"edges_without_samples\":[");
        for (i, e) in self.edges_without_samples.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            push_json_string(&mut s, e);
        }
        s.push(']');

        s.push_str(",\"static_conflict_details\":[");
        for (i, c) in self.static_conflict_details.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('{');
            push_kv_str(&mut s, "parent", &c.parent);
            s.push(',');
            push_kv_str(&mut s, "child", &c.child);
            s.push(',');
            push_kv_str(&mut s, "declared_by", &c.declared_by);
            s.push(',');
            push_kv_str(&mut s, "contradicted_by", &c.contradicted_by);
            s.push_str(",\"existing\":");
            push_pose(&mut s, &c.existing);
            s.push_str(",\"offered\":");
            push_pose(&mut s, &c.offered);
            s.push('}');
        }
        s.push_str("]}");
        s
    }

    /// The terminal summary; every anomaly line is omitted when its count is zero.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "ingested {}", self.source);
        let _ = writeln!(
            s,
            "  {} frames, {} static edges, {} dynamic edges",
            self.frames, self.static_edges, self.dynamic_edges
        );
        match self.span_ns {
            Some((lo, hi)) => {
                let _ = writeln!(
                    s,
                    "  {} transforms read, {} samples stored, {:.3} s span",
                    self.transforms_read,
                    self.samples_pushed,
                    (hi - lo) as f64 / 1e9
                );
            }
            None => {
                let _ = writeln!(
                    s,
                    "  {} transforms read, {} samples stored, no dynamic span",
                    self.transforms_read, self.samples_pushed
                );
            }
        }
        if self.fill.passes > 1 {
            let _ = writeln!(
                s,
                "  re-read the recording {} times to stay under --max-memory (peak {} B)",
                self.fill.passes, self.fill.peak_buffer_bytes
            );
        }
        if self.fill.spilled_runs > 0 {
            let _ = writeln!(
                s,
                "  spilled {} sorted run{} ({} B) to a temporary file, \
                 {} B of run index; one edge exceeds --max-memory on its own",
                self.fill.spilled_runs,
                if self.fill.spilled_runs == 1 { "" } else { "s" },
                self.fill.spilled_bytes,
                self.fill.peak_run_index_bytes
            );
        }
        let a = &self.anomalies;
        let mut row = |cond: bool, text: String| {
            if cond {
                let _ = writeln!(s, "  ! {text}");
            }
        };
        row(
            a.truncated,
            "the recording ends mid-record and was read up to that point; \
             every count below covers only the part that exists"
                .to_owned(),
        );
        row(
            a.bad_chunks > 0,
            match a.bad_chunk_span_ns {
                Some((lo, hi)) => format!(
                    "{} chunk(s) were unreadable and were skipped, losing the \
                     transforms between {lo} and {hi} ns; every count below covers \
                     only the part that could be read. Use --on-bad-chunk=halt to \
                     refuse a recording that is not whole",
                    a.bad_chunks
                ),
                None => format!(
                    "{} chunk(s) were unreadable and were skipped, and their \
                     headers named no message times, so what was lost cannot be \
                     placed in time; every count below covers only the part that \
                     could be read",
                    a.bad_chunks
                ),
            },
        );
        row(
            a.chunks_over_limit > 0,
            format!(
                "{} of those were not damaged — they exceeded this reader's limits \
                 (--max-chunk-size, --max-chunk-expansion, or a zstd frame asking \
                 for more decoding window than a chunk that size is allowed). \
                 Raising the flag reads them",
                a.chunks_over_limit
            ),
        );
        // Names only what is known: this run did not look, and which flag makes it.
        row(
            a.oversized_records_skipped > 0,
            format!(
                "{} record(s) over --max-record-size were of a kind this reader \
                 does not read, and were stepped over on the length they declare; \
                 the bytes inside that span were not read, so raise \
                 --max-record-size to have them parsed rather than skipped",
                a.oversized_records_skipped
            ),
        );
        row(
            a.zero_stamp_drops > 0,
            format!(
                "{} transforms had stamp 0 and were dropped \
                 (a publisher is not setting header.stamp)",
                a.zero_stamp_drops
            ),
        );
        row(
            a.future_stamps > 0,
            format!(
                "{} transforms are stamped up to {:.3} s ahead of when they were recorded; kept",
                a.future_stamps,
                a.worst_future_offset_ns as f64 / 1e9
            ),
        );
        row(
            a.duplicate_stamps > 0,
            format!(
                "{} duplicate (edge, stamp) pairs; the last one in the recording won",
                a.duplicate_stamps
            ),
        );
        row(
            a.out_of_order > 0,
            format!(
                "{} transforms arrived out of stamp order; sorted per edge before storing",
                a.out_of_order
            ),
        );
        row(
            a.clock_resets > 0,
            format!(
                "{} backward clock jumps past the reset threshold",
                a.clock_resets
            ),
        );
        row(a.static_conflicts > 0, {
            // Full precision: `StaticStore` treats poses within 1e-12 as equal.
            let mut t = format!(
                "{} /tf_static messages contradicted an already-declared value; the first won",
                a.static_conflicts
            );
            for c in &self.static_conflict_details {
                let _ = write!(
                    t,
                    "\n      {} -> {}: {} declared {}, {} offered {}",
                    c.parent,
                    c.child,
                    c.declared_by,
                    pose_text(&c.existing),
                    c.contradicted_by,
                    pose_text(&c.offered),
                );
            }
            t
        });
        row(
            a.stripped_slash_names > 0,
            format!(
                "{} frame names arrived with a leading '/'",
                a.stripped_slash_names
            ),
        );
        row(
            a.empty_names > 0,
            format!(
                "{} transforms had an empty frame name and were dropped",
                a.empty_names
            ),
        );
        row(
            a.filtered_channels + a.non_cdr_channels > 0,
            format!(
                "{} TF channels were skipped ({} not CDR, {} excluded by --topic)",
                a.filtered_channels + a.non_cdr_channels,
                a.non_cdr_channels,
                a.filtered_channels
            ),
        );
        row(
            !self.edges_without_samples.is_empty(),
            format!(
                "{} dynamic edges are in the tree with no samples: {}",
                self.edges_without_samples.len(),
                self.edges_without_samples.join(", ")
            ),
        );
        s
    }
}

/// A canonical `[qw qx qy qz tx ty tz]` pose as a JSON array.
///
/// Non-finite components become `null`.
fn push_pose(s: &mut String, p: &[f64; 7]) {
    s.push('[');
    for (i, v) in p.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        if v.is_finite() {
            let _ = write!(s, "{v}");
        } else {
            s.push_str("null");
        }
    }
    s.push(']');
}

/// The same pose for the terminal, at full `f64` round-trip precision.
fn pose_text(p: &[f64; 7]) -> String {
    let mut s = String::new();
    push_pose(&mut s, p);
    s
}

fn push_opt_i64(s: &mut String, v: Option<i64>) {
    match v {
        Some(v) => {
            let _ = write!(s, "{v}");
        }
        None => s.push_str("null"),
    }
}

fn push_kv_str(s: &mut String, key: &str, value: &str) {
    push_json_string(s, key);
    s.push(':');
    push_json_string(s, value);
}

/// Write `v` as a JSON string literal.
///
/// Escapes what RFC 8259 requires: quote, backslash, and control characters
/// below 0x20 (as `\u00XX`).
fn push_json_string(s: &mut String, v: &str) {
    s.push('"');
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// A non-finite rate is emitted as `null`.
    #[test]
    fn non_finite_rate_is_null() {
        let row = EdgeRow {
            parent: "a".into(),
            child: "b".into(),
            topic: "/tf".into(),
            is_static: false,
            samples: 2,
            source_oldest_ns: Some(0),
            source_newest_ns: Some(0),
            rate_hz: Some(f64::NAN),
        };
        let report = IngestReport {
            source: "x.mcap".into(),
            tool_version: "0",
            frames: 2,
            static_edges: 0,
            dynamic_edges: 1,
            transforms_read: 2,
            samples_pushed: 2,
            span_ns: None,
            fill: FillStats::default(),
            edges: vec![row],
            anomalies: crate::Anomalies::default(),
            remaps: Vec::new(),
            edges_without_samples: Vec::new(),
            static_conflict_details: Vec::new(),
        };
        let json = report.to_json();
        assert!(json.contains("\"rate_hz\":null"), "{json}");
        assert!(!json.contains("NaN"), "{json}");
    }

    /// A ceiling refusal is reported naming the flag, not as an unreadable chunk.
    #[test]
    fn a_ceiling_refusal_is_reported_apart_from_damage() {
        let with_limit = |over: u64| {
            IngestReport {
                source: "x.mcap".into(),
                tool_version: "0",
                frames: 0,
                static_edges: 0,
                dynamic_edges: 0,
                transforms_read: 0,
                samples_pushed: 0,
                span_ns: None,
                fill: FillStats::default(),
                edges: Vec::new(),
                anomalies: crate::Anomalies {
                    bad_chunks: 4,
                    chunks_over_limit: over,
                    ..crate::Anomalies::default()
                },
                remaps: Vec::new(),
                edges_without_samples: Vec::new(),
                static_conflict_details: Vec::new(),
            }
            .summary()
        };

        let text = with_limit(3);
        assert!(
            text.contains("--max-chunk-size") && text.contains("--max-chunk-expansion"),
            "the flags that would read those chunks must be named: {text}"
        );
        assert!(
            text.contains("not damaged"),
            "and the report must say the chunks were sound: {text}"
        );

        assert!(
            !with_limit(0).contains("--max-chunk-size"),
            "{}",
            with_limit(0)
        );
    }

    /// Quotes, backslashes (Windows paths) and control characters are escaped.
    #[test]
    fn strings_are_escaped() {
        let mut s = String::new();
        push_json_string(&mut s, r"C:\bags\run.mcap");
        assert_eq!(s, r#""C:\\bags\\run.mcap""#);

        let mut s = String::new();
        push_json_string(&mut s, "he said \"base_link\"\u{1}");
        assert_eq!(s, r#""he said \"base_link\"\u0001""#);
    }
}
