# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning, stated before anything else, because `0.0.x` is not ordinary
semver.** Cargo treats every `0.0.x` release as incompatible with every other:
`^0.0.1` — which is what a bare `tf_tree = "0.0.1"` means — matches `0.0.1` and
nothing else. That is deliberate and it is the whole promise. **Nothing in this
release is stable. Every release may break every other**, in the public Rust
API, in the Python API, in the C ABI, and in the arena format. Pin exactly.

Two consequences worth naming rather than leaving to be discovered:

- **PyPI does not have that rule.** PEP 440 gives `0.0.x` no special meaning, so
  `pip install -U transform_tree` will move you from `0.0.1` to `0.0.2` without asking.
  Pin the wheel yourself if the promise above matters to you.
- **`SUPPORT.md`'s "an MSRV bump is a minor-version bump pre-1.0" rule does not
  apply on the `0.0.x` line** — there is no minor slot for it to occupy. The
  argument, and why the resolver already enforces what that rule was written to
  enforce, is in the root `Cargo.toml`'s comment on `[workspace.package]
  version` and in `SUPPORT.md`'s MSRV section.

The single source of truth for what is implemented is the status tables in
`docs/` — `## 0.0 Implementation status` at the head of `PHASE2.md`, `PHASE4.md`
and `PHASE5.md`, and `## 0.0 Status` in `PHASE7.md`. `PHASE1.md` has none because
Phase 1 is implemented whole; `PHASE3.md` has none because it records deviations
inline, in the section each one belongs to — §5.5's buffer-protocol steps are the
example. Where this file and one of those disagree, they are right and this file
is a bug.

---

## [Unreleased]

### Added

- **`docs/PHASE2.md` §11.3's last two untested crash sites now have tests**, and
  the facade's site list has the completeness gate it never had.
  `reclaim.after_probe_before_cas` and `hangup.after_probe_before_cas` were the
  two the §0.0 row recorded as "a site and no test", and the reason they were
  *those* two is structural: they are both in `tf_tree::CRASH_SITES`, the list
  with no counterpart to
  `crash_tests::the_published_site_list_is_the_one_the_tests_arm`.

  `the_facade_site_list_is_pinned_by_index_and_every_site_has_a_test` closes it
  and **pins index to name**, not set membership: the facade arms its sites *by
  index* (`CRASH_SITES[0]`…`[5]`), so a set-equality assertion would still pass
  after a reorder that left `reap_participants` arming the *hangup* name and the
  hangup callback arming the *reclaim* name — every test still green, every
  crash point lying about where it fired.

  `reclaim.*` needed a sweeper that is **not the test binary** (every
  `reap_participants` call in `tests/rendezvous.rs` runs in-process, where arming
  the site aborts the runner), hence `rendezvous_child`'s new `join-sweep` arm;
  and its fixture has to kill the **owner**, because a joiner's death fires the
  hangup callback that collects its record before any sweep can find it.
  `hangup.*` needed the **owner** armed, where every other crash test arms a
  joiner.

  **Both were verified against a disarmed site**, and `Kid::wait_within` exists
  because of what that showed: with the unbounded `Kid::wait`, a site that stops
  firing produces a 180-second nextest timeout, which says "something hung"
  rather than "the crash point is now a no-op". Disarmed, the two tests now fail
  in 0.03 s and 20 s with messages that name the finding.

- **`just owner-migration`** — `docs/PHASE2.md` §12.2's two ownership-migration
  rows and **§12.3 gate 4b**, which had no artifact. §3.5's migration shipped on
  2026-08-28 with correctness tests in `crates/tf_tree/tests/rendezvous.rs`, and
  nothing under `crates/tf_tree_bench/` referenced `owner_lost` or
  `inherit_ownership` — so a *normative* criterion of a phase recorded
  **Implemented** could not be evaluated, and `docs/benchmarks/EVIDENCE.md`
  carried no row for it.

  Five processes, and the split is the design: an **owner** that only serves the
  rendezvous (killing a publishing owner would stop the data stream, so "zero
  failed lookups" would be measuring the writer's death), a **writer** that is
  never killed, an **heir** running §3.5's caller-driven trigger, and read-only
  **readers** that make no control-plane call at all.

  Measured on this host: kill → a fresh process can join again at **0.6–1.2 ms
  p50, 1.1–2.0 ms p99**, and **zero failed lookups** in every run.

  **The p99.9 quotient is only weakly evaluable, and the row it feeds says so
  rather than quoting a number.** It reads 0.976–1.093 at the default five
  migrations — one run of five past gate 4b's 1.05 — and exactly 1.000 at
  `--repeat 15`. Those are the same fact: any window wide enough to contain the
  migration is dominated by steady-state samples, so the quotient's sensitivity
  *falls* as its sample count rises. It cannot be made both stable and sensitive
  by tuning the window or the repeat count, and picking the count that passes
  would be choosing the vacuous end on purpose. The default therefore stays at
  the sensitive-but-noisy end, the recipe is wired into **no CI workflow**, and
  re-cutting the criterion is a decision record — `0023` did exactly that for
  `PHASE4` §7 gate 1, which is the same shape of finding. What is load-bearing
  and did not move: zero failed lookups, and a per-phase stall count of 510–542
  per million steady against 517–531 during.

  **Three things it refuses to do quietly.** A run whose writer this host
  starved exits `INVALID`, not `FAIL` — that is a statement about the scheduler,
  and charging it to the arena is the misattribution this project has shipped
  before. A composed-path refusal reporting an *inverted* window is counted and
  printed separately rather than absorbed. And the gate's arithmetic is asserted
  to be capable of failing (`gate_arithmetic_is_not_vacuous`), because the first
  revision used a 750 ms window, made the during-histogram 99.7% steady-state
  samples, and printed exactly `1.000` on three consecutive runs — a gate that
  cannot fail, which is the same vacuous-green shape `shm_torture`'s first
  revision had.

- **`shm_torture --crash-points`** — `docs/PHASE2.md` §11.4's *"a random crash
  point armed in 10% of children"*, which §11.3 and §11.4 could not do for each
  other until both existed. A random site from `tf_tree_core::crash::SITES` and
  `tf_tree::CRASH_SITES` — read from the published lists, never re-spelled — is
  armed in about a tenth of children, so a soak kills processes **at named
  instructions** and then checks the same invariants.

  Measured at 40 s / 10 children / 2 Hz: **11 armed, 4 aborted at four distinct
  sites** (`attach.after_slot_assigned_before_publish`, `push.after_seq_odd`,
  `push.after_data_before_seq_even`, `claim.after_cas`), 0 violations, clean
  recovery. The first of those is worth noting: §11.3 records that its ~12 ns
  window cannot be reached without fault injection, so a torture run now
  *produces* the state §11.2's collector tests stage.

  **The run reports armed and aborted separately**, because they differ — an
  armed child the driver's `SIGKILL` reached first never got to its site, and
  `armed N, aborted 0` exercised nothing. The flag is **refused** by a build
  without the feature: the children are the same executable, so a compiled-out
  site would arm nothing while looking like it had. That refusal replaces an
  older unconditional one whose stated reason ("there is nothing to arm") had
  expired.

- **`doctor --exit-code[=error|warn]`.** Bare `--exit-code` still means `error`,
  so no existing invocation changes. The `warn` tier is what was missing.

  Six ids carry `Error`, and on a **live** arena four of them structurally skip
  (`TFT001`, `TFT002`, `TFT003`, `TFT018` all need evidence an arena does not
  carry) — so `--exit-code` reduced to `TFT006` and `TFT012`. Those are the right
  errors: both make every lookup fail. But almost everything an operator is
  paged about is `Warn` — a dynamic edge with no live writer, an undersized ring,
  rate collapse, gaps, clock skew, a slot leak, an arena at 100% capacity — and
  all of it exited 0.

  **The capability was already there and only the exit code was missing.**
  `doctor --json | jq -e '.summary.warn == 0 and .summary.error == 0'` gates on
  exactly that today, and `Report::is_healthy` was written and unit-tested for it
  with **no caller**. `warn` is *warn-and-above*, so an arena with a cycle does
  not pass it because nothing warned; `--suppress` remains the escape hatch for a
  warn a fleet has decided to live with.

- **Every row of `docs/PHASE2.md` §11.3's crash matrix is now placed, executed,
  or argued not to be a crash point.** Six more sites land here:
  `topo.holding_lock`, `open.after_ownership_lock_before_bind`,
  `open.after_create_before_bind`, `attach.after_slot_assigned_before_publish`,
  `reclaim.after_probe_before_cas` and `hangup.after_probe_before_cas`.
  `takeover.after_ownership_lock_before_bind` was the only one outside
  `tf_tree_core` before this.

  **`attach.after_slot_assigned_before_publish` closes a gap the table itself
  stated.** Its window — the `FREE -> RESERVED` CAS to the `live_word` store — is
  ~12 ns, so nothing outside fault injection could kill a process inside it, and
  §11.2's two collector tests therefore *staged* the word by hand and said so.
  They still do, and they are still the repair; what changes is that the state
  they collect is now produced by a real death.

  Two of the six carry a site and no test yet, and the rows say which:
  `hangup.after_probe_before_cas` needs a joiner to hang up while the owner is
  armed, and `reclaim.after_probe_before_cas` is the general sweeper's version of
  the same shape.

  `topo.holding_lock` sits **before** `set_parent`, deliberately: after it, the
  surviving state is indistinguishable from a completed reparent whose guard had
  not dropped, and the test would pass on a build where A2's byte was never taken
  at all.

- **`reclaim.probe_then_reoccupied` is recorded as *not an abort site*, and that
  is the finding rather than a shortfall.** Every other row names an instruction
  a process can die at; this one names an **interleaving between two processes
  that are both alive**. Killing either does not produce it. `crash_point!` is
  the wrong tool, and placing one there would produce a test that passes without
  ever reaching the state. The row's analysis — the `RESERVED` word carrying no
  incarnation, bounded by the byte rather than the word — stands unchanged; what
  is retracted is only its membership in a table of crash points. `loom` and
  `shm_torture` are the mechanisms that can reach it.

### Fixed — documentation

- **Six Python docstrings pointed where a wheel user cannot go.** PyO3 copies a
  Rust `///` comment into `__doc__` verbatim, so an intra-doc link written for
  rustdoc reaches a Python user as those characters: `help(Tree.span)` printed
  ``[`span_impl`](crate::offline::span_impl)`` — a private path in a crate that
  is `publish = false`. Six methods (`span`, `frames`, `edges`, `freeze`,
  `Plan.edges`, `instance_uuid`) delegated their *substance* that way, so the
  answer to "what are the three cases?" was a link to something nobody outside
  this repository can open. Each now says the thing.

  A test walks every `__doc__` in the module and fails on a rustdoc link, so the
  next one does not ship. A rustdoc link is fine anywhere the reader is a Rust
  developer; a PyO3 docstring is not such a place.

- **`docs/decisions/0003` said Phase 1 amendments A1–A8 are "not yet applied".**
  They have all been applied since `FORMAT_VERSION` 2. It is the only mention of
  A1–A8 in that record, and it points at the section `CLAUDE.md` names as the
  reason several atomic orderings in the concurrency core look odd — so a reader
  who believed it would take those orderings for accidents. Corrected in place
  rather than left as history, which is what the rest of that superseded record
  is.

### Added

- **`just two-processes`** — the capability the README leads with, as something
  you can run. `crates/tf_tree/examples/` held two files and neither showed a
  second *process*: `control_loop.rs`'s reader is a thread, deliberately,
  because that example is about latency and a thread keeps the measurement about
  the fold. The only publisher/consumer pair in the repository was the test
  harness, whose own README says it "is not a tool and nothing about it is
  stable".

  One target with a `--publish`/`--consume` switch, so it stays one recipe. It
  shows the two things a newcomer gets wrong: `require_create(true)`, without
  which a publisher racing a second copy of itself silently *joins* the other's
  arena and publishes into a topology it did not declare; and `await_open`
  rather than `open`, because launch order is not something a launch file
  guarantees.

### Fixed

- **The "inverted composed window" recorded one commit ago was the wrong
  mechanism, and the error carried the refutation in its own shape.**
  `docs/PHASE2.md` §12.3 said "only the composed path can produce it … it
  intersects the four edges' windows". `LookupError::Extrapolation` names a
  *single* `edge` (`crates/tf_tree_core/src/error.rs:156`), so its
  `oldest`/`newest` are one ring's bounds and never an intersection.

  The real mechanism is a sampling race in how those bounds are read.
  `SampleCursor::sample` loads `head`, then reads the two stamps with **two
  independent `Relaxed` loads** (`crates/tf_tree_core/src/sample.rs:139-141`);
  `stamp_at` is a bare `Relaxed` load with no seqlock (`:323-325`), deliberately,
  because these are bounds probes rather than sample reads. A writer that laps
  the ring between the two loads leaves the older slot holding a stamp *newer*
  than the one already read for `newest`, and the pair inverts by exactly the
  number of slots it advanced — two, at the 4 ms observed against a 2 ms publish
  period.

  The refusal itself stays correct and conservative; only the reported pair is
  inconsistent. It is consumed, though: a torn pair files as `extrap_before` and
  its `gap` reaches `worst_extrap_gap_ns`, which `TFT011` reads against the
  ring's retained span. **Measured bound, rather than an assumed one: it did not
  come close to mattering** — ~50 ms of bogus gap against an ~8 s span. Making
  the two bounds mutually consistent is a hot-path change under a concurrent
  writer, so it is a decision record rather than a PR. `owner_migration` now
  calls the class `torn-bounds` instead of `disjoint`.

- **Three release-readiness claims that the 2026-08-17 publish falsified.**
  `docs/PHASE3.md`'s Appendix B stated in bold that
  `.github/workflows/wheels.yml` "**has still never executed**" and that its
  cross-platform rows — musllinux, macOS, Windows, aarch64 — were "unproven".
  It has run **five times** and was green on `v0.0.3` (2026-08-19) and `v0.0.4`
  (2026-08-22), with every wheel row succeeding, `abi3.abi3t` present and
  skipped exactly as §14 specifies, and `publish` succeeding. §14's matching
  checkbox carried the same stale parenthetical.

  `docs/PHASE3.md` §14 also left **PEP 740 attestations** unticked: they are
  published, and have been since the first green tag — `wheels.yml`'s `publish`
  job carries `attestations: write` and `attestations: true` under Trusted
  Publishing. The row is split rather than ticked, because its SBOM half is
  genuinely absent.

  `docs/PHASE5.md` §10 listed "release automation (`cargo-dist`, PEP 740
  attestations, signed tags)" as not done, on the stated premise that "all three
  are ceremony **until there is a release**" — a premise that expired when the
  project started publishing. Release automation exists and has run; attestations
  are published; `cargo-dist` is not owed, because the only binary in the
  workspace is `publish = false` by decision. **Signed tags are the one item of
  the three genuinely open** — all four tags are unsigned, checked.

  What remains under §10 is therefore the mdBook site, the SBOM, and signed
  tags, and the honest reason for the first two is no longer "until there is a
  release".

