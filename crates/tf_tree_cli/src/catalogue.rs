//! The diagnostics catalogue (`docs/PHASE5.md` §6): identifiers, severities and
//! the two renderers. Detection lives in [`crate::checks`].
//!
//! Identifiers are a wire contract: an id never changes meaning and is never
//! recycled. The catalogue is `TFT001`-`TFT019`; `docs/PHASE5.md` §0.0 owns which
//! ids detect what.
//!
//! | Phase 1 check | Catalogue id |
//! |---|---|
//! | `multi-writer` | `TFT001` |
//! | `inconsistent-rate` | `TFT008` |
//! | `short-buffer` | `TFT011` |
//! | `cycle`, `unreachable` | `TFT012` |
//! | `unclaimed-dynamic` | `TFT017` |
//! | `out-of-order` | `TFT018` |
//!
//! `TFT019` maps onto no Phase 1 check. [`Uncatalogued`] has no producer today;
//! its `--json` key is stable. A check that cannot run is [`Status::Skipped`]
//! with a reason, never silently passed. Severity belongs to the check, not the
//! finding.

use core::fmt::Write as _;

/// How serious a finding is. Ordered: [`Severity::Info`] < [`Severity::Warn`] <
/// [`Severity::Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth knowing, never worth failing a build over.
    Info,
    /// Worth attention but not necessarily broken.
    Warn,
    /// A genuine fault. These, and only these, drive `--exit-code`.
    Error,
}

