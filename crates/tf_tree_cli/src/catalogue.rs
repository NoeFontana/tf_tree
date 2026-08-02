//! The diagnostics catalogue — `docs/PHASE5.md` §6: identifiers, severities,
//! and the two output renderers.
//!
//! Detection lives in [`crate::checks`]; this module is the vocabulary and the
//! print layer.
//!
//! # What a stable identifier buys
//!
//! §6's opening sentence is the whole design: *"Each check has a stable
//! identifier so it can be suppressed, tested, and referenced from
//! documentation."* Before this, `doctor` had seven checks named by a local
//! `enum` whose `label()` was free to change with any refactor. A CI job that
//! wanted to gate on one of them had a substring of an English sentence to match
//! on, and a runbook that named one had no guarantee the name would survive.
//!
//! So the identifiers here are a **wire contract**, exactly like the arena
//! layout: an id never changes meaning, never gets recycled, and is what
//! `--json`, `--suppress` and every document refer to.
//!
//! # The catalogue is `TFT001`–`TFT019`
//!
//! §6's table lists sixteen; its amendments add three. Mapping the seven Phase 1
//! checks ([`crate::doctor`]) onto it goes:
//!
//! | Phase 1 check | Catalogue id |
//! |---|---|
//! | `multi-writer` | `TFT001` |
//! | `inconsistent-rate` | `TFT008` (jitter — a coefficient of variation *is* the inter-arrival spread) |
//! | `short-buffer` | `TFT011` |
//! | `cycle`, `unreachable` | `TFT012` (both are "the topology walk does not reach everything") |
//! | `unclaimed-dynamic` | `TFT017` |
//! | `out-of-order` | `TFT018` |
//!
//! `TFT019` maps onto no Phase 1 check: it is an **attribution** of `TFT018`'s
//! evidence, reading one more fact the arena already holds (`EdgeRecord::domain`)
//! to say that a wall clock stepped rather than that a publisher misbehaved.
//! Where `TFT018` says *what*, `TFT019` says *who*.
//!
//! **The last two got new ids rather than being folded into existing ones.**
//! `TFT013` is *declared but never published*, which is not *published and then
//! abandoned*; `TFT014` is a claim held by a slot whose owner is gone, which is
//! not *no claim at all*; `TFT006` is a check on a stamp's *value*, not on the
//! order stamps arrive in. Giving any of those a second meaning would defeat the
//! point of having stable ids. Appending two is additive — `--suppress` gains
//! two spellings, `--json` gains two entries in an array consumers already
//! iterate — whereas renumbering an existing id would break every runbook and CI
//! job that names one. §6's amendment records the decision.
//!
//! [`Uncatalogued`] survives that change and has no producer today. It stays for
//! two reasons: the `uncatalogued` key is part of the stable `--json` schema, so
//! removing it would break a consumer that reads it; and it is the shape any
//! future check without an id takes, which is the state this catalogue spent its
//! first revision in.
//!
//! Going the other way, some ids have no detection *here*: three cannot detect
//! anything in any configuration, and seven more depend on what this arena, this
//! engine build and this host can supply. Each is reported [`Status::Skipped`]
//! with the reason stated in [`crate::checks`], never silently passed.
//!
//! # Severity is a property of the check, not of the finding
//!
//! §6's table assigns one severity per id. A check therefore cannot emit an
//! `error` on Tuesday and a `warn` on Wednesday for the same condition, which is
//! what makes `--exit-code` a usable gate: the set of ids that can fail a build
//! is knowable from the documentation, before running anything.

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

/// A stable diagnostic identifier (`docs/PHASE5.md` §6). The numbering is the
/// specification's and does not change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tft {
    /// Multi-publisher conflict on an edge.
    Tft001,
    /// Static transform republished with a different value.
    Tft002,
    /// Edge kind changed (static <-> dynamic).
    Tft003,
    /// Clock skew between publishers.
    Tft004,
    /// Stamps in the future.
    Tft005,
    /// Zero or absurd stamps.
    Tft006,
    /// Publish rate deviates from the declared nominal rate.
    Tft007,
    /// Jitter: the inter-arrival distribution is far from its own centre.
    Tft008,
    /// Gaps / dropouts.
    Tft009,
    /// Extrapolation hotspot.
    Tft010,
    /// Ring capacity too small for the observed consumer lag.
    Tft011,
    /// Disconnected subtree.
    Tft012,
    /// Frame declared but never published.
    Tft013,
    /// Participant or claim slot leak.
    Tft014,
    /// Arena occupancy above 80%.
    Tft015,
    /// Transparent huge pages disabled, or `RLIMIT_MEMLOCK` below the arena size.
    Tft016,
    /// A dynamic edge with no live writer holding its claim.
    Tft017,
    /// Stamps arriving out of monotonic order on an edge.
    Tft018,
    /// A wall-clock domain stepped backwards — [`Tft::Tft018`]'s cause, not a
    /// publisher fault.
    Tft019,
}