- **`doctor`'s zero-counter skip reason named three causes and missed the one a
  running robot is usually in.** `Guard::drop` and `note_err` both early-return
  on a non-writable view, so a **read-only** participant records nothing —
  writing a counter is a write, and read-only is the consumer default (D18). On
  the ordinary deployment (one publish-only publisher, N read-only consumers)
  `TFT010` and `TFT011` see nothing *while consumers are hammering the arena*,
  and the message closed with "a live arena reaches it before its first
  consumer" — which reads as though the state ends when a consumer attaches. It
  does not. The reason now names the read-only cause and points at
  `tf_tree participants` for the rw/ro split.

  The predicate was always right — it reads the evidence, not the source. What
  was wrong was the prose, which sent an operator looking for a bag-shaped
  explanation of a deployment-shaped state. `docs/PHASE5.md` §5.5's amendment
  justified the design with "`doctor` reports what the *writable* participants
  recorded … the bridge and every publisher"; a counter is incremented by a
  **lookup** and a bridge does not look up, so that justification does not hold
  for the two consumer-facing checks. Corrected in place rather than overwritten.

### Changed — documentation

- **`EdgeWriter::push` says that the stamp's domain is unchecked there.** The
  read side takes `Stamp<D>` and refuses a mismatch; `push` takes a bare `i64`
  and `PushError` has no variant for one, so an edge declared `domain(1)`
  accepts wall-clock nanoseconds with `Ok(())`. What covers it and how far is
  now written down: `TopologyConfig::check_domain` guards the ROS 2 ingest path
  by comparing two *declarations*, and `TFT004` catches only the direction where
  the arena's stamps still look like Unix time.

  Making `push` generic over `D` is **not** the fix and the doc says so:
  `Stamp::<SensorDomain>::from_nanos(wall_clock_nanos)` compiles either way.
  `Stamp<D>` is an unchecked assertion at *both* ends — the read side compares
  one assertion against a declaration, not a clock against a clock. Symmetry is
  worth a decision record on its own terms, not as a claim that the clock
  becomes checked.

### Added

- **Recovery from C, and a read-write attachment to recover *with*.**
  [`0044`](docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md).
  `docs/PHASE2.md` §3.5's ownership migration and the reapers were Rust-only, so
  an all-C++/Python fleet whose arena owner was `SIGKILL`ed **could not rejoin
  it**: survivors keep their participant bytes, §3.4's split-brain check refuses
  every new create, and the documented recovery was to stop every attached
  process. ROS 2 nodes are written in C++ and Python.

  New in the **unstable** tier (`tf_tree_unstable.h`), because §3.5's protocol
  is days old and the frozen header is a promise about a decade:
  `tft_tree_open_named`, `tft_tree_owner_lost`, `tft_tree_inherit_ownership`,
  `tft_tree_reap_dead`, and the five `TFT_INHERITED`…`TFT_NOT_APPLICABLE`
  values.

  New in Python: `Tree.owner_lost()`, `Tree.inherit_ownership()` — which returns
  the outcome's **name** as a string, since that is what a Python caller
  branches on — and `Tree.reap_dead()`. Python already had `open(mode="rw")`, so
  it needed no equivalent of `tft_tree_open_named`. All three are present off
  Linux too, answering `False` / `"NotApplicable"` / `0`: shared arenas are
  Linux-only, so those are the true answers rather than a stub, and a portable
  script does not meet an `AttributeError` at a line that has nothing to do with
  the reason.

  **`tft_tree_open_named` was not in the record and the other three are
  decoration without it.** `tft_tree_open(out)` was the entire arena-opening
  surface of the C ABI and it is `tf_tree::open()` — read-only, name from the
  environment — so a C consumer could only ever hold a `PROT_READ` mapping and
  `tft_tree_inherit_ownership` would answer `TFT_READ_ONLY` every time. Found by
  the C test failing to compile against a signature that did not exist.

### Changed

- **`Tree::inherit_ownership` takes `&self`.** A relaxation, so every existing
  caller still compiles. It took `&mut self`, which both bindings cannot satisfy
  — they hold the tree in an `Arc` and `Arc::get_mut` fails as soon as any plan
  or publisher holds a clone — and which cost `docs/PHASE2.md` §3.5 a
  caller-side qualification: the inheriting handle's own `Guard<'_>` could not
  be outstanding across the call, so a control loop had to arrange recovery
  between cycles. Both are gone. The attachment moved behind a `Mutex`, which
  the read path never touches: `Plan::at` lives in `tf_tree_core` and cannot
  name the field.

### Fixed

- **An exact query returns the pose of the stamp it named, or refuses.**
  `SampleRing::sample`'s `# Errors` promises `SlotRecycled` when the ring lapped
  the reader mid-read, and the interpolating tail enforced it — but every arm
  that *short-circuits* returned the slot directly: `ExtrapPolicy::Hold`, an
  exact hit on the **newest** stamp (in both `sample` and `sample_from`),
  `sample_with_twist_seeking`'s `Hold`, and `constant_twist`'s single-sample
  case. Six return sites. A reader descheduled long enough for the ring to lap
  got a complete, valid pose belonging to a **different stamp** — the seqlock
  catches a torn slot, not a recycled one — while naming an *interior* stamp
  four lines away was refused for exactly the same race.

  The sharp one is the exact-newest pair: a caller that names a stamp can be
  handed the pose of another. A 64-slot ring — `Capacity::slots(64)`, what the
  ABI cost fixture uses — laps in 64 samples, which is one ordinary preemption
  at 1 kHz. One shared `revalidated` helper now covers all six, so there is one
  spelling of the check rather than seven.

- **`tf_tree tree`'s `age(ms)` column is measured against a real clock.** It
  was `fixture::NOW_NS - newest` — the in-process benchmark rig's synthetic
  "now", `9_900_000_000`. Against a live arena that number is arbitrary
  (measured: ~8 750 ms of age for a transform pushed milliseconds earlier), and
  against a robot stamping Unix nanoseconds the subtraction clamps and every
  edge reads `0` however long its publisher has been dead. It now uses
  `Clock::decide`, the estimator `doctor` and `top` already share, and the
  header says which clock it picked.

- **The runbook stopped naming a command that does not exist.** Its
  startup-ordering section offered `tf_tree serve --config` as the supervised
  remedy; `docs/PHASE2.md` §0.0 records `tf_tree serve` as not implemented. The
  remedy is the topology config itself, which the ROS bridge, the CLI's
  `topology --discover`, and Python all already speak.

- **The `tf_tree` crates.io front page stopped telling adopters that
  `LookupError` does not implement `std::error::Error`.**
  [`0040`](docs/decisions/0040-the-error-that-cannot-be-returned.md) made that
  false and did not touch the README, so the first page a Rust adopter reads
  talked them out of `?` and into hand-rolled match arms over a limitation that
  no longer exists. `0019`'s Context carried the same expired clause.

- **`TFT009` reports a publisher that has *stopped*, which no check could see.**
  Every rule in the catalogue measured intervals *between retained stamps*, so a
  publisher that died three weeks ago left a full ring of perfectly spaced
  samples: the median period reads healthy, the largest gap is one period, and
  `doctor` reported nothing — while the transform every consumer reads had been
  frozen since. That is the most common fault in the field, and the only thing
  in the project that saw it was `tf_tree top`, which needs two ticks to notice
  a `head` that did not move.

  A single snapshot compares the newest stamp against the reference clock
  instead. Same id (so `doctor` and `top` agree), same `GAP_FACTOR`, same
  per-edge median — the trailing gap is just the open end of the same
  inter-arrival distribution, so nothing new is calibrated.

  It runs only on a **live** arena with a **wall-comparable** clock: on a frozen
  `.tft` or a bag the distance is the age of the recording, and firing there is
  the false positive that makes an operator stop reading the report. When it
  cannot run, `doctor`'s report metadata says so — a check that quietly does
  half its work is indistinguishable from one that passed.

- **A killed publisher's claims are revoked by the owner, so a restarted node
  can take its own edges back.** `docs/PHASE2.md` §3.9 says a dead
  participant's *"arena-side records"* are the owner's to reap; the hangup
  callback freed the participant **record** and left every **claim** that
  participant held. `Tree::reap_participant` had been written for that call
  site — its doc names `EPOLLHUP` as how the owner learns *which slot* went
  away — and had **no caller** in the workspace outside a benchmark and a test
  helper. There is no reap surface in `tf_tree_c` or `tf_tree_py` and no CLI
  subcommand that reaps, so nothing in a deployment invoked it.

  What that cost is the ordinary supervised restart. The publisher is killed,
  the supervisor restarts it, the assigner hands it **its predecessor's slot** —
  and `reap_claims` skips `own_slot`, because `F_OFD_GETLK` does not report a
  description's own byte. So the one process that needs those edges is the one
  that cannot repair them, and it is refused `EdgeAlreadyClaimed` forever.
  Measured before the fix: `refused AlreadyClaimed { owner_slot: 1 }`, with the
  claim word still reading `0x10002` after the holder was `SIGKILL`ed.

  Two producers of a stale claim remain and are **not** closed by this: a dead
  **owner**, whose hangup nobody observes, and a `TreeBuilder::build_shared`
  participant, which has no socket. `Tree::reap()` from a surviving read-write
  participant is still the only collector for those, and it is reachable from
  Rust only.

- **`Extrapolated::by_ns` could report `0` for a pose the fold invented.** The
  distance was derived from a walk that ran *after* the fold, so a `push`
  landing in between with a stamp at or past the query lifted the common newest
  stamp past it — and `0` means *"every edge bracketed the query — interpolated,
  not invented"*, which is the one claim the type exists to make unmissable. A
  1 kHz controller reading a 100 Hz estimator crosses that stamp regularly. The
  walk now runs **before** the fold; `SampleRing::newest_stamp` is
  non-decreasing, so the error inverts into the safe direction — `by_ns` may
  over-report a query that was in fact bracketed, and `0` is sound. No signature
  change, and `Plan::at` is untouched.