impl Severity {
    /// The fixed-width label used in the human output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "INFO ",
            Severity::Warn => "WARN ",
            Severity::Error => "ERROR",
        }
    }

    /// The lowercase token used in `--json`. **Stable.**
    #[must_use]
    pub fn json(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

/// The catalogue, declared once: every accessor is generated from one row.
macro_rules! catalogue {
    ($( $(#[$m:meta])* $variant:ident => { id: $id:literal, title: $title:literal, severity: $sev:ident } ),+ $(,)?) => {
        /// A stable diagnostic identifier (`docs/PHASE5.md` §6).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub enum Tft {
            $( $(#[$m])* $variant, )+
        }

        impl Tft {
            /// Every check, in id order. [`crate::checks::run`] walks this.
            pub const ALL: [Tft; [$(Tft::$variant),+].len()] = [$(Tft::$variant),+];

            /// The stable identifier, e.g. `"TFT010"`. **Never changes.**
            #[must_use]
            pub fn id(self) -> &'static str {
                match self { $( Tft::$variant => $id, )+ }
            }

            /// A one-line description, for the human report's header.
            #[must_use]
            pub fn title(self) -> &'static str {
                match self { $( Tft::$variant => $title, )+ }
            }

            /// This check's severity — fixed per id, not per finding.
            #[must_use]
            pub fn severity(self) -> Severity {
                match self { $( Tft::$variant => Severity::$sev, )+ }
            }
        }
    };
}

catalogue! {
    /// Multi-publisher conflict on an edge.
    Tft001 => { id: "TFT001", title: "multi-publisher conflict on an edge", severity: Error },
    /// Static transform republished with a different value.
    Tft002 => { id: "TFT002", title: "static transform republished with a different value", severity: Error },
    /// Edge kind changed (static <-> dynamic).
    Tft003 => { id: "TFT003", title: "edge kind changed (static <-> dynamic)", severity: Error },
    /// Clock skew between publishers.
    Tft004 => { id: "TFT004", title: "clock skew between publishers", severity: Warn },
    /// Stamps in the future.
    Tft005 => { id: "TFT005", title: "stamps in the future", severity: Warn },
    /// Zero or absurd stamps.
    Tft006 => { id: "TFT006", title: "zero or absurd stamps", severity: Error },
    /// Publish rate deviates from the declared nominal rate.
    Tft007 => { id: "TFT007", title: "publish rate deviates from nominal", severity: Warn },
    /// Jitter: the inter-arrival distribution is far from its own centre.
    Tft008 => { id: "TFT008", title: "jitter: inter-arrival spread", severity: Warn },
    /// Gaps / dropouts.
    Tft009 => { id: "TFT009", title: "gaps / dropouts", severity: Warn },
    /// Extrapolation hotspot.
    Tft010 => { id: "TFT010", title: "extrapolation hotspot", severity: Warn },
    /// Ring capacity too small for the observed consumer lag.
    Tft011 => { id: "TFT011", title: "ring capacity too small for observed consumer lag", severity: Warn },
    /// Disconnected subtree.
    Tft012 => { id: "TFT012", title: "disconnected subtree", severity: Error },
    /// Frame declared but never published.
    Tft013 => { id: "TFT013", title: "frame declared but never published", severity: Info },
    /// Participant or claim slot leak.
    Tft014 => { id: "TFT014", title: "participant or claim slot leak", severity: Warn },
    /// Arena occupancy above 80%.
    Tft015 => { id: "TFT015", title: "arena occupancy above 80%", severity: Warn },
    /// Transparent huge pages disabled, or `RLIMIT_MEMLOCK` below the arena size.
    Tft016 => { id: "TFT016", title: "transparent huge pages off, or RLIMIT_MEMLOCK below the arena size", severity: Info },
    /// A dynamic edge with no live writer holding its claim.
    Tft017 => { id: "TFT017", title: "dynamic edge with no live writer", severity: Warn },
    /// Stamps arriving out of monotonic order on an edge.
    Tft018 => { id: "TFT018", title: "stamps arriving out of order", severity: Error },
    /// A wall-clock domain stepped backwards — [`Tft::Tft019`]'s cause, not a
    /// publisher fault.
    Tft019 => { id: "TFT019", title: "a wall-clock domain stepped backwards", severity: Warn },
}

impl Tft {
    /// Parse an identifier for `--suppress`. Case-insensitive; near-misses like
    /// `"TFT10"` and `"10"` are rejected.
    #[must_use]
    pub fn parse(s: &str) -> Option<Tft> {
        let up = s.trim().to_ascii_uppercase();
        Tft::ALL.into_iter().find(|c| c.id() == up)
    }
}

/// One diagnostic finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Which check raised it.
    pub check: Tft,
    /// The edge this finding is about, if any.
    pub edge: Option<u32>,
    /// A short label for the subject (`"map->odom (edge#3)"`, `"arena"`).
    pub subject: String,
    /// A human-readable explanation.
    pub message: String,
}

impl Finding {
    /// A finding about something that is not a single edge.
    #[must_use]
    pub fn about(check: Tft, subject: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            check,
            edge: None,
            subject: subject.into(),
            message: message.into(),
        }
    }

    /// A finding about one edge.
    #[must_use]
    pub fn on_edge(
        check: Tft,
        edge: u32,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Finding {
        Finding {
            check,
            edge: Some(edge),
            subject: subject.into(),
            message: message.into(),
        }
    }

    /// This finding's severity, which is its check's.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.check.severity()
    }
}

/// A Phase 1 finding `docs/PHASE5.md` §6 gives no identifier: `--suppress` cannot
/// name it, but it still gates `--exit-code`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uncatalogued {
    /// The Phase 1 check's label, e.g. `"out-of-order"`.
    pub check: &'static str,
    /// How serious it is.
    pub severity: Severity,
    /// What the finding is about.
    pub subject: String,
    /// The explanation.
    pub message: String,
}

/// What happened when a check ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// It ran and found nothing.
    Pass,
    /// It ran and found something; see [`CheckOutcome::findings`].
    Fired,
    /// It could not run. The reason is mandatory.
    Skipped(String),
}

/// One catalogue entry's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Which check.
    pub check: Tft,
    /// Pass / fired / skipped.
    pub status: Status,
    /// Findings, empty unless `status` is [`Status::Fired`].
    pub findings: Vec<Finding>,
    /// Whether `--suppress` named this id.
    ///
    /// A suppressed check still runs and reports; it leaves the `--exit-code` gate only.
    pub suppressed: bool,
}