impl Tft {
    /// Every check, in id order. [`crate::checks::run`] walks this, so a new
    /// variant cannot be added and then silently never executed.
    pub const ALL: [Tft; 19] = [
        Tft::Tft001,
        Tft::Tft002,
        Tft::Tft003,
        Tft::Tft004,
        Tft::Tft005,
        Tft::Tft006,
        Tft::Tft007,
        Tft::Tft008,
        Tft::Tft009,
        Tft::Tft010,
        Tft::Tft011,
        Tft::Tft012,
        Tft::Tft013,
        Tft::Tft014,
        Tft::Tft015,
        Tft::Tft016,
        Tft::Tft017,
        Tft::Tft018,
        Tft::Tft019,
    ];

    /// The stable identifier, e.g. `"TFT010"`. **Never changes.**
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Tft::Tft001 => "TFT001",
            Tft::Tft002 => "TFT002",
            Tft::Tft003 => "TFT003",
            Tft::Tft004 => "TFT004",
            Tft::Tft005 => "TFT005",
            Tft::Tft006 => "TFT006",
            Tft::Tft007 => "TFT007",
            Tft::Tft008 => "TFT008",
            Tft::Tft009 => "TFT009",
            Tft::Tft010 => "TFT010",
            Tft::Tft011 => "TFT011",
            Tft::Tft012 => "TFT012",
            Tft::Tft013 => "TFT013",
            Tft::Tft014 => "TFT014",
            Tft::Tft015 => "TFT015",
            Tft::Tft016 => "TFT016",
            Tft::Tft017 => "TFT017",
            Tft::Tft018 => "TFT018",
            Tft::Tft019 => "TFT019",
        }
    }

    /// A one-line title, matching §6's *Check* column.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Tft::Tft001 => "multi-publisher conflict on an edge",
            Tft::Tft002 => "static transform republished with a different value",
            Tft::Tft003 => "edge kind changed (static <-> dynamic)",
            Tft::Tft004 => "clock skew between publishers",
            Tft::Tft005 => "stamps in the future",
            Tft::Tft006 => "zero or absurd stamps",
            Tft::Tft007 => "publish rate deviates from nominal",
            Tft::Tft008 => "jitter: inter-arrival spread",
            Tft::Tft009 => "gaps / dropouts",
            Tft::Tft010 => "extrapolation hotspot",
            Tft::Tft011 => "ring capacity too small for observed consumer lag",
            Tft::Tft012 => "disconnected subtree",
            Tft::Tft013 => "frame declared but never published",
            Tft::Tft014 => "participant or claim slot leak",
            Tft::Tft015 => "arena occupancy above 80%",
            Tft::Tft016 => "transparent huge pages off, or RLIMIT_MEMLOCK below the arena size",
            Tft::Tft017 => "dynamic edge with no live writer",
            Tft::Tft018 => "stamps arriving out of order",
            Tft::Tft019 => "a wall-clock domain stepped backwards",
        }
    }

    /// The severity this check reports at, from §6's table.
    ///
    /// Fixed per id rather than per finding: `--exit-code` is only a usable
    /// gate if the set of ids that can fail it is knowable from the
    /// documentation, without running anything.
    #[must_use]
    pub fn severity(self) -> Severity {
        match self {
            Tft::Tft001 | Tft::Tft002 | Tft::Tft003 | Tft::Tft006 | Tft::Tft012 | Tft::Tft018 => {
                Severity::Error
            }
            Tft::Tft004
            | Tft::Tft005
            | Tft::Tft007
            | Tft::Tft008
            | Tft::Tft009
            | Tft::Tft010
            | Tft::Tft011
            | Tft::Tft014
            | Tft::Tft015
            | Tft::Tft017
            | Tft::Tft019 => Severity::Warn,
            Tft::Tft013 | Tft::Tft016 => Severity::Info,
        }
    }

    /// Parse an identifier for `--suppress`. Case-insensitive; `"TFT10"` and
    /// `"10"` are **not** accepted, because a near-miss that silently suppresses
    /// nothing is worse than an error.
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
    /// The edge this finding is about, if it is about one. Named in JSON so a
    /// consumer can key on it without parsing the message.
    pub edge: Option<u32>,
    /// A short label for what the finding is about (`"map->odom (edge#3)"`,
    /// `"slot 2 pid 4711"`, `"arena"`).
    pub subject: String,
    /// A human-readable explanation (this is the print layer's crate, so a
    /// `String` here is fine — the engine's errors stay `Copy`).
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