- **`Plan::at_many` returns `LookupError::BufferTooSmall` instead of panicking**
  on a short output buffer, as `at_many_into` and `at_many_into_f32` already
  did. It was an `assert!` — unconditional, so it unwound in release and aborts
  outright under the `panic = "abort"` profile a control loop is built with —
  while the `# Errors` section two lines above called the check "debug-time".
  `clippy::panic`, which the workspace denies precisely to keep this out of the
  engine, does not lint `assert!`.

- **`Tree::owner_lost` answers a question about the owner, not about this
  process's socket** ([`0043`](docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md)).
  It polled only the attach socket, which points at the dead owner and stays hung
  up for the life of the process — so after any takeover **every survivor but the
  winner was told `true` forever**, and `docs/PHASE2.md` §3.5's recommended loop
  re-attempted an `F_OFD_SETLK` on byte 0 every control cycle. On a fleet of *N*
  read-write survivors that is *N−1* processes doing a syscall per cycle, in the
  loop this library exists to keep quiet, to be told each time that somebody else
  owns the arena. A hangup is now followed by `F_OFD_GETLK` on byte 0 of the lock
  file the session already holds, so *role taken or mid-bind* separates from
  *role vacant*. **The healthy path is unchanged** — one non-blocking `poll` that
  answers `false`, no probe — and a survivor that stopped calling **starts again
  by itself** if the new owner dies too, which is why latching was never the fix.
  `docs/RUNBOOK.md` says to delete any latch written to work around the old
  behaviour.

  Two things it does **not** do. §3.5's literal *"retry connect with backoff"* is
  still not implemented: it needs a new wire message, because §3.5 requirement 2
  forbids a survivor from registering a second time, and its only remaining
  benefit is that the new owner learns of that participant's death promptly —
  the byte-keyed collectors still reclaim its slot, a grant or a sweep later.
  And one *correct* behaviour changed observably: a survivor that evaluates
  `owner_lost` after the winner took byte 0 now reports
  `Inheritance::OwnerAlive` where it reported `Contended`, having never attempted
  the lock. `Contended` remains reachable for a genuine tie.

### Added

- **`tf_tree_ipc::Session::ownership_held`** — does anyone *else* hold the
  ownership byte, from `F_OFD_GETLK` on the description the session already
  holds. The second half of the question above. A description never conflicts
  with itself, so an owner asking gets `false`; it is not "am I the owner".

### Changed — documentation