impl CheckOutcome {
    /// A check that ran; `Fired` iff it produced findings.
    #[must_use]
    pub fn ran(check: Tft, findings: Vec<Finding>) -> CheckOutcome {
        CheckOutcome {
            check,
            status: if findings.is_empty() {
                Status::Pass
            } else {
                Status::Fired
            },
            findings,
            suppressed: false,
        }
    }

    /// A check that could not run, with the mandatory reason.
    #[must_use]
    pub fn skipped(check: Tft, why: impl Into<String>) -> CheckOutcome {
        CheckOutcome {
            check,
            status: Status::Skipped(why.into()),
            findings: Vec::new(),
            suppressed: false,
        }
    }
}

/// The result of running the whole catalogue.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// One outcome per [`Tft::ALL`] entry, in id order.
    pub outcomes: Vec<CheckOutcome>,
    /// Findings from the Phase 1 checks §6 gives no id — see [`Uncatalogued`].
    pub uncatalogued: Vec<Uncatalogued>,
}

impl Report {
    /// Catalogue findings at `sev` that were not suppressed.
    pub fn at(&self, sev: Severity) -> impl Iterator<Item = &Finding> {
        self.outcomes
            .iter()
            .filter(move |o| !o.suppressed && o.check.severity() == sev)
            .flat_map(|o| o.findings.iter())
    }

    /// Every finding at `sev`, catalogued or not, counting suppression.
    fn count_at(&self, sev: Severity) -> usize {
        self.at(sev).count()
            + self
                .uncatalogued
                .iter()
                .filter(|u| u.severity == sev)
                .count()
    }

    /// Whether any unsuppressed error-severity check fired (`--exit-code`).
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.count_at(Severity::Error) > 0
    }

    /// Whether no unsuppressed warnings or errors exist; info does not count.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.count_at(Severity::Error) == 0 && self.count_at(Severity::Warn) == 0
    }

    /// One check's outcome, so a [`Meta::notes`] disclosure cannot contradict it.
    #[must_use]
    pub fn outcome(&self, id: Tft) -> Option<&CheckOutcome> {
        self.outcomes.iter().find(|o| o.check == id)
    }

    /// `(passed, fired, skipped, suppressed)` counts over the catalogue.
    #[must_use]
    pub fn tally(&self) -> (usize, usize, usize, usize) {
        let mut t = (0, 0, 0, 0);
        for o in &self.outcomes {
            match o.status {
                Status::Pass => t.0 += 1,
                Status::Fired => t.1 += 1,
                Status::Skipped(_) => t.2 += 1,
            }
            if o.suppressed {
                t.3 += 1;
            }
        }
        t
    }
}

/// What the report is *about* — the parts of the header that are not findings.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    /// `"in-process fixture"` or `"live arena"`.
    pub source: &'static str,
    /// The arena's format version.
    pub format_version: u32,
    /// The arena's layout hash.
    pub layout_hash: u32,
    /// Instance uuid, hex, when the source is a shared arena.
    pub instance: Option<String>,
    /// The rendezvous runtime directory (`docs/PHASE2.md` §15); `None` if
    /// resolution fails.
    pub runtime_dir: Option<String>,
    /// Frame count, for the one-line summary.
    pub frames: usize,
    /// Edge count.
    pub edges: usize,
    /// Wall-clock time of the report, nanoseconds since the epoch.
    pub generated_unix_nanos: i64,
    /// The reference clock the time-based checks used, and where it came from.
    pub now_nanos: i64,
    /// How `now_nanos` was obtained, for the header line.
    pub clock_source: &'static str,
    /// Whether the **engine** compiled `docs/PHASE5.md` §5's counters in.
    pub counters_compiled_in: bool,
    /// Disclosures for a check that ran with an evidence source missing.
    pub notes: Vec<String>,
    /// What the declared ring capacities reserve (display only; `0021`).
    pub rings: crate::sizing::Rings,
}