/// A finding from a Phase 1 check that `docs/PHASE5.md` §6 assigns no
/// identifier to — see the module docs for which two and why.
///
/// It carries a label rather than a [`Tft`], and that difference is the point:
/// there is nothing here for `--suppress` to name, and nothing a runbook can
/// cite. It still gates `--exit-code` at error severity, because the
/// alternative is that adding the catalogue silently downgraded a fault
/// `doctor` already failed on.
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
    /// It could not run. **The reason is mandatory**, because a check that
    /// silently does not run is indistinguishable from one that passed, and the
    /// difference is the entire value of the report.
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
    /// **A suppressed check still runs and still reports.** Suppression removes
    /// it from the `--exit-code` gate, not from the output: an operator who
    /// silenced `TFT013` on a fleet still wants to see it when they run `doctor`
    /// by hand, and a suppression that hid the finding would make the report a
    /// record of the flags rather than of the robot.
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

    /// Whether any **unsuppressed** error-severity check fired — the
    /// `--exit-code` condition.
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.count_at(Severity::Error) > 0
    }

    /// Whether the tree is clean: no unsuppressed warnings or errors.
    ///
    /// Info findings do not count. `TFT016` reports a host without transparent
    /// huge pages, which is a normal state worth printing and not a defect.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.count_at(Severity::Error) == 0 && self.count_at(Severity::Warn) == 0
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
    /// Frame count, for the one-line summary.
    pub frames: usize,
    /// Edge count.
    pub edges: usize,
    /// Wall-clock time the report was produced, nanoseconds since the epoch.
    ///
    /// §5.6 requires `doctor --json` output to be timestamped and appendable, so
    /// a field snapshot carries its own diagnosis rather than needing the fault
    /// reproduced on a bench.
    pub generated_unix_nanos: i64,
    /// The reference clock the time-based checks used, and where it came from.
    /// Printed because `TFT005`/`TFT006` are meaningless without it.
    pub now_nanos: i64,
    /// How `now_nanos` was obtained, for the header line.
    pub clock_source: &'static str,
    /// Whether the **engine** compiled `docs/PHASE5.md` §5's counters in.
    pub counters_compiled_in: bool,
    /// Disclosures that are not findings and not whole-check skips: a check
    /// that ran but with one of its evidence sources missing.
    ///
    /// [`Status`] is deliberately three-valued, so there is nowhere in an
    /// outcome to record "ran, but half blind". Dropping the fact instead would
    /// leave a `pass` that was never earned, which is the one thing this
    /// report's shape exists to prevent.
    pub notes: Vec<String>,
}

/// Render the human-readable report — the default output.
///
/// Grouped by severity, worst first, because the first screen is all an
/// operator reads under pressure. Skipped checks come last **and are always
/// printed**: `doctor` claiming a clean bill of health it did not earn is the
/// specific failure this layout exists to prevent.
#[must_use]
pub fn render_human(report: &Report, meta: &Meta) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "tf_tree doctor ({})", meta.source);
    if let Some(uuid) = &meta.instance {
        let _ = writeln!(s, "  instance {uuid}");
    }
    let _ = writeln!(
        s,
        "  arena format_version {} layout_hash 0x{:08X}  {} frames, {} edges",
        meta.format_version, meta.layout_hash, meta.frames, meta.edges
    );
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
        // **Say what was not checked.** A live arena has no recorded push
        // stream, a build without the bridge cannot see a publisher conflict,
        // and no arena records a receipt time — none of which are visible in a
        // report that lists only findings.
        let _ = writeln!(s, "  not run:");
        for o in &report.outcomes {
            if let Status::Skipped(why) = &o.status {
                let _ = writeln!(s, "    {}  {} — {why}", o.check.id(), o.check.title());
            }
        }
    }
    s
}

/// The `--json` schema identifier. **Bump only for an incompatible change**;
/// adding a check, or a field, is compatible by construction.
pub const JSON_SCHEMA: &str = "tf_tree.doctor/1";

/// Render the machine-readable report.
///
/// Hand-written rather than via `serde_json`: the structure is flat, and
/// `docs/PROJECT.md`'s dependency budget is not worth spending on a serializer
/// for five object shapes. [`json_escape`] is the only part that is easy to get
/// wrong, so it is a separate, tested function.
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
///   "notes": [ string ],              // checks that ran with evidence missing
///   "arena": { "format_version": u32, "layout_hash": "0x........",
///              "instance": string|null, "frames": u32, "edges": u32 },
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
/// `checks` always carries **every** id in the catalogue, including the ones
/// that passed, so a consumer can tell "this check did not fire" from "this
/// build does not have this check". `summary.error`/`warn`/`info` count
/// `uncatalogued` findings too, so they agree with the process exit status.
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
    let _ = writeln!(s, "    \"edges\": {}", meta.edges);
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