- **`README.md` is restructured around evaluating and installing the project**,
  which it previously answered on line 127 and only for Python (#276): registry
  badges, an **Install** table (`cargo install tf_tree` installs no command), a
  seven-row **Look elsewhere when** table, and a **Rust** worked example — now
  **compiled**, by a `#[cfg(doctest)]` `include_str!` in `tf_tree_cli`, whose
  module records what that gate does **not** reach.

### Changed — breaking

- **`Iso3` is 56 bytes at `align(8)`, not a padded 64-byte cacheline**
  ([`0042`](docs/decisions/0042-the-cacheline-the-arena-never-asked-for.md)). Its
  `_pad` field is gone and `#[repr(C, align(64))]` is now `#[repr(C)]`. Anything
  matching on the struct's fields or relying on `size_of::<Iso3>() == 64` will
  notice; nothing in the arena does, because nothing in the arena ever held one.

  **The public surface widens with it**: `_pad` was private and was the only
  thing preventing `Iso3 { q, t }` and exhaustive destructuring from another
  crate. Both are supported now, which makes `Iso3` consistent with `Vec3` and
  `Quat` — plain `repr(C)` structs with public fields — and makes a future added
  field breaking for a second reason. Accepted deliberately; `0042` carries the
  argument for not reaching for `#[non_exhaustive]`.

  The alignment existed *"so the Phase 2 shared-memory arena can store slots
  without re-deriving layout"*. The arena re-derived it anyway — `PoseSlot` is
  its own `align(64)` of atomics, which it has to be for the seqlock to be sound
  — and an `Iso3` reaches it through `to_bits`/`from_bits`. So the alignment
  bought the arena nothing and cost every in-memory use:

  | | before | after |
  |---|---|---|
  | `Iso3` | 64 | **56** |
  | `Step` | 128 | **64** |
  | `Plan` | 4160 | **2064** |
  | plan cache, per thread | 66.0 KiB | **32.6 KiB** |
  | `(i64, Iso3)` | 128 | **64** |

  No `FORMAT_VERSION` bump, no `layout_hash` change, no C ABI change. **This is
  a footprint change and not a latency one** — `fast-path.md` §15 already
  measured the alignment's effect on the fold at zero, `just bench-check` holds,
  and the control-loop reading did not regress. What it buys is that a
  perception node with eight threads holds 261 KiB of plan cache where it held
  528.

- **`tf_tree_core::edge::ClaimRecord::last_push_nanos` is now
  `clock_offset_nanos`, and holds `wall clock - stamp` rather than the wall
  clock** (#273, [`0036`](docs/decisions/0036-the-receipt-time-the-format-already-reserved.md)).
  It shipped since `0.0.1` *unwritten*, so nothing could have read it for its
  value, but a field path or a `ClaimRecord { .. }` literal naming it will not
  compile. No arena byte moves.

- **`OpenOutcome::TookOver` and `tf_tree::OpenError::TakeoverUnsupported` are
  deleted** ([`0037`](docs/decisions/0037-a-takeover-is-not-a-second-open.md)
  question 3, answered `no`). A takeover is not an outcome of `open()` — it is a
  method on the session that already holds the byte — so neither the variant nor
  the arm that refused it had anything left to describe. A `match` on
  `OpenOutcome` that named `TookOver` will not compile; nothing else moves.

- **`tf_tree_ipc::Open::already_attached`, the takeover arm it reached, and
  `Open::register_any` are deleted**
  (#275, closes #201, [`0037`](docs/decisions/0037-a-takeover-is-not-a-second-open.md)).
  `LockFile::take_any_participant` survives with no production caller; `IpcError`
  and `OpenOutcome` are unchanged as types; and `tf_tree`'s `TookOver` arm keeps
  the `OpenError::TakeoverUnsupported` refusal it has carried since #229
  (`0028` step 9), now unreachable and kept deliberately rather than made
  `unreachable!()` — it is what stands between an heir and a forked tree the day
  §3.5 gives the variant a producer (`crates/tf_tree/src/open.rs`).
  The arm handed back the first **free**
  participant byte while the caller's arena record was elsewhere, and could not be
  repaired in place — `0037` carries the five executed unsound states, the
  `F_OFD_GETLK` root cause, and the shape a real §3.5 would take.

  **Ownership migration (§3.5) was therefore not implemented, and had no path at
  all — until the entry below.** `0037` moved from `draft` to `implemented` in
  the same release: the deletion recorded here is half of the change, and
  `Session::take_over_ownership` is the other half. What is written above about
  the deleted arm stands; what no longer holds is that `TakeoverUnsupported` is
  "what stands between an heir and a forked tree", because the heir that shipped
  keeps its existing mapping and never constructs an arena at all.

- **The topology lock's error surface changes shape** (#213, `0029`).
  `ReparentError::LockContended`'s `owner_slot` is `Option<u32>`, not `u32`, so
  it stops passing `tf_tree_core`'s `u32::MAX` sentinel to the operator's message
  (`docs/API.md` R5); `ReparentError` gains `TopologyLease { raw_os_error: i32 }`;
  and `tf_tree_ipc::LockRole` gains `Topology`, which breaks a downstream
  exhaustive `match`, that enum not being `#[non_exhaustive]` (`docs/API.md` §7).

- **Both depth bounds move, and one changes meaning** (#251,
  [`0034`](docs/decisions/0034-the-depth-bound-priced-two-slots-the-same.md)).
  `tf_tree_core::MAX_DEPTH` is 32, not 16, and bounds the *compiled* plan; the
  new `MAX_PATH_EDGES` = 64 bounds the raw walk. Both are `pub const` and
  re-exported by `tf_tree`: a change to a value **and** to a meaning, on five
  published crates. `LookupError::TreeTooDeep { depth }` reports one quantity per
  bound, with no new variant and no new `tft_status`.

- **`tf_tree_ipc::self_comm` returns `[u8; 16]`, not `[u8; 32]`, and
  `tf_tree_ipc::Identity` gains `pid_ns_inode: u64` while `name` narrows to
  `[u8; 16]`** (#239, [`0033`](docs/decisions/0033-the-identity-record-cannot-name-a-namespace.md)).
  A public break on a **publishing** crate; the on-disk record did not grow or
  move, and the 88-byte handshake is unaffected (`docs/PHASE2.md` §3.7).

- **`IpcError::ArenaHeldButUnreachable` gains an `ownership_held: bool`, and its
  message now tells an operator which of three states they are in** (#257).
  Breaking: `IpcError` is not `#[non_exhaustive]`. With it, a false sentence
  about `--force-new` — that the arena the hatch exists for has *dead*
  participants, where a wedge **requires** a live holder — is corrected in the
  four live places it was copied to, the entry below included, and `RUNBOOK.md`
  stops promising an error that path no longer returns. `docs/PHASE2.md` §0.0's
  `--force-new` row carries the rule.

### Added

- **Python can declare a real topology** ([`0041`](docs/decisions/0041-python-declares-a-topology-the-way-everything-else-does.md)).
  `build`'s `edges` and `open`'s `create` each now accept **either** the existing
  list of `(parent, child)` pairs **or** the text of a topology config — the same
  schema `ros/tf_tree_ros` starts a bridge from and `tf_tree topology --discover`
  writes.

  The list form makes every edge dynamic under one global capacity, so a
  Python-built tree could not express a **static** edge — a sensor mount became a
  ring somebody had to publish into forever, which is the latched-topic
  behaviour `docs/PROJECT.md` §2 lists among the `tf2` problems this engine
  exists to solve — nor a per-edge size, nor a declared rate (the only evidence
  `TFT007` has that an observed rate is *wrong* rather than merely what it is),
  nor a per-edge domain.

  **A Python builder mirroring `TreeBuilder` was the expected answer and is not
  the one taken.** It would have been the third spelling of one declaration
  surface — the smell `PROJECT.md` §6 names — and would have drifted from the
  schema the CLI emits, so a discovered config would have been unusable from the
  one language most likely to want it. `capacity=` and `interp=` are refused
  beside a config, since it carries both. The wheel gains a path dependency on
  `tf_tree_bridge`, whose only dependency is `tf_tree` and whose parser is
  hand-written — no third-party and nothing Linux-only enters it.

- **`Plan.at_extrapolating` gains `layout=` and an `_into` form** in Python,
  closing the R2 violation the binding half of
  [`0039`](docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md) left.
  R2 is NORMATIVE that every batch entry point has an `_into` form and justifies
  it with this exact caller — the allocation is "half the call at n = 64, and
  n = 64 is the control loop" — so extrapolation, the method a controller reaches
  for, was the one path the rule was written about and the one that did not obey
  it. A `quat_twist` layout is refused as it is in C, and for the same reason.

- **Both bindings reach the time domain and extrapolation** — the halves
  [`0038`](docs/decisions/0038-the-domain-a-binding-cannot-name.md) and
  [`0039`](docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md) left
  unwired, and the ones that matter most for a robot node, since C++ is the
  language a control loop is written in.

  **C ABI** (frozen at 1.0, so both are appended and nothing moves;
  `TFT_ABI_VERSION_MINOR` 5 → 7): `tft_plan_create_in_domain` puts the query
  domain on the plan handle, validated at creation where the frame names are
  still in hand; `tft_plan_at_extrapolating` takes a policy and a **required**
  `tft_extrapolated *info` out-parameter — a null `info` is `TFT_ERR_NULL_ARG`
  and nothing is written, because C cannot express "no pose-only accessor" any
  other way. `edge` is `TFT_INVALID_ID` when `by_ns == 0`, deliberately sharper
  than the Rust field it mirrors. The C++ wrapper returns the pose and the
  distance as one object, which C can only ask for.

  **Python**: `Tree.plan(target, source, domain=0)`,
  `Tree.lookup(..., domain=)`, the four domain tags as module-level ints, and
  `plan.at_extrapolating(stamps, policy, /) -> (poses, by_ns)` with `policy`
  required. **Batch `by_ns` is an `(N,)` array, not a scalar**: a batch
  straddling the newest sample holds interpolated and extrapolated elements
  together, and collapsing it would mark one or the other wrongly.

  `docs/API.md` §4.1 records both shapes and why each took the form it did; §3.3
  records the two parity gaps still open — Python's `at_extrapolating` has no
  `layout=` or `_into` form, and Python still cannot declare a static edge, a
  per-edge capacity, a rate or a domain.

- **D5's owed measurement, taken at last** — `just interp-accuracy`
  (`crates/tf_tree_bench/examples/interp_accuracy.rs`). D5 ends *"do not make it
  the default without a measurement justifying it"*, which was read as a rule
  about *changing* the default and is also a standing obligation on the default
  that shipped: `ScLerp` costs more than `LerpSlerp` and nothing had priced what
  it buys. It buys position and only position — both policies SLERP the rotation,
  measured at ≤0.06 µrad, which is `f64` noise. The rest is chord-vs-arc: lever
  arm × θ²/8. At 1 kHz that is 0.001 mm; at 10 Hz, 6.16 mm. The trade points one
  way, and `docs/PROJECT.md` §5 D5 now carries the table.

- **A control-loop example, which did not exist** —
  `crates/tf_tree/examples/control_loop.rs`, run by `just control-loop`. The
  pitch is "fast enough to sit inside a control loop" and the only worked example
  shipped was an offline dataloader, so a consumer evaluating the runtime path had
  nothing to copy. It is a 1 kHz controller against a 200 Hz estimate under a
  concurrent writer, showing plan-once, one guard per *cycle* covering both of its
  queries, `ExtrapPolicy::ConstantTwist` with `Extrapolated::by_ns` gated against a
  per-route budget, and `SlotContended` handled as data rather than logged as an
  error.

  It also gives `docs/API.md` §8 its first executable reading: that section stated
  a real-time envelope and then admitted only the allocation claim had an
  executor. The example reports p50/p99/p99.9/max and says in its own output why
  all three of an unpinned host, no RT scheduler, and two clock reads around a
  sub-microsecond operation inflate them. **It reports and does not gate** —
  `docs/PHASE1.md` §11.3 remains the pinned-hardware criterion.

  The lesson it exists to make visible: on a composed route, staleness is set by
  the *slowest edge*. Its two routes differ by the ratio of their estimator rates,
  which is why `Extrapolated::by_ns` names the worst edge rather than an average.

- **Every core error type now implements `Display` and `core::error::Error`**
  ([`0040`](docs/decisions/0040-the-error-that-cannot-be-returned.md)).
  `LookupError`, `PushError`, `ClaimError`, `FrameError` and `TopologyError`
  implemented neither, so none of them could be `?`-chained into
  `anyhow::Error` or even `Box<dyn Error>` — a consumer's first function could
  not be `-> anyhow::Result<_>`. This crate's own rustdoc cited that as the
  reason `0019` §2b's startup sequence is published as a `text` block rather
  than as compiling Rust.

  Nothing about the representation changes: `Display` writes into the caller's
  formatter, so the errors stay `Copy`, `String`-free and returnable from the
  wait-free read path (D11), and `core::error::Error` rather than
  `std::error::Error` keeps `tf_tree_core` `no_std`. The messages print
  **identifiers** — `edge 3`, not `odom -> base_link` — because resolving a name
  needs the arena; `Tree::describe` remains the layer that has one, and its
  fallback arm now delegates here instead of `Debug`-printing the five variants
  it cannot name. Message text is a diagnostic and not a compatibility promise
  (`docs/API.md` R5, which gains the three-layer table).

- **`Tree::inherit_ownership` and `Tree::owner_lost` — `docs/PHASE2.md` §3.5,
  which had never run** ([`0037`](docs/decisions/0037-a-takeover-is-not-a-second-open.md),
  now `implemented`). Kill an arena's owner and lookups keep being served, exactly
  as §3.5 promises — but until now **no new process could join**, for as long as
  any survivor lived. A joiner won the ownership byte, met §3.4's split-brain
  check against the survivors' held participant bytes, backed off, and timed out
  with `ArenaHeldButUnreachable`. That is what a supervised robot does every time
  it restarts one node.

  `tf_tree_ipc::Session::take_over_ownership` takes byte 0 on the file
  description the session **already holds**, so the slot, the participant byte
  and the arena record cannot move — the invariant is structural rather than
  checked, which is why this is a `Session` method and not a second `open()`.
  `tf_tree_ipc::peer_hung_up` and `Tree::owner_lost` are §3.5's trigger, which
  never existed: nothing watched the client socket, so no participant ever
  *reached* the takeover path even while one was implemented. The trigger is
  caller-driven by design — no background thread, no daemon
  ([`0019`](docs/decisions/0019-one-binary-and-topology-you-can-wait-for.md)) —
  so a fleet whose survivors never call it still ends up ownerless.

- **`Plan::at_extrapolating` and `Extrapolated`** ([`0039`](docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)).
  `ExtrapPolicy::{Hold, ConstantTwist}` were implemented, tested and reachable
  from nothing: all five fold sites passed the `Error` literal and the facade did
  not re-export the type. Extrapolation is now selected **per query** — a `Hold`
  right for a 10 Hz map edge is wrong for the 1 kHz edge beside it — and
  `Extrapolated` has no accessor yielding the pose alone, so how far past
  `latest_common` an answer reached is handed over with the answer. `Plan::at` is
  unchanged and still refuses. `ExtrapPolicy` is now re-exported from `tf_tree`.

- **A tagged sibling for every query shape**, and `Tree::lookup_tagged`
  ([`0038`](docs/decisions/0038-the-domain-a-binding-cannot-name.md)):
  `Plan::{at_tagged, at_with_derivatives_tagged, at_many_into_tagged,
  at_many_into_f32_tagged, at_adaptive_tagged}`. `Domain` is an **open trait**, so
  a foreign binding cannot name the type `at::<D>` needs and must carry the tag as
  data. The typed forms are now one-line delegations; behaviour is unchanged for
  every existing caller.

- **`docs/API.md` §8, the real-time envelope.** The pitch is "fast enough to sit
  inside a control loop" and nothing stated what that meant. §8 records what the
  query path does not do, the bound that makes it usable from `SCHED_FIFO`
  (`SEQ_RETRY_LIMIT` = 64, then `SlotContended`), that page residency is the
  residual and `mlockall` is the embedder's call — and, in §8.4, that only the
  allocation claim has an executor and that `PHASE4.md` §1's operational criterion
  is still open.

- **A per-publisher clock offset, from a field that has been in every shipped
  arena since it was declared, and a `TFT004` that reads it** (#272, #273, #274,
  [`0036`](docs/decisions/0036-the-receipt-time-the-format-already-reserved.md),
  closing that record). Nothing wrote `ClaimRecord::last_push_nanos` and nothing
  read it, which is why `TFT004` could detect nothing in any configuration.
  `EdgeWriter::push` now records `wall clock - stamp`, sampled rather than on
  every push, and a claim clears the offset it inherits. **It costs about
  +1.1 ns per push** — `0036` carries the measurements, the percentage and the
  sampling interval, `docs/PHASE1.md` §11.2 the cost per second of publishing.
  `TFT004` is the **second** check to leave `docs/PHASE5.md` §0.0's *"cannot
  detect anything in any configuration"* group — `TFT007` left it first, on §6's
  declared-rate amendment — so *"sixteen detect"* becomes seventeen;
  what it deliberately does not claim to detect, and the four skips, are `0036`
  question 3.

- **`tf_tree_ipc::LockFile::try_take_topology` / `release_topology`** (#213) —
  byte 1 of the lock file, A2's topology mutation lock, so §3.3's byte table
  gains a row and `bytes 1–15 reserved` becomes `bytes 2–15`. Deliberately **no
  `probe_topology`**: an unused `pub fn` is surface with no consumer.

- **`tf_tree_py` gains a `pure-hash` passthrough, and `just py-cross-check`
  compiles it** (#180). `pure-hash` (#243) removed blake3's target-C-toolchain
  requirement for the Rust crates but was never forwarded to the binding, so a
  cross build of the *wheel* died in blake3's build script. Six of the seven
  things #180's 2027 fallback needs now hold; the seventh needs a real runner.

- **`just artifact-versions` reads the five crates.io front pages, and `just
  msrv`'s prose arm now covers them too** (#238). Neither gate could see a claim
  written in prose, so four of the five publishable crates said `0.0.1` for three
  releases while all five stated an unchecked MSRV. The version rule is a
  three-component `v?X.Y.Z` outside an inline code span; the MSRV arm stays a
  *presence* test, which is written down next to the loop.

### Fixed

- **The `Guard`'s packed search cursor went permanently inert past 2^32 pushes**
  — 49.7 days of unbroken 1 kHz publishing on one edge. The cursor is the low 32
  bits of a logical index and `head` is monotone and never masked, so beyond that
  point every stored hint was smaller than `lo_logical` and the clamp pinned it to
  the **oldest** retained sample on every call, reverting the resumed gallop to a
  walk from the far end of the window — worse than the midpoint restart it exists
  to beat. Never a wrong answer, which is why nothing caught it.
  `sample::rebase_hint` lifts the truncated value back; the readable window is
  strictly narrower than 2^32, so the lift is exact rather than a heuristic, and
  below 2^32 it is the identity.

- **`TFT016` told an operator to raise a limit nothing in this codebase spends.**
  Its finding read `mlock() of the arena will fail`, and `tf_tree` calls `mlock`
  nowhere — `docs/PHASE2.md` §7.4's `LockPolicy` exists in no line of code (§0.0
  now carries the row). The check fires on the same condition; it now addresses
  the embedding application, which is the only place that can see the
  `RLIMIT_MEMLOCK` budget it would spend (`docs/API.md` §8.3).

- **`Tree::reparent` no longer steals A2's topology lock from a live mutator that
  `/proc` misreports** (#213,
  [`0029`](docs/decisions/0029-the-topology-lock-is-a-kernel-lock.md)). It takes
  an exclusive OFD lock on the lock file's byte 1 before touching the arena word
  and releases them in the other order, so a live holder is refused by the kernel
  before any inference runs. No arena byte changes, and the two added `fcntl`s sit
  on a call D3 puts off the query path.

- **A `Tree::lookup` that cannot be planned recompiled on every call, forever**
  (#259). `with_plan` propagated a *failed* compile with `?`, which returns
  before the store; the cache now holds the **result** of compiling a key,
  refusal included — **−49.6%** on a refused 60-edge chain. Why that is safe is
  on `crates/tf_tree/src/cache.rs`.

- **`Tree::plan` moved a `Plan`-sized array three times, none of it proportional
  to the path** (#264) — 12 352 bytes of `memcpy` per compile, all three copies
  confirmed by disassembly. It is now written once in place, by `Plan::identity`
  plus `fold_into`: **−55.2%** on a 6-step `Tree::plan` (`docs/PHASE1.md` §7.2).

- **`docs/decisions/README.md` carried an unresolved merge conflict on `main`,
  and all eighteen CI checks were green on it** (#266) — a `git rebase` on a
  dirty worktree whose autostash popped into a conflict *after* the rebase
  reported success. `just no-conflict-markers` is the new gate, first in
  `just lint`; the script says why it matches three markers and not four.

- **`doctor`'s `TFT014` no longer calls a healthy participant in another PID
  namespace a fork inheritor and tells the operator to stop it** (#239,
  [`0033`](docs/decisions/0033-the-identity-record-cannot-name-a-namespace.md)).
  A recorded pid is namespace-local while `/proc` is not, so `Identity` now
  carries the writer's own `/proc/self/ns/pid` inode — recorded at registration,
  for `0033`'s reason — and `doctor` compares it against its own, behind a second
  guard for what the first is blind to. It fixes `doctor` and only `doctor`: the
  three paths `docs/PHASE2.md` §0.0 calls corrupting read `ParticipantRecord`,
  which gains no discriminator.

- **Stopping and continuing an owner — Ctrl-Z then `fg`, or a `gdb -p` attach and
  detach — no longer strands its arena** (#260). `OwnerServer::serve`'s
  `epoll_wait` propagated `EINTR`, so `serve` returned `Err`, its `Drop` unlinked
  the published socket, and the process lived on holding participant byte 0 and
  the ownership byte — §3.4 then has no exit for anybody. The fix retries that one
  errno; `crates/tf_tree_ipc/src/server.rs` carries the `signal(7)` argument and
  which triggers are measured.

- **A creator now takes participant slot 0 atomically, so it cannot end up
  holding one integer while its arena record holds another** (#201,
  [`0035`](docs/decisions/0035-the-creators-slot-is-taken-not-found.md)). §3.4
  step 4's split-brain scan and step 5's slot acquire were two passes over the
  same bytes; it is now one `F_OFD_SETLK` on byte 0, and `--force-new` is
  deliberately not exempted. `docs/PHASE2.md` §0.0's participant-registry row
  carries the measurement and the reachability argument.

  > **Amended by #275.** This entry ended *"Not fixed: the takeover arm
  > (`Open::already_attached`) still reaches `register_any` and can still produce
  > the divergence"*, and that is no longer true: the arm, its builder and
  > `register_any` are deleted. It was right that `0028` question 3 had settled
  > the arm's correct slot and wrong about what it settled — §3.5 **cannot** be an
  > `Open::open` call at all, which
  > [`0037`](docs/decisions/0037-a-takeover-is-not-a-second-open.md) records.

- **A stamp far from the origin overflowed two arithmetic sites, and a release
  build answered wrongly rather than panicking** (#247). `plan::subdivide`'s
  segment width and `sample.rs`'s interpolation parameter both subtracted stamps
  signed, returning — as `Ok` — a two-knot straight line and a pose from outside
  the bracket. Every stamp difference in `sample.rs` now subtracts in `u64`
  through one `span_ns` helper, and `crates/tf_tree/tests/wide_stamps.rs` pins an
  all-static path's stamp-independence, true and untested there until now.

- **358 MiB of committed cargo build output is untracked, and a gate now makes
  the class unmergeable** (#246). Four `CARGO_TARGET_DIR` siblings merged across
  #237, #242 and #243, because `.gitignore`'s `/target/` is *anchored* and
  nothing in the pipeline looks at what is tracked. **No published artifact was
  affected**, verified against the registries, and history is left intact so every
  published SHA and the `v0.0.4` tag still resolve. `just no-build-output` rejects
  a cargo build-output signature whatever the directory is called.

- **`CLAUDE.md` and `ci.yml` both said `just lint` runs six clippy passes; it
  runs eight** — `pure-hash` (#243) added two and neither prose site was updated,
  including a comment that claimed to have counted rather than remembered.

---

## [0.0.4] — 2026-08-22 (the slot a killed participant keeps)

**One defect, present for the project's whole life, is the reason this release
exists.** A shared arena granted participant slots and took one back only when
the process holding it ran `Drop`. A `SIGKILL`ed writer does not, so sixty-four
abnormal read-write exits — over an arena's *whole life*, not sixty-four at once
— wedged it at `NoParticipantSlots` permanently. It hid because **a wedged arena
scores perfectly**: a ring outlives the process that filled it, so every composed
read still succeeds, off samples nobody is refreshing.

`docs/decisions/0028` is the record and this release is its whole plan. Three
collectors now reclaim a dead participant's slot — the owner's slot assigner on
the next grant that walks past it, the owner's socket-hangup callback, and
`Tree::reap_participants()` from any read-write participant, which is the only
one that can reach the owner's own slot. All three share **one** liveness
predicate and **one** `ParticipantTable::reclaim`, and that predicate is the OFD
lock byte — the only fact `docs/PHASE2.md` §5.1 permits to answer the question,
in as many words: *"Any code deciding liveness from `state` or `heartbeat` is a
bug."*

**Two things to read before upgrading.** A read-write attach over a bare file
descriptor is now *refused* — see *Changed — breaking*, which carries the port.
And `Open::await_open` returned immediately for any whole-second budget in
`0.0.3`; if you called it that way, you never waited.

### Added

- **`ParticipantTable::reclaim`** in `tf_tree_core` — free a participant slot
  whose process is gone, guarded by the state word the caller observed rather
  than by an incarnation a reaper has no way to hold. `ParticipantTable::release`
  stays the clean-detach path; this is the path for a slot whose process never
  ran `Drop`, and it also accepts `RESERVED` — the word a process killed inside
  the two-phase publication leaves behind — which `release` cannot name at all.
  `docs/decisions/0028`, plan step 1.

  **It is listed here because it is public surface, not because anything ships
  on it yet.** `tf_tree_core` is one of the five crates that publish, so a new
  `pub fn` is an API change on the `0.0.x` promise. The pieces that make #184's
  leaked slots actually get reclaimed — the liveness predicate, the owner's
  assign-time sweep, and `Tree::reap_participants()` — land on top of it.

  **The liveness verdict is not taken here.** `docs/PHASE2.md` §5.1 is normative
  that whether a participant is alive is a kernel fact; this is only the guarded
  store that acts on the decision, and it reads no `heartbeat`. A caller must
  observe the state word **before** it probes the OFD lock byte — the doc comment
  carries that obligation, and its loom model ships with two runnable controls
  that erase a live participant's record without it.

- **`Tree::reap_participants()`** — reclaim the participant records of processes
  the kernel says are gone, and return how many were collected. Sweeps the
  arena's participant table, asks the OFD lock byte about each slot that holds a
  record, and frees the ones whose byte the kernel has released.
  `docs/decisions/0028`, plan step 5.

  **It is not owner-only, and that is the point of it.** `docs/PHASE2.md` §6.3:
  *"reaping must not be owner-only — an owner-only design leaks every claim held
  at the moment the owner died."* The owner's socket-hangup reap cannot reach the
  owner's **own** slot — the owner registers itself and no socket of its own
  closes, so no hangup ever fires for it — which is why a `SIGKILL`ed owner used
  to leave a `LIVE` record over a released lock byte for the life of the segment.
  Any surviving read-write participant can now collect it.

  **Refused on a read-only tree**, returning `0` rather than reaping: reclaiming
  is a `compare_exchange` and a `PROT_READ` mapping answers one with `SIGSEGV`
  (`docs/API.md` R6, D18). `Tree::is_writable()` is how a caller tells that `0`
  from "there was nothing to collect". A tree that did not come from
  `tf_tree::open` reaps nothing either: liveness is a kernel fact about a lock
  byte (`docs/PHASE2.md` §5.1), and a heap tree has no lock file to ask.

  **It never collects its own slot**, and it does not decide from `state`: the
  word selects which slots are candidates, the kernel decides which of those are
  dead, and the word is observed *before* the byte is probed so the two cannot be
  read out of order.

  **This does not make reclamation automatic.** It runs when a participant calls
  it. The owner-side collectors that reclaim without being asked are the other
  two in this release — the slot assigner (step 3) and the socket-hangup
  callback (step 4) — and all three share this one predicate and one
  `ParticipantTable::reclaim`. The `fork` case is deliberately out of reach of
  every one of them: a forked child keeps the parent's open file description, so
  the kernel reports the byte held and they correctly decline to act.

### Changed — breaking

- **`Tree::attach_shared` and `Tree::attach_shared_at` now refuse
  `AttachMode::ReadWrite`**, with the new `ShmError::ReadWriteNeedsRendezvous`.
  A read-write attach over a bare file descriptor is a compile-time-fine,
  run-time-refused combination as of this release; `AttachMode::ReadOnly` is
  unchanged on both, and so is every path through `tf_tree::Open`.

  **A writer joins through `tf_tree::Open`.** The reason is not style. A
  read-write attach *registers a participant record* in the arena, and the
  rendezvous takes an OFD lock byte for that slot before writing the record — so
  on every rendezvous path the byte is a complete answer to "is this participant
  alive?". A descriptor carries no lock file, so a writer that arrived that way
  held a `LIVE` record with a permanently free byte: indistinguishable, by the
  byte alone, from a slot leaked by a `SIGKILL`ed process. That ambiguity is what
  stopped anything from reclaiming leaked slots, which is the wedge
  `docs/decisions/0028` exists to fix — 64 abnormal read-write exits over an
  arena's whole life wedge it at `NoParticipantSlots` permanently.

  **Both entry points, because closing one closes nothing.** `attach_shared_at`
  takes a slot number, but being *told* a slot index is not the same as having
  been granted one, and a `pub` function cannot tell the two apart.
  `tf_tree::Open`'s joiner now registers through a crate-private path whose
  precondition is the byte it is already holding.

  **Porting.** A consumer that read: attach `AttachMode::ReadOnly`, unchanged. A
  consumer that published over an inherited or `SCM_RIGHTS`-passed fd: build it
  on `tf_tree::Open` — the descriptor stops being the whole capability, and the
  process needs a runtime directory (`$TF_TREE_RUNTIME_DIR`,
  `$XDG_RUNTIME_DIR/tf_tree`, `/run/tf_tree`, or `/tmp/tf_tree-<uid>`) it shares
  with the arena's creator. The Python `mode="rw"`, the C ABI and the ROS 2
  bridge are all unaffected: none of them exposes an attach-from-descriptor
  surface, and Python's read-write mode already built a `tf_tree::Open`.

  This is a breaking change to the Rust facade, and on the `0.0.x` line that is
  what every release is permitted to be — see the note at the head of this file.
  It is a refusal rather than a deprecation because the shape being removed is
  unsound rather than merely discouraged: a deprecation warning would leave the
  slot-leak ambiguity in place for as long as anyone ignored it.

### Fixed

- **A `SIGKILL`ed read-write participant kept its arena slot for ever, and the
  owner's slot assigner decided liveness from a word it is normative not to**
  (issue #184). The assigner skipped any slot whose `ParticipantRecord::state`
  read `LIVE`, and `docs/PHASE2.md` §5.1 says that in as many words: *"Any code
  deciding liveness from `state` or `heartbeat` is a bug."* A process killed
  without running `Drop` never clears its record, so sixty-four abnormal
  read-write exits wedged an arena permanently — a budget a crash-looping node
  spends in minutes. The assigner now takes its verdict from the participant's
  OFD lock byte, through the single `reclamation_verdict` predicate, and
  **reclaims the dead record before granting the slot** — deciding without
  reclaiming would change nothing, because `fill_slot` fills only a `FREE` slot
  and the joiner would be refused the slot just judged collectable.
  `docs/decisions/0028` plan step 3.

  **The owner's hangup callback is rebased onto the same operation** (plan
  step 4): it loads the `state` word once and hands it to
  `ParticipantTable::reclaim` rather than reading an incarnation out of
  `identity()` and calling `release`. For a live word the guard is identical —
  `live_word` packs the incarnation into the word — and the callback now also
  collects `RESERVED`, which `identity()` reports as `None` and `release` could
  therefore never name: a process killed inside the two-phase publication used
  to lose its slot to everybody, for ever. **That widening is the headline of
  these two steps, so it is pinned rather than asserted**: one test per
  collector stages the `RESERVED` word — `0028` open question 4 measured that
  publication window at ~12 ns, so it is staged rather than raced, and the tests
  say so — and narrowing either caller back to `LIVE` fails exactly one of them,
  while both fail at the commit before this change.

  **No format change and nothing on the hot path.** `FORMAT_VERSION` and
  `layout_hash` are untouched, `Plan::at` is not involved, and the cost is at
  most one `F_OFD_GETLK` per slot the assigner would otherwise have skipped, on
  a handshake already measured at 97.5 µs p50. No public API changes.

  **What is still not reclaimed**, so that a green run is not read as more than
  it is: a slot whose lock byte is held by a `fork`ed child that inherited the
  descriptor is **deliberately** left alone — the kernel says the byte is held,
  and §6.2 says that is the truth (`docs/decisions/0030`). A full sweep of every
  slot, rather than of the slots one grant walks past, is
  `Tree::reap_participants()` above — the third collector, added in this same
  release, and the only one that can reach the owner's own slot.

- **A process's participant lock byte and its arena participant record could
  carry different indices** (issue #201), which every liveness predicate in the
  engine assumes cannot happen. `Open::attempt` now compares the two where they
  are paired — the single `hold_ownership` call site, the one place in the
  workspace that can see both, since `tf_tree_ipc` chooses the byte and has no
  arena dependency — and refuses with the new
  `OpenError::ParticipantSlotDiverged` instead of returning a `Tree` whose every
  liveness answer is about another process. `docs/decisions/0028` plan step 0c.

  **`CreatePolicy::Always` is the only policy that can reach it**, because §3.4
  step 4 refuses to create while any participant byte is held and the escape
  hatch skips that check by design. The state it lands on is a live *non-owner*
  holding byte 0, which `tf_tree_ipc::Session::release_ownership` produces from
  a documented §3.5 call — the route `0.0.3`'s *Known issues* entry said was not
  known. A refusal costs the caller nothing: the check runs before the owner
  server binds, so no peer ever saw the arena, and both the participant and
  ownership bytes are released with the session. Stop the process still holding
  the byte, or open with `CreatePolicy::IfAbsent`.

  **New public API** on the `0.0.x` promise: `OpenError` gains a variant. It is
  `#[non_exhaustive]`, so a caller matching with a wildcard arm is unaffected;
  both bindings forward it by `Display` and needed no change.

- **`Open::await_open` failed immediately for any whole-second budget.** It
  derives each attempt's socket timeout by subtracting elapsed time from the
  caller's budget, so `await_open(Duration::from_secs(1))` reached the handshake
  as `999.999_9xx ms`. Converting that to `SO_RCVTIMEO_NEW`'s
  `(tv_sec, tv_usec)` rounds the sub-microsecond tail up without carrying into
  `tv_sec`, producing `tv_usec == 1_000_000`, which the kernel rejects with
  `EDOM` — surfaced as `IpcError::ClientSocketSetup`, which is classified
  **terminal**, so the call returned in microseconds with a local-resource error
  rather than waiting out its budget. The per-attempt timeout is now truncated to
  whole microseconds, the ceiling counterpart of the `MIN_BACKOFF` floor that was
  already there for the `EINVAL`-on-zero case at the other end. Every existing
  test used a sub-second budget, whose remainder never reaches the last
  microsecond of a second, which is why all fifteen rendezvous tests were blind
  to it.

### Known issues

These are the two things `docs/decisions/0028` does **not** fix, plus one of the
same shape one layer up. Each has a home; none is a surprise waiting to be found.

- **A `fork`ed child keeps its parent's participant slot alive, and the kernel
  agrees with it.** The client socket is `CLOEXEC` and `fork` does not `exec`, so
  the child keeps the connection's open file description and the owner never sees
  `HUP`; and by `docs/PHASE2.md` §6.2 the child holds the participant **lock
  byte** by the same mechanism. So the byte — which this release makes the whole
  liveness predicate — answers *alive* for a process that provably cannot
  participate: no mapping, poisoned `Tree`. None of the three collectors touches
  it, deliberately, because reclaiming a slot the kernel calls held is the
  corrupting direction. Reclamation waits for the last inheritor to exit, which
  for a `multiprocessing` worker pool is the pool's lifetime. `tf_tree doctor`
  reports this case under `TFT014` with its own message, because the operator
  response is the opposite of a free byte's. Whether a child-side
  `pthread_atfork` handler may close the inherited descriptors is
  `docs/decisions/0030`, which is `draft` — and whose first open question can
  close it as *rejected*, making this a permanent documented limitation rather
  than an open hole.

- **An arena whose owner has died cannot be rejoined.** `docs/PHASE2.md` §3.5
  takeover is unwired, and as of this release the arm that would have reached it
  refuses with the new `OpenError::TakeoverUnsupported` instead of doing the
  wrong thing quietly: it `memfd_create`d a *fresh* segment, so a "taker-over"
  got an empty arena with the same name and none of the state it was inheriting.
  Recovery is unchanged and is not a signal — every mapping has to go so the
  segment is freed. `docs/RUNBOOK.md` carries the procedure where an operator
  will look for it.

- **`Tree::reap_participants` will free the record of a live process that
  created its arena with `TreeBuilder::build_shared`.** That call registers a
  `LIVE` participant record and takes **no lock byte**, because such an arena has
  no lock file at all — the fd is the capability. Every collector in this release
  keys on the byte, so every one of them reads that record as dead. Measured: a
  `build_shared` creator served through `tf_tree_ipc::OwnerServer` is reported
  `alive false` by an ordinary joiner, and the sweep CASes its record to `FREE`
  while it is still publishing.

  **Who is affected is narrow, and worth stating precisely.** It needs a
  *rendezvous* over a `build_shared` arena — that is, `OwnerServer::bind_at` and
  `serve` called by hand — because a peer only forms an opinion about a record if
  it joined and took a probe. An arena created through `tf_tree::Open` is
  unaffected (its creator holds byte 0), and so is the ordinary `build_shared`
  deployment that passes `Tree::shared_fd` to children and stands up no
  rendezvous: no peer there carries a probe. Nothing in this workspace composes
  it the affected way. **Until `docs/decisions/0031` is answered, do not call
  `Tree::reap_participants` in a process tree where anything served a
  `build_shared` arena by hand.**

- **`Tree::reparent` decides topology-lock liveness from `/proc` even when the
  tree holds an OFD probe** (issue #213) — the same §5.1 shape this release fixed
  for participant slots, one layer up, on the one path that takes A2's topology
  lock. `docs/decisions/0029` is the record. **It was `draft` when this entry
  was written and is `implemented` as of #269** — re-scoped to *the topology lock
  is a kernel lock*, and the fix is not the one this entry's phrasing implies:
  `reparent` does not consult the probe, it takes a kernel lock so that no
  inference decides the steal.

## [0.0.3] — 2026-08-19 (first with a source distribution)

**This release exists partly to prove one thing.** `0.0.2` has eleven wheels and
no sdist, because PyPI accepted the wheels and then refused the twelfth file with
`400 License-File LICENSE-APACHE does not exist in distribution` — a partial
upload no registry lets you re-cut. The metadata declared three licence files the
tarball did not carry, and `pyproject.toml`'s `[tool.maturin] include` closed
that. The gate that keeps it closed lives in the `sdist` job, before `publish`,
so a build that cannot be uploaded fails where it was built rather than after
eleven wheels have gone to the index. **This is the first release where that runs
in CI rather than locally** (issue #179).

### Known issues

- **A process's lock byte and its arena participant record can carry different
  indices** (issue #201), which every liveness predicate in the engine assumes
  cannot happen. Reproduced and pinned by
  `defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing`: with a
  live holder of participant byte 0 and a `CreatePolicy::Always` creator at arena
  record 0, `Tree::participant_alive(0)` answers about the *holder's* byte, and
  the verdict flips from alive to dead when only that holder leaves — about a
  process that never stopped publishing.

  **How that state arises is not known.** The escape-hatch route is measurably
  not it: an owner plus survivors holds bytes `[0, 1, 2]`, killing the owner
  leaves `[1, 2]`, and the forced creator then takes byte 0 against record 0,
  which agree. Two attempts to construct a live non-owner holder of byte 0
  through `tf_tree::Open` alone failed — a failed construction, not an
  unreachability argument. Nothing acts on the verdict destructively today, so
  the harm is conditional on a future rescuer; `docs/decisions/0028` question 3
  is where it is settled.

- **`Tree::reparent` decides topology-lock liveness from `/proc` even on a tree
  holding an OFD probe** (issue #213), and it is the only topology-lock path in
  the facade. Recorded in `docs/PHASE2.md` §0.0 as the third place the identity
  triple is still correctness-critical.


### Fixed

- **`Tree::lookup`'s per-thread plan cache could serve one tree's compiled plan
  to another** (issue #196). The cache is `thread_local!` and shared by every
  `Tree` on the thread, but its key was `(target, source, generation)` — and all
  three agree across trees as a matter of course, because `FrameId`s are handed
  out in interning order and a built tree's generation is its declared edge
  count. Two trees built from the same names in the same order therefore
  collided, and so did a tree rebuilt after its predecessor was dropped. The key
  now carries an arena identity: a shared segment uses its existing
  `instance_uuid`, so two handles onto one segment still share plans, and every
  other backing takes a process-local counter. **No arena field was added** and
  the per-thread cache's footprint is unchanged — the extra `u64` fits in the
  entry's existing padding, measured at 2176 bytes per entry either way.

  The failure was a wrong number, never a bad read: a stolen plan naming an edge
  index the other arena does not have is refused by `ArenaView::edge`'s bounds
  check with `LookupError::UnknownEdge`.

  **`tf_tree_py` inherited this and is fixed by the same change** — its `lookup`
  calls the Rust facade and keeps no cache of its own, so any Python process
  holding two `tf_tree.Tree` objects on one thread was exposed. The C ABI never
  was: it exposes no collapsed lookup, only `tft_plan_create` and `tft_plan_at`.

- **The liveness predicate could report a running process dead** (issue #194).
  Its own doc comment states the bias it is built on — every ambiguity resolves
  to *alive*, because a false "dead" lets a rescuer take an entry from a running
  process, which is corruption, while a false "alive" only delays recovery — and
  two of its branches produced the other direction.

  A `/proc` that is not mounted fails every open with `ENOENT`, the same errno a
  genuinely dead pid produces, and that was read as proof of death: on such a
  host every participant read dead at once. The classification now depends on a
  one-shot probe of `/proc/self`, because a process that cannot see its own entry
  learns nothing from the absence of anybody else's. The `ENOENT` that remains
  ambiguous is `hidepid=2` hiding another user's process, which `docs/PHASE2.md`
  §3.10's same-user participants rule out — a dependency the code now names.

  Separately, a registrant that could not read its own start time stored `0`, and
  `0` compares unequal to every real start time, so the first reader that *could*
  read `/proc` declared that registrant dead while it was running. A stored `0`
  is now read as *unknown*, and `process_start_time` returns an `Option` so the
  sentinel is written deliberately rather than returned as though it were a
  measurement.

  **No arena field was added.** The record's `start_time` is unchanged and `0`
  was already what a fresh arena's participant region holds, so this is a
  read-side reinterpretation: `FORMAT_VERSION` and `layout_hash` are untouched.
- **`RUNBOOK.md`'s recovery from `ArenaHeldButUnreachable` told an operator to
  use a flag that does not exist** (issue #189). `--force-new` is `PHASE2.md`
  §3.4's name for the escape hatch, and the escape hatch shipped as
  `CreatePolicy::Always` on `tf_tree::Open` — a policy on whoever creates the
  arena, never a command-line flag. `tf_tree_cli` cannot usefully carry one:
  it supplies no `layout_if_creating`, so the create path it would reach ends in
  `OpenError::NoLayoutToCreate`, and it exits, so the arena would not outlive the
  command. The runbook now names the policy and shows the call; `PHASE2.md` §0.0
  carries the status row the prose is read against.

  The same entry described the wrong cause. It blamed a stopped or wedged
  participant; on this build the ordinary cause is an owner that exited while a
  *healthy* survivor kept the arena mapped, because §3.5's takeover has no
  trigger — and the wedge then lasts as long as any survivor does. The recovery
  is to stop every participant, which the entry now says.

- **`PHASE2.md` §5.1 said the identity triple is off the correctness path while
  three paths decided liveness from it** (issue #205). All three are in
  `crates/tf_tree/src/tree.rs`: the OFD probe's fallback, which hands the
  question to `record_is_alive` whenever `F_OFD_GETLK` declines to answer;
  `liveness_for`, which *is* the whole predicate for any tree that never got a
  probe — a heap tree, or one from `Tree::attach_shared`; and `Tree::reparent`,
  which steals A2's topology lock on `participant_is_alive` and never consults
  the probe even when the tree has one. The third was found verifying the first
  two.

  Recorded as a §0.0 status row, not as surgery on §5.1's NORMATIVE prose, on
  the same ground as #189's row: §0.0 outranks prose, and §5.1's wording is
  `0028`'s claimed ground. The row also names §3.10 as the reason a `hidepid=2`
  `ENOENT` is survivable — a dependency #204 put in a code comment and nowhere
  in the spec, which was #194's own complaint one layer down. **No code
  changed**: moving those paths off the triple is a design decision and belongs
  in a record.

- **`tests/frozen.rs`'s litter check failed about files it had not produced.**
  It scanned the shared `std::env::temp_dir()` for any entry whose name
  contained this process's id as a substring, so an unrelated process's scratch
  directories tripped it — roughly two runs in five, making `just shm-check`
  intermittently red. It now matches the `.{stem}.tmp.` prefix `freeze_to`
  actually writes.

### Added

- **`tf_tree_math::slerp` is public**, the shortest-arc quaternion
  interpolation `LerpSlerp` evaluates. It was reachable only by building two
  `Iso3` with throwaway zero translations, which the optimizer does not fold
  away: both arms end in the *same* out-of-line `slerp`, and what the `Iso3`
  one puts in front of it is 256 bytes of stack, two 64-byte isometries written
  out field by field, and a lerp of one zero translation into another. Measured
  as exported `extern "C"` arms at `opt-level = 3` on x86-64, that prologue is
  45 instructions bare (7 against 52) and 28 through the consumer's own
  `nalgebra` adapter (41 against 69). `ScLerp`'s
  kernel `dualquat::screw_pow` has been public since Phase 1; this removes the
  asymmetry rather than opening a new surface.

  **Two behavioural differences from the `Iso3` round trip, at the endpoints
  only** — both because `LerpSlerp::eval` answers `s = 0` and `s = 1` from a
  shortcut that never reaches the kernel:

  - At `s = 1` with `qa·qb < 0` the bare function returns `-qb` where `eval`
    answers `qb`. Same rotation, opposite components. Callers comparing
    quaternion components rather than rotations at `s = 1` will see it.
  - Inside the `1e-6`-rad LERP fallback band the bare function *renormalizes*
    at both endpoints, where `eval`'s shortcut returns its input exactly — a
    departure of about `2.7e-16` for any input whose components do not happen
    to square to exactly `1.0`.

  (The count was **one** here when this entry first landed, while `slerp`'s
  rustdoc, `docs/API.md` §6 row 16 and
  `endpoints_lose_bit_exactness_only_in_the_lerp_fallback` all said two. A third
  difference exists and is not counted with these because no rotation can reach
  it: a `-0.0` component comes back `+0.0` from the kernel and survives `eval`'s
  shortcut. It needs a hand-built `Quat`; `slerp`'s *Endpoints and degenerate
  inputs* section states it.)

- **`tf_tree` re-exports `slerp`.** The commit above made
  `tf_tree_math::slerp` public for a consumer of the *engine*, and the facade
  did not carry it — so that consumer still had to add `tf_tree_math` as a
  second direct dependency and pin it in lockstep with `tf_tree` on a line
  where every release breaks every other, which is worse than the `Iso3` round
  trip they were told to abandon. `tf_tree::slerp` is now the same item as
  `tf_tree_math::slerp`, and **`tf_tree::dualquat`** carries `ScLerp`'s kernel
  beside it — the first revision re-exported only `LerpSlerp`'s and so
  reproduced the same asymmetry one layer up, leaving an `ScLerp` consumer in
  exactly the two-dependency position this closes. The module is re-exported
  rather than the function, so `tf_tree::dualquat::screw_pow` is the same *path*
  as `tf_tree_math::dualquat::screw_pow` and not a second spelling of it
  (`PROJECT.md` §6). On the facade's **stable** tier (no `unstable`
  feature), checked by `tf_tree/tests/math_reexports.rs`, which compiles only
  if the two paths resolve to one function. `ScLerp`'s kernel is deliberately
  not re-exported beside it: `screw_pow` is reached through
  `tf_tree_math::dualquat`, and a bare name at the facade root would be a
  second spelling of that path rather than the same one.

- **`just artifact-versions` checks every Markdown table's rows against its
  header.** `docs/API.md` row 16 rendered with a phantom cell from #198 to #209 —
  and the commit that claimed to fix it escaped four of six pipes — while
  `PHASE2.md` §12.2 hid two rows' measured results entirely, because GFM discards
  every cell past the header count. Nobody proofreads what they cannot see. The
  check is escape-aware, skips fenced code, HTML comments and indented code, and
  is verified in both directions: a ragged row fails with its location, and the
  three constructions GFM does *not* render as a table stay silent.
- `just shm-check` runs `cargo nextest run -p tf_tree --features shm --lib`. The
  recipe named integration targets individually and had no `--lib` line, so an
  `shm`-gated unit test in the facade was compiled by clippy and executed by
  nothing.

- `just py-compile` gates the workspace-excluded `tf_tree_py` on a clean
  checkout. It skipped whenever there was no `.venv` — which is every CI runner
  and every first clone — so the dependency `just lint` carries covered nothing
  in the one configuration CI runs. It now falls back to the interpreter on
  `PATH` (PyO3 executes a Python to configure itself; it includes no header, and
  the fallback compiles on a host with no `Python.h`), prefers `.venv` where
  there is one, and skips only where there is no Python at all. It also carries
  `cargo fmt --check` for that crate, which `cargo fmt --all` does not reach
  because the crate is not a workspace member — and `just fmt` gained the
  matching write-mode line, so the formatting `just lint` now enforces has a
  recipe that fixes it. `ci.yml`'s `bindings` job invokes the recipe instead of
  re-spelling its two lines.
- **`just gate4-python` re-derives PHASE5 §12 gate 4's Python arm**, which had
  been a recorded 1.785× that no recipe regenerated — the shape
  `docs/benchmarks/EVIDENCE.md` exists to catch. `frozen_workers` gained
  `--python <interpreter>`, and `crates/tf_tree_bench/python/gate4_worker.py` is
  the worker it drives: same fixture, same stamp grid, same barrier, so the two
  arms differ in the worker's language and nothing else. It **reports** — the
  criterion is stated over the Rust worker and the recipe exits 0 on the FAIL it
  prints. Both arms now name their worker in the verdict line, so a pasted
  transcript carries the qualification the amendment asks for.
- **`tf_tree doctor`'s `TFT014` reports the participant-slot leak its catalogue
  row is named for** (issue #190, `docs/decisions/0028` plan step 6). A
  participant killed without running `Drop` leaves a `LIVE` record over a free
  lock byte; the owner's slot assigner skips such a record, and 64 of them wedge
  the arena at `NoParticipantSlots`. `doctor` now names each one, with the pid
  and how much of the fixed budget is spent. Severity stays **warn**, and
  **nothing is reclaimed** — `0028` is a draft record that exists so no
  reclamation lands before its predicate is settled.

  **It is not the shape #184 measured, and the finding says so.** #191 gave the
  owner a socket-hangup reap (`0028`'s *candidate B*), so a rendezvous joiner
  killed under a running owner has its record released — measured with an
  owner, a read-write joiner and a third observing process: with the joiner
  `SIGKILL`ed, its `state` word had gone `0x6` (`LIVE`) to `0x0` (`FREE`) by the
  observer's first poll 50 ms later. Seeing this finding means the slot was one
  that reap cannot reach, and the message names the five: the owner's own slot,
  a client its `epoll` never watched, an `attach_shared` participant, a
  takeover, and an owner that died inside the callback. Two leave the owner
  dead, so `doctor --attach` cannot be pointed at them at all — the check's doc
  comment and `PHASE5.md` §6's amendment both say which is which.

  **The claim half was blind in that same state and is fixed with it.** It fired
  on `owner_pid == 0`, resolved through `ParticipantTable::identity`, which
  answers for any record whose `state` word reads `LIVE` — which a `SIGKILL`ed
  writer's does until something clears it. `PHASE2.md` §5.1 forbids deciding
  liveness from `state` in as many words. A `Snapshot` now carries the
  participant table with `Tree::participant_alive` already applied — the
  kernel's `F_OFD_GETLK` on the slot's lock byte — and both halves read that one
  answer.

  Three conditions in the title stay undetected, and `PHASE5.md` §6's amendment
  says why each is a gap in the available evidence rather than in the check: a
  `RESERVED` record, which the predicate cannot tell from a healthy joiner
  mid-attach; the fork case, where the kernel truthfully reports the inherited
  byte as held; and a claim whose slot has since been re-granted, where the
  claim word carries the `ClaimRecord`'s own epoch rather than the participant's
  incarnation and so joins to whoever holds the slot now.

  **`TFT014` now skips on `doctor --from-file`, where it reported `pass`.** A
  frozen `.tft` is a byte copy of the whole arena, participant records
  included, so every slot in one names a process that exited when the freeze
  finished — and a file has no assigner for a leaked slot to wedge. A `--json`
  consumer reading `TFT014` on a `.tft` sees `skipped` with a reason where it
  saw `pass`; the `pass` was an all-clear about a question the file cannot be
  asked, and it held only because the old predicate was reading `state`.

## [0.0.2] — 2026-08-17 (wheels, no sdist)

### Fixed

- **The Python binding did not compile on macOS or Windows.** `tf_tree_py`
  imported `OpenError` and `AttachMode` unconditionally and named
  `Open`/`CreatePolicy` in `open_arena`, all of which the facade gates on
  `#[cfg(all(feature = "shm", target_os = "linux"))]`. Since the binding always
  enables `shm`, it is the *target* that decides, and four `wheels.yml` rows —
  both Windows, both macOS — failed with `E0432`/`E0433`. `open_arena` now keeps
  a paired non-Linux arm that refuses with a message naming the platform, on the
  argument `offline.rs` already records for `.tft`: a missing attribute makes a
  portable script fail with `AttributeError` somewhere unrelated.

### Note on 0.0.2's PyPI upload

**0.0.2 published eleven wheels and no source distribution**, and the sdist for
it can never be added under that version — PyPI refused it and the version is
now partially published. `pip install transform_tree` works on every platform
the wheels cover; a platform they do not cover has no source fallback until
0.0.3.

The sdist declared `License-File: LICENSE-APACHE` (and `LICENSE-MIT`, `NOTICE`)
in its `PKG-INFO` and did not contain them. maturin auto-detects those files at
the project root for PEP 639 but its sdist selection follows the cargo package
rules, which never reach the workspace root — the tarball held only
`Cargo.toml`, `PKG-INFO`, `pyproject.toml` and `README.md`. PyPI validates the
pair and returns `400 License-File LICENSE-APACHE does not exist in
distribution`. The wheels were never affected; only the sdist selection is.

`[tool.maturin] include` now names the three files, and `twine check` passes.

### Note on 0.0.1

**0.0.1 is a crates.io-only release.** All five crates are published at that
version and stay published; no wheel exists for it and none can, because the
commit it was cut from is the one that did not compile off Linux. The bug was
invisible until the `v0.0.1` tag ran `wheels.yml` for the first time in the
project's life — no workflow had run here between 2026-07-23 and 2026-08-16, and
that one triggers only on a `v*` tag.

0.0.2 is the first release published from a tag to **both** registries, and the
first to use Trusted Publishing on either: crates.io by OIDC through
`release.yml`, PyPI through its pending publisher in `wheels.yml`. Neither uses
a stored token.

## [0.0.1] — 2026-08-17 (crates.io only)

First publish: the five publishable crates went to crates.io at 00:33 UTC on
2026-08-17. **No wheel** — PyPI refused `tf_tree` as too close to an existing
project, and the distribution was renamed to `transform_tree` for `0.0.2`, so
that is the first version installable with `pip`.

Everything below is *Added*, because there is no previous release for anything
to be changed, deprecated, removed or fixed relative to.

### Added — the engine (Phase 1, `docs/PHASE1.md`)

- A pointer-free, fixed-capacity, `#[repr(C)]` transform arena. No growth, no
  realloc, no `Arc`/`Box`/`Vec` inside an arena structure; `FrameId` and
  `EdgeId` are append-only and tombstoned rather than recycled.
- Lock-free reads: per-edge seqlock sample rings, one writer per edge.
- Compiled lookup plans — `Plan` is resolved once and evaluated many times, and
  evaluation allocates nothing, takes no lock and converts nothing.
- `tf_tree_math`: `no_std`, `#![forbid(unsafe_code)]` SE(3)/SO(3), quaternion and
  dual-quaternion math. Two interpolators, ScLerp (default) and LerpSlerp.
- Integer-nanosecond stamps carrying a clock domain in the type, so mixing a
  sensor clock with a host clock is a compile error rather than a silent wrong
  answer. Cross-domain lookup is an error until Phase 8 supplies alignment.
- `Copy` error types that name the offending edge. No `String` in an error type
  or on a hot path anywhere in the workspace.
- Builder-time edge declaration
  (`docs/decisions/0004-builder-time-edge-declaration.md`), which is what sizes
  the arena.

### Added — shared memory between processes, Linux only (Phase 2, `docs/PHASE2.md`)

Behind the default-off `shm` feature; see *Absent* for the one part of this that
is specified and not wired up.

- A `memfd_create`-backed, sealed, `MAP_SHARED` arena, and a zero-diff read path
  proven by a relocation gate — the same bytes answer the same at a different
  address in a different process.
- Discovery and rendezvous: `tf_tree::open()`, a lock file whose byte 0 is
  ownership, and an attach protocol over `SOCK_SEQPACKET` + `SCM_RIGHTS` served
  by a thread rather than a daemon.
- A participant registry, liveness derived from `F_OFD_GETLK`, edge claims held
  as OFD leases so a dead writer's claim is observable, and reaping by any
  read-write participant.
- Fork poisoning: a `pthread_atfork` counter, with five destructors guarded, so
  a tree inherited across `fork()` refuses rather than corrupting.
- Per-edge page population at take-up. `docs/PHASE2.md` §0.0 records the
  measurement: 66.3 MiB → 3.8 MiB resident on an over-provisioned arena.

### Added — Python bindings (Phase 3, `docs/PHASE3.md`)

Published as the `tf_tree` wheel. The bindings go straight to Rust through PyO3,
not through the C ABI, because typed errors and zero-copy buffers do not survive
a C boundary (`docs/PHASE3.md` §0).

- `tf_tree.open()`, batch lookup with NumPy in and NumPy out and no intermediate
  allocation, and `at_into` writing directly into a caller-owned NumPy array
  (only a NumPy array — see *Partial*).
- The GIL is released above a measured work threshold rather than always or
  never.
- Free-threaded CPython is supported: the module is `#[pymodule(gil_used =
  false)]`, which is what emits the `Py_MOD_GIL_NOT_USED` slot, so importing it
  does not silently re-enable the GIL for the whole process — the failure mode
  that has no error, no warning and no failed import. Wheels are `abi3` for GIL
  builds plus version-specific `cp314t`; `3.13t` is deliberately absent and
  `abi3t` awaits CPython 3.15.
- Hand-written `.pyi` stubs and `py.typed`, checked under strict pyright.

### Added — C, C++ and ROS 2 (Phase 4, `docs/PHASE4.md`)

Of this group, only `at_with_derivatives` reaches a published artifact — it is
part of the `tf_tree` crate. The C ABI, the C++ wrapper and the ROS 2 package are
build-from-source; see *What is and is not published*.

- A two-tier C ABI: `tf_tree.h` (stable) and `tf_tree_unstable.h` (opt-in behind
  `#define TFT_ENABLE_UNSTABLE`), at ABI version 0.5, which is versioned
  independently of this crate version. Handle model, `Copy` error identifiers, a
  panic guard at every boundary, and all five pose layouts in both directions —
  reading needs matrix→quaternion, which needs Shepperd's four-branch method and
  a determinant check, because a reflection and a scaled rotation both convert
  silently into a *valid, different* answer.
- A header-only C++ wrapper, `tf_tree.hpp`, with Eigen and Sophus interop, both
  error modes (exceptions and `-fno-exceptions`), and a CMake package a consumer
  reaches with `find_package(tf_tree CONFIG)`.
- `sample_with_derivatives` / `at_with_derivatives`, pulled forward from Phase 6
  because ScLerp already computes the twist.
- `ros/tf_tree_ros`: a one-way `/tf` and `/tf_static` → arena ingest bridge, as
  an `ament_cmake` package. Ingress only — **it never writes to `/tf`**, which is
  the Phase 7 line and not an oversight.

### Added — offline, observability (Phase 5, `docs/PHASE5.md`)

- **`FORMAT_VERSION = 3`**, `layout_hash 0x3D104195` (`tf_tree doctor
  --explain-version` prints both). The Phase 6 spline regions are already
  declared in the header, absent, so that format break happens once rather than
  again in Phase 6.
- **Frozen `.tft` arenas**: `Tree::freeze_to`, `Tree::open_frozen`, `tf_tree
  freeze`, and `Tree.freeze()` from Python. The arena bytes as a memory-mapped
  file, so sixteen dataloader workers share one copy. A frozen tree answers
  bit-for-bit identically to the live one it came from, and `docs/PHASE5.md` §2
  records that as tested rather than intended — by `crates/tf_tree/tests/frozen.rs`,
  which is compiled only under `--features shm` and so runs in `just shm-check`,
  not in `just test`.
- **Bag ingestion, MCAP only** — `tf_tree ingest --bag`, `tf_tree freeze
  --from-bag`, and `tf_tree_ingest` as a library. Two passes with a
  spill-to-run-file for recordings larger than the memory cap; zstd and lz4
  chunks decode through pure-Rust codecs, so a rosbag2 or Foxglove recording
  ingests with no C build step. A truncated recording is read up to the cut and
  reported as truncated rather than refused, because a SIGKILLed recorder is how
  bags in the field end.
- **Offline Python API**: `tf_tree.open_file()` returns the ordinary `Tree`, so
  there is no parallel offline surface to learn.
- **Diagnostic counters**, default-on behind the `counters` feature, with the
  arena regions declared whether or not the feature is compiled in — so turning
  them off does not fork `layout_hash`.
- **A diagnostics catalogue, `TFT001`–`TFT019`**, behind `tf_tree doctor`, with
  `--json` (schema `tf_tree.doctor/1`), `--exit-code`, `--suppress`, and
  `--from-bag` / `--from-file` so a recording or a frozen index can be diagnosed
  without a running robot. Sixteen of the nineteen can detect something; the
  other three say they cannot rather than passing (see *Partial*).
- **`tf_tree top`**: attaches read-only, refuses `--rw`, and renders per-edge
  rate/staleness/occupancy/writer, the participant list, a rolling event feed and
  a per-edge inter-arrival histogram. `--web` serves the same data over a
  hand-rolled HTTP/1.1 loop on `std::net::TcpListener` with a `default-src 'none'`
  CSP — no new dependency, no CDN.

### What is and is not published

**Published to crates.io:** `tf_tree`, `tf_tree_core`, `tf_tree_math`,
`tf_tree_arena`, `tf_tree_ipc`. **Published to PyPI:** the `tf_tree` wheel.

**Everything else in the repository is `publish = false`**, and two of those are
worth calling out because a reader will otherwise assume they arrive with the
crates:

- **`tf_tree_cli` — the `tf_tree` / `tft` binary — is not published.** So
  `tf_tree tree`, `echo`, `doctor`, `top`, `bench`, `ingest` and `topology` are
  build-from-source in this release, and `freeze` and `participants` are
  build-from-source *with* `--features shm` on Linux, which is the only
  configuration in which those two subcommands exist at all.
  `tf_tree` on crates.io is a library: `cargo install tf_tree` installs no
  command. It does not fail, either — on cargo 1.95.0 it exits **0** after
  `warning: none of the package's binaries are available for install using the
  selected features`, so a script that installs the crate and then looks for a
  `tf_tree` on `PATH` gets a green build and a missing command.
- **`tf_tree_c` — the C ABI — is not published either**, so the C header, the
  C++ wrapper and the CMake package are build-from-source too, as are both
  `ros/` packages (they need `rclcpp`, which only exists inside `docker/tf2`).

**And in the other direction: two of the published crates do carry a binary,
both multi-process test helpers rather than anything to run.** `cargo install
tf_tree_ipc` installs `tf_tree_ipc_child`; `cargo install tf_tree --features
shm` installs `tf_tree_rendezvous_child`. (The plain `cargo install tf_tree`
above installs neither — that target is behind the feature.) Each is named for
its crate because the names they had, `ipc_child` and `rendezvous_child`, are
names neither crate has any business owning in a shared `~/.cargo/bin`, and
crates.io is one-way: a version can be yanked, never deleted. That they install
at all is a residue rather than an oversight — each is spawned by its own test
through `CARGO_BIN_EXE_*`, a compile-time guarantee, and every way to install
*nothing* trades that for a path resolved at run time, in the two most
process-dependent test suites in the repository. Both manifests carry the
measurements.

### Absent, deliberately

Each of these is a decision with an argument behind it, not a gap waiting to be
filled in a patch release.

- **A `tf2_ros::Buffer`-compatible shim, and arena → `/tf` egress.** That is
  Phase 7, and D21 gates it on operating evidence rather than scheduling it.
  `docs/PHASE7.md` §0.0 lists four gates and none is met. That document is a
  requirements artifact; its existence is not permission to build it.
- **Cross-host operation.** Phase 8. Interest-based replication, a delta-coded
  wire format, and clock-domain alignment with reported uncertainty. A
  cross-domain lookup is an error today for exactly this reason.
- **Continuous-time interpolation — cumulative B-splines with analytic
  derivatives.** Phase 6. The arena header already reserves its regions
  (`docs/PHASE5.md` §1.2), which is why this release's format break is the only
  one Phase 6 needs.
- **Visualization.** `docs/PHASE5.md` §8 is a section about not building
  something, with the argument recorded. This is the finished state, not a gap.
- **ROS 1.** There is no path and there will not be one.
- **rosbag2 `.db3` (sqlite3) ingestion.** Refused on a measured dependency
  finding — `rusqlite` vendors C, `prsqlite` records no licence on the crates.io
  index so `cargo deny` refuses it outright, and the remaining readers are header
  parsers. A `.db3` handed to `tf_tree ingest` is *diagnosed* as one, with the
  `ros2 bag convert` remedy, rather than reported as a corrupt MCAP.
- **`tf_tree serve`, and a recorder.** `docs/PHASE2.md` §9 is superseded by
  `docs/decisions/0019`; keeping an owner alive is the operator's job in this
  release, which is what D16 says it should be.

### Partial, and stated rather than papered over

- **Ownership migration when the arena's owner dies (`docs/PHASE2.md` §3.5).**
  The lock-file protocol exists and is tested; **nothing triggers it.** No
  participant watches its client socket for `HUP`. Observable consequence: kill
  the owner and every already-attached process keeps serving lookups exactly as
  specified, but **no new process can join** — it wins the ownership byte, meets
  the split-brain check against the survivors, backs off, and times out with
  `ArenaHeldButUnreachable`, for as long as any survivor lives.
- **Three diagnostic checks cannot detect anything in any configuration, and
  report that instead of passing:** `TFT002` and `TFT003` (owned by
  `tf_tree_bridge::StaticStore`, whose state is process-local) and `TFT004` (no
  arena receipt time is recorded). Eight more skip conditionally, on evidence
  rather than on capability, and each says which condition it hit.
- **`at_into` acquires its output buffer by casting to `numpy.ndarray`, not
  through the buffer protocol** (`docs/PHASE3.md` §5.5's status note). So a
  `memoryview`, or a pinned `torch`/`cupyx` allocation that is not a NumPy array,
  is **refused whatever its layout** — with an error saying so and suggesting
  `np.asarray(...)`, not a wrong answer.
- **`freeze_from_arrays`** — building a frozen index straight from NumPy arrays
  — is not implemented. Unlike the `.db3` row above this is a schedule and not a
  decision: it needs no dependency and no format change.
- **Phase 4's exit criterion is operational and is open.** A real node on real
  hardware for a sustained period, and a written log of every surprise. No amount
  of code closes it, and it is what gates Phase 7.
- **`docs/PHASE4.md` §7's benchmark gate criterion 1** is measured as four
  quotients on an interleaved ladder rather than the single ratio its own table
  names, because the single ratio's denominator moved 43% on edits that did not
  touch it. That section records all four as passing, over twelve pinned runs;
  `docs/decisions/0023` is the record that would change the wording.

### Platforms

- **Linux `x86_64`** is where this is developed and gated.
- **Linux `aarch64`** is a target and not yet evidence: the CI matrix rows exist
  and have never executed.
- **macOS and Windows** get a Python wheel with the single-process engine only —
  no shared memory, no `.tft`, no `tf_tree top` — and nothing tests it. See
  `SUPPORT.md`.

### A note on the gate

**GitHub Actions produced no run for this repository between 2026-07-23 and
2026-08-16**, so most of this release was verified by the `just` recipes run
locally on `x86_64` Linux, and by the `## 0.0 Implementation status` tables
those runs are recorded in — which is what the note at the top of this file
means by naming those tables as the source of truth.

Making the repository public restored CI, and the first runs found three latent
defects that had been invisible for the project's whole life: `c_char`
signedness on `aarch64`, an exported symbol in no header tier, and a container
with no writable `ROS_HOME`. The `ubuntu-24.04-arm` rows now execute and pass
for the first time.
`.github/workflows/ci.yml`'s header carries the evidence for the outage and the
diagnosis. Do not read a green check as verification.

<!-- The tag does not exist until the release commit creates it, so this link
     404s until then. That is the accurate state, not a broken link to fix. -->
[0.0.1]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.1