/// Render the human-readable report. Skipped checks are always printed.
#[must_use]
pub fn render_human(report: &Report, meta: &Meta) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "tf_tree doctor ({})", meta.source);
    if let Some(uuid) = &meta.instance {
        let _ = writeln!(s, "  instance {uuid}");
    }
    if let Some(dir) = &meta.runtime_dir {
        let _ = writeln!(s, "  runtime dir {dir}");
    }
    let _ = writeln!(
        s,
        "  arena format_version {} layout_hash 0x{:08X}  {} frames, {} edges",
        meta.format_version, meta.layout_hash, meta.frames, meta.edges
    );
    let _ = writeln!(s, "  {}", meta.rings.line());
    let _ = writeln!(s, "  {}", crate::sizing::FORMULA);
    let _ = writeln!(
        s,
        "  reference clock {} ns ({})",
        meta.now_nanos, meta.clock_source
    );
    if !meta.counters_compiled_in {
        let _ = writeln!(
            s,
            "  engine built without the `counters` feature: TFT010/TFT011 have no data (PHASE5 §5.5)"
        );
    }
    for n in &meta.notes {
        let _ = writeln!(s, "  note: {n}");
    }
    let _ = writeln!(s);

    let mut any = false;
    for sev in [Severity::Error, Severity::Warn, Severity::Info] {
        for o in &report.outcomes {
            if o.check.severity() != sev || o.findings.is_empty() {
                continue;
            }
            any = true;
            let tag = if o.suppressed { " (suppressed)" } else { "" };
            let _ = writeln!(
                s,
                "  [{}] {}  {}{tag}",
                sev.label(),
                o.check.id(),
                o.check.title()
            );
            for f in &o.findings {
                let _ = writeln!(s, "          {}: {}", f.subject, f.message);
            }
        }
        for u in report.uncatalogued.iter().filter(|u| u.severity == sev) {
            any = true;
            let _ = writeln!(s, "  [{}] (no id)  {}", sev.label(), u.check);
            let _ = writeln!(s, "          {}: {}", u.subject, u.message);
        }
    }
    if !any {
        let _ = writeln!(s, "  no findings");
    }
    let _ = writeln!(s);

    let (pass, fired, skipped, suppressed) = report.tally();
    let _ = writeln!(
        s,
        "  {} catalogue checks: {pass} passed, {fired} fired, {skipped} not run, {suppressed} suppressed",
        report.outcomes.len()
    );
    if skipped > 0 {
        let _ = writeln!(s, "  not run:");
        for o in &report.outcomes {
            if let Status::Skipped(why) = &o.status {
                let _ = writeln!(s, "    {}  {} — {why}", o.check.id(), o.check.title());
            }
        }
    }
    s
}

/// The `--json` schema identifier; bump only for an incompatible change.
pub const JSON_SCHEMA: &str = "tf_tree.doctor/1";

