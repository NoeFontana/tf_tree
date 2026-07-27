//! The ingest report — `docs/PHASE5.md` §3.2.
//!
//! > *"The ingest report is a first-class output, not log noise: emit it as JSON
//! > alongside the `.tft` and summarize it to the terminal. For many users the
//! > ingest report will be the first thing `tf_tree` ever tells them about their
//! > data, and it should be worth reading."*
//!
//! Two renderings of one structure: [`IngestReport::to_json`] for the file that
//! sits next to the `.tft`, and [`IngestReport::summary`] for the terminal.
//! Neither computes anything the other does not — a summary that quietly
//! rounded, or a JSON that carried a field the summary never showed, is how the
//! two drift into disagreeing about the same recording.
//!
//! # JSON without a JSON dependency
//!
//! Written by hand, the same choice `tf_tree`'s CBOR manifest writer makes and
//! for the same reason: this is one flat document with no user-controlled
//! structure, and the only thing that needs escaping is a frame name. The
//! escaper is [`push_json_string`] and it is tested against the characters that
//! actually appear in a bag — a Windows path in `source`, and a frame name with
//! a quote in it.

use std::fmt::Write as _;
use std::path::Path;

use crate::ingest::{FillStats, Frames, Survey};

/// The JSON document's schema tag. Bumped only for a breaking change, so a
/// consumer can pin it.
pub const REPORT_SCHEMA: &str = "tf_tree.ingest/1";

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
    /// Oldest stamp **in the source** — not the manifest's `oldest_ns`, which
    /// §2.3's amendment defines as the oldest still retained in the ring. §3.1's
    /// counting pass is the only thing that knows this number, which is exactly
    /// why that amendment says the counting pass can supply it.
    pub source_oldest_ns: Option<i64>,
    /// Newest stamp in the source.
    pub source_newest_ns: Option<i64>,
    /// Mean publish rate over the source span, or `None` for fewer than two
    /// samples or a zero-length span.
    pub rate_hz: Option<f64>,
}

impl IngestReport {
    /// Build a report from a completed survey and fill.
    #[must_use]
    pub fn new(path: &Path, survey: &Survey, frames: &Frames, fill: FillStats) -> IngestReport {
        // Canonical order, **by calling the same function `ingest::fill` calls**
        // rather than by repeating its comparator here. A report whose rows moved
        // when the recording's message order moved would be undiffable between
        // two ingests of the same data; a report whose rows were sorted by a
        // second, separately-maintained copy of the rule would agree with the
        // arena until someone edited one of them.
        let order = crate::ingest::canonical_order(survey, frames);
        let edges: Vec<EdgeRow> = order
            .iter()
            .map(|&i| &survey.edges[i])
            .map(|e| {
                let rate = match (e.source_oldest_ns, e.source_newest_ns) {
                    (Some(lo), Some(hi)) if hi > lo && e.samples > 1 => {
                        // `samples - 1` intervals over the span, not `samples`:
                        // ten samples one second apart span nine seconds, and
                        // dividing by ten reports 1.11 Hz for a 1 Hz edge.
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
        // Duplicates are only knowable after the sort, so pass two owns the
        // count and the report is where the two halves meet.
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
             \"passes\":{},\"peak_buffer_bytes\":{}",
            self.frames,
            self.static_edges,
            self.dynamic_edges,
            self.transforms_read,
            self.samples_pushed,
            self.fill.passes,
            self.fill.peak_buffer_bytes,
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
             \"empty_names\":{},\"undecodable_channels\":{}",
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
        );
        let _ = write!(s, ",\"truncated\":{}", a.truncated);
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
                // Non-finite has no JSON spelling, and emitting `NaN` produces a
                // document `json.load` refuses. A rate that is not a number is
                // not a rate.
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
        s.push_str("]}");
        s
    }

    /// The terminal summary.
    ///
    /// Ordered so the first three lines answer "did it work, over what, and how
    /// much", and every anomaly line is **omitted when its count is zero** — a
    /// report that always prints ten zeroes trains the reader to skip it, which
    /// is the opposite of §3.2's requirement that it be worth reading.
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
        let a = &self.anomalies;
        let mut row = |cond: bool, text: String| {
            if cond {
                let _ = writeln!(s, "  ! {text}");
            }
        };
        // First among the anomaly rows, because it changes what every other
        // number in this report means: they describe a prefix of the recording,
        // not the recording.
        row(
            a.truncated,
            "the recording ends mid-record and was read up to that point; \
             every count below covers only the part that exists"
                .to_owned(),
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
        row(
            a.static_conflicts > 0,
            format!(
                "{} /tf_static messages contradicted an already-declared value; the first won",
                a.static_conflicts
            ),
        );
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
            a.filtered_channels > 0,
            format!(
                "{} TF channels were skipped (not CDR, or excluded by --topic)",
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
/// Escapes what RFC 8259 requires and nothing else: quote, backslash, and every
/// control character below 0x20 (as `\u00XX`, since only some of them have short
/// forms). A Windows path in `source` is the backslash case and it is not
/// hypothetical.
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

    /// A rate that is not a number is emitted as `null`, because JSON has no
    /// spelling for `NaN` and a document containing one is refused by every
    /// parser — including `json.load`, which is what a user will reach for.
    ///
    /// The row is built by hand rather than through an ingest because no
    /// recording this crate can read produces a non-finite rate; the guard
    /// exists for the arithmetic, not for a fixture.
    ///
    /// Mutant: drop the `is_finite` guard from the `rate_hz` arm — applied, and
    /// this test failed with `"rate_hz":NaN`.
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
        };
        let json = report.to_json();
        assert!(json.contains("\"rate_hz\":null"), "{json}");
        assert!(!json.contains("NaN"), "{json}");
    }

    /// The characters that actually break a hand-written encoder are escaped.
    ///
    /// Mutant: delete the `'\\' => s.push_str("\\\\")` arm — applied, and the
    /// Windows-path assertion failed (`C:\bags` came out with a lone backslash,
    /// which is an invalid escape rather than a literal one).
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