/// Escape a string for a JSON double-quoted scalar.
///
/// Frame names come from somebody else's robot and reach this function
/// unmodified, so this is not decorative: a frame called `he said "hi"` would
/// otherwise emit a document no parser accepts, and one containing a newline
/// would emit one that parses into the wrong thing. Everything below `0x20` is
/// escaped, `"` and `\` are escaped, and valid UTF-8 above that passes through
/// (JSON strings are Unicode, so `\u` escaping non-ASCII would only make the
/// output larger).
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

    /// **Every identifier is distinct, parses back to itself, and appears in
    /// the catalogue exactly once.**
    ///
    /// The ids are a wire contract: `--suppress`, `--json` consumers and the
    /// runbook all key on them. A duplicate would silently make one check
    /// unsuppressable and unreferenceable, and `Tft::parse` — a linear scan of
    /// `ALL` — would resolve the shadowed id to the wrong variant.
    ///
    /// Mutant: give `Tft::Tft011` the id `"TFT010"`. Applied: the uniqueness
    /// assertion fires (`duplicate identifier in the catalogue`), and so does
    /// the round-trip, which resolves `"TFT010"` to `Tft010`.
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
        // A near miss must not resolve to something: silently suppressing
        // nothing is the failure mode worth refusing.
        assert_eq!(Tft::parse("TFT10"), None);
        assert_eq!(Tft::parse("10"), None);
        // One past the end of the catalogue, so this line moves every time the
        // catalogue is appended to — which is the point: it is what proves
        // `parse` is driven by `ALL` and not by a hand-written list that a new
        // variant can be forgotten from.
        assert_eq!(Tft::parse("TFT020"), None);
    }

    /// **A hostile frame name must not be able to break the JSON document.**
    ///
    /// Frame names arrive from somebody else's robot and are interpolated into
    /// `subject` and `message` verbatim. A name containing a quote would end the
    /// string early and produce a document no CI consumer can parse; one
    /// containing a newline would produce a document that parses into something
    /// else.
    ///
    /// Mutant: make `json_escape` the identity (`s.to_owned()`). Applied: the
    /// first `assert_eq!` fails, and so do the two `render_json` assertions
    /// about raw quotes and a raw newline reaching the output.
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
        // Non-ASCII passes through: JSON strings are Unicode.
        assert!(json_escape("naïve/frame").contains('ï'));
    }

    /// **`--suppress` removes a check from the gate, not from the report.**
    ///
    /// An operator who silenced a known-benign finding on a fleet still wants to
    /// see it when they run `doctor` by hand; a suppression that hid it would
    /// make the report a record of the flags rather than of the robot.
    ///
    /// Mutant A: drop the `!o.suppressed` filter from `Report::at`. Applied: the
    /// `has_error` assertion fails. Mutant B: skip suppressed outcomes when
    /// rendering — `if o.suppressed { continue; }` in `render_human`. Applied:
    /// the `two islands` assertion fails.
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

        // Non-vacuity: the same finding unsuppressed does gate.
        report.outcomes[0].suppressed = false;
        assert!(report.has_error());
    }

    /// **The report always lists what it did not check.**
    ///
    /// `doctor` printing only findings turns "no evidence" into "no problem",
    /// which is the specific dishonesty the live-arena banner was added for in
    /// Phase 2 and which the catalogue must not lose.
    ///
    /// Mutant: delete the `if skipped > 0 { ... }` block from `render_human`.
    /// Applied: the `TFT004` and reason-text assertions both fail.
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

    /// **An id-less Phase 1 finding is visible, is marked as id-less, and still
    /// gates.**
    ///
    /// `out-of-order` and `unclaimed-dynamic` have no §6 identifier. Reporting
    /// them without saying so would imply an id that does not exist; dropping
    /// them would mean the catalogue silently deleted two working checks; and
    /// letting the error-severity one stop gating would mean `doctor` stopped
    /// failing on a fault it used to fail on.
    ///
    /// Mutant: make `Report::count_at` ignore `self.uncatalogued`. Applied: both
    /// the `has_error` and the `is_healthy` assertions fail.
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
        // Non-vacuity: a warn-severity id-less finding does not gate.
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

    /// Info findings are printed but do not make a tree unhealthy: a host
    /// without THP (`TFT016`) is a normal state, and a `doctor` that called it a
    /// defect would be one nobody runs.
    ///
    /// Mutant: add `|| self.count_at(Severity::Info) > 0` to `is_healthy`.
    /// Applied: the first assertion fails.
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

        // Non-vacuity: a warn-severity finding *does* make it unhealthy.
        report.outcomes.push(fired(
            Tft::Tft010,
            vec![Finding::on_edge(Tft::Tft010, 1, "edge#1", "hot")],
        ));
        assert!(!report.is_healthy());
        assert!(!report.has_error(), "...but still does not gate");
    }
}