/// Render the machine-readable report (hand-written; no `serde_json`).
///
/// # Schema — stable
///
/// ```text
/// {
///   "schema": "tf_tree.doctor/1",
///   "tool_version": string,
///   "generated_unix_nanos": i64,
///   "now_nanos": i64,                  // the clock the time checks used
///   "clock_source": string,
///   "source": "live arena" | "in-process fixture",
///   "counters_compiled_in": bool,
///   "runtime_dir": string|null,       // resolved rendezvous dir, or null
///   "notes": [ string ],              // checks that ran with evidence missing
///   "arena": { "format_version": u32, "layout_hash": "0x........",
///              "instance": string|null, "frames": u32, "edges": u32,
///              // `rounding_slack_*_max` is an upper bound
///              "rings": { "edges": u32, "declared_slots": u64, "declared_bytes": u64,
///                         "used_slots": u64, "used_bytes": u64,
///                         "rounding_slack_slots_max": u64,
///                         "rounding_slack_bytes_max": u64, "bytes_per_slot": u64 } },
///   "summary": { "error": u32, "warn": u32, "info": u32,
///                "passed": u32, "fired": u32, "not_run": u32, "suppressed": u32 },
///   "checks": [ { "id": "TFT001", "title": string, "severity": "error",
///                 "status": "pass"|"fired"|"skipped", "suppressed": bool,
///                 "reason": string|null,
///                 "findings": [ { "edge": u32|null, "subject": string,
///                                 "message": string } ] } ],
///   "uncatalogued": [ { "check": string, "severity": string,
///                       "subject": string, "message": string } ]
/// }
/// ```
///
/// `checks` carries every catalogue id; `summary` counts `uncatalogued` too.
#[must_use]
pub fn render_json(report: &Report, meta: &Meta) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{{");
    let _ = writeln!(s, "  \"schema\": \"{JSON_SCHEMA}\",");
    let _ = writeln!(
        s,
        "  \"tool_version\": \"{}\",",
        json_escape(env!("CARGO_PKG_VERSION"))
    );
    let _ = writeln!(
        s,
        "  \"generated_unix_nanos\": {},",
        meta.generated_unix_nanos
    );
    let _ = writeln!(s, "  \"now_nanos\": {},", meta.now_nanos);
    let _ = writeln!(
        s,
        "  \"clock_source\": \"{}\",",
        json_escape(meta.clock_source)
    );
    let _ = writeln!(s, "  \"source\": \"{}\",", json_escape(meta.source));
    let _ = writeln!(
        s,
        "  \"counters_compiled_in\": {},",
        meta.counters_compiled_in
    );
    let _ = writeln!(s, "  \"notes\": [");
    for (i, n) in meta.notes.iter().enumerate() {
        let comma = if i + 1 == meta.notes.len() { "" } else { "," };
        let _ = writeln!(s, "    \"{}\"{comma}", json_escape(n));
    }
    let _ = writeln!(s, "  ],");
    match &meta.runtime_dir {
        Some(d) => {
            let _ = writeln!(s, "  \"runtime_dir\": \"{}\",", json_escape(d));
        }
        None => {
            let _ = writeln!(s, "  \"runtime_dir\": null,");
        }
    }
    let _ = writeln!(s, "  \"arena\": {{");
    let _ = writeln!(s, "    \"format_version\": {},", meta.format_version);
    let _ = writeln!(s, "    \"layout_hash\": \"0x{:08X}\",", meta.layout_hash);
    match &meta.instance {
        Some(u) => {
            let _ = writeln!(s, "    \"instance\": \"{}\",", json_escape(u));
        }
        None => {
            let _ = writeln!(s, "    \"instance\": null,");
        }
    }
    let _ = writeln!(s, "    \"frames\": {},", meta.frames);
    let _ = writeln!(s, "    \"edges\": {},", meta.edges);
    let r = &meta.rings;
    let _ = writeln!(s, "    \"rings\": {{");
    let _ = writeln!(s, "      \"edges\": {},", r.edges);
    let _ = writeln!(s, "      \"declared_slots\": {},", r.reserved_slots);
    let _ = writeln!(s, "      \"declared_bytes\": {},", r.reserved_bytes());
    let _ = writeln!(s, "      \"used_slots\": {},", r.used_slots);
    let _ = writeln!(s, "      \"used_bytes\": {},", r.used_bytes());
    let _ = writeln!(
        s,
        "      \"rounding_slack_slots_max\": {},",
        r.rounding_slack_slots
    );
    let _ = writeln!(
        s,
        "      \"rounding_slack_bytes_max\": {},",
        r.rounding_slack_bytes()
    );
    let _ = writeln!(s, "      \"bytes_per_slot\": {}", crate::sizing::SLOT_BYTES);
    let _ = writeln!(s, "    }}");
    let _ = writeln!(s, "  }},");

    let (pass, fired, skipped, suppressed) = report.tally();
    let _ = writeln!(s, "  \"summary\": {{");
    let _ = writeln!(
        s,
        "    \"error\": {}, \"warn\": {}, \"info\": {},",
        report.count_at(Severity::Error),
        report.count_at(Severity::Warn),
        report.count_at(Severity::Info)
    );
    let _ = writeln!(
        s,
        "    \"passed\": {pass}, \"fired\": {fired}, \"not_run\": {skipped}, \"suppressed\": {suppressed}"
    );
    let _ = writeln!(s, "  }},");

    let _ = writeln!(s, "  \"checks\": [");
    for (i, o) in report.outcomes.iter().enumerate() {
        let comma = if i + 1 == report.outcomes.len() {
            ""
        } else {
            ","
        };
        let (status, reason) = match &o.status {
            Status::Pass => ("pass", None),
            Status::Fired => ("fired", None),
            Status::Skipped(why) => ("skipped", Some(why.as_str())),
        };
        let _ = writeln!(s, "    {{");
        let _ = writeln!(s, "      \"id\": \"{}\",", o.check.id());
        let _ = writeln!(s, "      \"title\": \"{}\",", json_escape(o.check.title()));
        let _ = writeln!(s, "      \"severity\": \"{}\",", o.check.severity().json());
        let _ = writeln!(s, "      \"status\": \"{status}\",");
        let _ = writeln!(s, "      \"suppressed\": {},", o.suppressed);
        match reason {
            Some(r) => {
                let _ = writeln!(s, "      \"reason\": \"{}\",", json_escape(r));
            }
            None => {
                let _ = writeln!(s, "      \"reason\": null,");
            }
        }
        if o.findings.is_empty() {
            let _ = writeln!(s, "      \"findings\": []");
        } else {
            let _ = writeln!(s, "      \"findings\": [");
            for (j, f) in o.findings.iter().enumerate() {
                let fc = if j + 1 == o.findings.len() { "" } else { "," };
                let edge = match f.edge {
                    Some(e) => e.to_string(),
                    None => "null".to_owned(),
                };
                let _ = writeln!(
                    s,
                    "        {{ \"edge\": {edge}, \"subject\": \"{}\", \"message\": \"{}\" }}{fc}",
                    json_escape(&f.subject),
                    json_escape(&f.message)
                );
            }
            let _ = writeln!(s, "      ]");
        }
        let _ = writeln!(s, "    }}{comma}");
    }
    let _ = writeln!(s, "  ],");

    let _ = writeln!(s, "  \"uncatalogued\": [");
    for (i, u) in report.uncatalogued.iter().enumerate() {
        let comma = if i + 1 == report.uncatalogued.len() {
            ""
        } else {
            ","
        };
        let _ = writeln!(
            s,
            "    {{ \"check\": \"{}\", \"severity\": \"{}\", \"subject\": \"{}\", \"message\": \"{}\" }}{comma}",
            json_escape(u.check),
            u.severity.json(),
            json_escape(&u.subject),
            json_escape(&u.message)
        );
    }
    let _ = writeln!(s, "  ]");
    let _ = writeln!(s, "}}");
    s
}

/// Escape a string for a JSON double-quoted scalar (frame names are untrusted).
#[must_use]
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn fired(check: Tft, findings: Vec<Finding>) -> CheckOutcome {
        CheckOutcome::ran(check, findings)
    }

    /// Ids are distinct and round-trip through `parse`.
    #[test]
    fn identifiers_are_unique_and_round_trip() {
        let mut ids: Vec<&str> = Tft::ALL.iter().map(|c| c.id()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate identifier in the catalogue");
        for c in Tft::ALL {
            assert_eq!(Tft::parse(c.id()), Some(c));
            assert_eq!(Tft::parse(&c.id().to_ascii_lowercase()), Some(c));
        }
        assert_eq!(Tft::parse("TFT10"), None);
        assert_eq!(Tft::parse("10"), None);
        assert_eq!(Tft::parse("TFT020"), None);
    }

    /// A hostile frame name cannot break the JSON document.
    #[test]
    fn json_escaping_survives_a_hostile_frame_name() {
        let nasty = "he said \"hi\"\\ then\nleft\u{1}";
        assert_eq!(
            json_escape(nasty),
            "he said \\\"hi\\\"\\\\ then\\nleft\\u0001"
        );

        let mut report = Report::default();
        report.outcomes.push(fired(
            Tft::Tft012,
            vec![Finding::about(Tft::Tft012, nasty, nasty)],
        ));
        report.uncatalogued.push(Uncatalogued {
            check: "out-of-order",
            severity: Severity::Error,
            subject: nasty.to_owned(),
            message: nasty.to_owned(),
        });
        let json = render_json(&report, &Meta::default());
        assert!(
            !json.contains("said \"hi\""),
            "raw quotes reached the output"
        );
        assert!(json.contains("said \\\"hi\\\""));
        assert!(
            !json.contains("then\nleft"),
            "a raw newline reached the output"
        );
        assert!(json_escape("naïve/frame").contains('ï'));
    }

    /// `--suppress` removes a check from the gate, not the report.
    #[test]
    fn a_suppressed_check_is_still_reported_but_does_not_gate() {
        let mut report = Report::default();
        let mut o = fired(
            Tft::Tft012,
            vec![Finding::about(Tft::Tft012, "arena", "two islands")],
        );
        o.suppressed = true;
        report.outcomes.push(o);

        assert!(!report.has_error(), "a suppressed error must not gate");
        let human = render_human(&report, &Meta::default());
        assert!(
            human.contains("two islands"),
            "the finding vanished:\n{human}"
        );
        assert!(
            human.contains("(suppressed)"),
            "and it must say so:\n{human}"
        );
        let json = render_json(&report, &Meta::default());
        assert!(json.contains("\"suppressed\": true"));

        report.outcomes[0].suppressed = false;
        assert!(report.has_error());
    }

    /// Skipped checks are always listed with their reason.
    #[test]
    fn the_human_report_names_every_check_it_could_not_run() {
        let mut report = Report::default();
        report.outcomes.push(CheckOutcome::skipped(
            Tft::Tft004,
            "nothing records a receipt time",
        ));
        report.outcomes.push(fired(Tft::Tft005, vec![]));
        let human = render_human(&report, &Meta::default());
        assert!(human.contains("1 not run"), "{human}");
        assert!(human.contains("TFT004"), "{human}");
        assert!(
            human.contains("nothing records a receipt time"),
            "the reason must be printed, not just the id:\n{human}"
        );
        assert!(
            !human.contains("TFT005  stamps"),
            "a passing check must not be listed as not-run:\n{human}"
        );
    }

    /// An id-less finding is visible, marked, and gates.
    #[test]
    fn an_id_less_finding_is_visible_marked_and_still_gates() {
        let mut report = Report::default();
        report.uncatalogued.push(Uncatalogued {
            check: "out-of-order",
            severity: Severity::Error,
            subject: "edge#7".to_owned(),
            message: "3 out-of-order stamp arrival(s)".to_owned(),
        });
        assert!(report.has_error(), "an id-less error must still gate");
        assert!(!report.is_healthy());

        let human = render_human(&report, &Meta::default());
        assert!(human.contains("out-of-order"), "{human}");
        assert!(
            human.contains("(no id)"),
            "an id-less finding must not imply an id:\n{human}"
        );
        assert!(human.contains("3 out-of-order stamp arrival(s)"), "{human}");

        let json = render_json(&report, &Meta::default());
        assert!(json.contains("\"uncatalogued\": ["), "{json}");
        assert!(json.contains("\"check\": \"out-of-order\""), "{json}");
        assert!(
            json.contains("\"error\": 1"),
            "the summary must count it, or it disagrees with the exit status:\n{json}"
        );
        let mut warn_only = Report::default();
        warn_only.uncatalogued.push(Uncatalogued {
            check: "unclaimed-dynamic",
            severity: Severity::Warn,
            subject: "edge#1".to_owned(),
            message: "no live writer".to_owned(),
        });
        assert!(!warn_only.has_error());
        assert!(!warn_only.is_healthy());
    }

    /// Info findings print but do not make a tree unhealthy.
    #[test]
    fn info_findings_do_not_make_a_tree_unhealthy() {
        let mut report = Report::default();
        report.outcomes.push(fired(
            Tft::Tft016,
            vec![Finding::about(Tft::Tft016, "host", "THP is 'never'")],
        ));
        assert!(report.is_healthy());
        assert!(!report.has_error());
        assert!(render_human(&report, &Meta::default()).contains("THP is 'never'"));

        report.outcomes.push(fired(
            Tft::Tft010,
            vec![Finding::on_edge(Tft::Tft010, 1, "edge#1", "hot")],
        ));
        assert!(!report.is_healthy());
        assert!(!report.has_error(), "...but still does not gate");
    }
}
