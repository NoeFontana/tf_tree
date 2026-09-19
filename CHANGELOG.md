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
and `PHASE5.md`, and `## 0.0 Status` in `PHASE7.md`. `PHASE1.md` and `PHASE3.md` have no `0.0` heading, and their
**definition-of-done lists — §13 and §14 — are their status tables**, with the
same authority. `PHASE3.md` also records deviations inline, in the section each
one belongs to; §5.5's buffer-protocol steps are the example.

This paragraph read *"`PHASE1.md` has none because Phase 1 is implemented
whole"* until 2026-09-09, and that was wrong in both directions at once: **six
of §13's nine boxes were satisfied and had never been ticked**, and two were
false — `loom` and `miri` ran on x86-64 only, and §11.2's cold-cache row is
measured nowhere. A sentence asserting completeness is exactly what stops anyone
from reading the list that would have said otherwise, which is why the list is
now named as the status table rather than explained away. Where this file and one of those disagree, they are right and this file
is a bug.

---

## [Unreleased]

### Changed — a repo-wide deletion pass over narrative prose

An eight-way sweep over the editable decision records, the phase specs, the
cross-cutting docs and the doc comments of every crate but two, applying the
rule above. Net −483 lines across 59 files, all of it narrative: what an earlier
revision of a comment said, how many attempts a repair took, and claims restated
two or three times in one document.

Deletion only. No surviving contract sentence was reworded, because the previous
entry's defects all came from compressing two scoped sentences into one general
one rather than from removing anything. Nine dangling-antecedent regressions —
a deleted sentence that a later pronoun still referred to — were caught and
repaired before this landed.

Out of scope and unchanged: the 28 `implemented` records, which are frozen;
`CHANGELOG.md`'s released sections, which are a historical record;
`crates/tf_tree/src/open.rs` and `tree.rs`, which many line citations point into.
`docs/decisions/0020` shed one citation and its budget row falls to match.

### Changed — a doc comment states the contract, not its own history

`CLAUDE.md` gains a rule: a comment states the decision and its load-bearing
evidence, then stops. Provenance — a retracted measurement, what an earlier
revision of the comment said, why a hardening was rejected — belongs in the
record that owns it, cited by number, and a measurement is kept only where
nothing else holds it.

Applied to the longest blocks first, with no contract statement dropped:
`tf_tree_math::slerp` (236 rustdoc lines to 97), `ParticipantTable::reclaim`
(110 to 91), `tf_tree`'s `cache::with_plan` (158 to 80), the shared
line-citation erratum in `docs/decisions/README.md` (54 to 16), and
`docs/API.md` §6 row 16, which held a third copy of `slerp`'s retracted
instruction counts inside a single table cell. What was cut was already pinned
by a named test or by the record that produced it. `slerp`'s ten tests are
unchanged; four of the five its rustdoc cited are still cited there, and the
fifth — `the_iso3_round_trip_it_replaces_agrees_as_a_rotation` — is named in
`docs/API.md` §6 row 16, which is where the claim it pins now lives.

`0061` (**draft**) proposes the missing half. Trimming a doc comment moves every
line citation below it, including citations inside `implemented` records, which
the freeze forbids repairing — the `with_plan` change above did exactly that to
`0034`, and the erratum is the only response available today. The record argues
a freeze protects the argument and not the pointers, which would also let the
citation gate **validate** targets rather than only count them: a sweep on
2026-09-19 found prefixed citations landing on blank lines, bare `///` markers
and closing braces, and none of them is currently reportable.

### Documented — a served `build_shared` arena is out of contract (`0031`)

`TreeBuilder::build_shared` registers a participant record and takes no lock
byte, because such an arena has no lock file: the fd is the capability. Publish
one through a hand-bound `tf_tree_ipc::OwnerServer` and every reclaimer reads
that record as dead — measured, and it costs the creator the edge it is
publishing to, repeatedly, with no data corrupted (`edge::reap` bumps the epoch
first, so the victim's next `push` is refused rather than interleaved).

**`0031` answers that this composition is not a shape the project supports.** The
supported way to serve a created arena is `tf_tree::Open::open`'s `Created` arm,
which is `build_shared` **plus** the rendezvous, the lock byte and the claim
leases. **No shipped path composes the byte-less served shape** — the only
`build_shared` call in shipped library code is that `Created` arm, and the two
that compose it without a lock file are the tests staging the measurement — and
neither the C nor the Python binding can reach it.

Nothing changes in behaviour. `reclamation_verdict`'s rustdoc said the question
was "being decided"; it now says what was decided and why nothing here changes
because of it, and `Tree::participant_slot`'s says that the peer which could hold
the wrong opinion only exists in the composition that is out of contract. The
advisory in 0.0.4's entry and in `PHASE2.md` §0.0 is **permanent, not lifted** —
both said "until `0031` is answered", which after an answer reads as expired.

**Where the boundary is written** (step 1). `TreeBuilder::build_shared`'s own
rustdoc says it, because that is where the call is, and `PHASE2.md` §3.1 — *The
sharing boundary is the runtime directory*, NORMATIVE — says it from the spec
side: an arena no runtime directory names sits outside that boundary, and
serving it is reaching back across. Three shipped sites described the shape and
are reconciled rather than rewritten. `tf_tree doctor`'s `TFT014` said
`build_shared` is "still supported" — true of the **call**, and not of serving
its result, which is the only way anything ever asks that check about a
byte-less record. `tft_tree_reap_dead`'s doc and `docs/RUNBOOK.md` each named
two producers of a stale claim that no hangup collects, a dead **owner** and a
byte-less `build_shared` participant, and now say which is which: the owner is
in contract and is what the sweep is for; the other is reachable only in the
composition this record refuses. The runbook adds what an operator does about
it — **do not sweep**, because `Tree::reap_dead`, `tft_tree_reap_dead` and
`Tree.reap_dead()` will take the claims of publishers that are running, while no
`tf_tree` subcommand sweeps at all, so `doctor` is safe either way.

**The sentence being retracted had five copies, and the plan named one.**
`TFT014`'s rustdoc said a byte-less record "reaches `slot_leak`'s `unknown`
byte row and is judged by `/proc` alone"; `docs/RUNBOOK.md` said it to
operators and `docs/PHASE5.md` said it in the spec. The claim is false — that
row is about **this run's** evidence, and a `--attach` run holds a lock file, so
the record reads `(byte free, process unknown)` and is reported as an abandoned
slot against a process that is publishing. The runbook copy was the one with
teeth: an operator reading a real `TFT014` finding was told to interpret it as
"the process is gone", three lines above the paragraph telling them not to
sweep. Found by grepping the claim rather than by working the list, which is
how the count went from three sites to five over four review rounds.

That rustdoc is twenty-four lines, so it moved every line-number citation into
`tree.rs` that pointed past it by exactly that much. The ones in `PHASE2.md`,
`PHASE3.md` and `PHASE5.md` are symbol citations now. One had been stale on
`main` since long before this change: `PHASE3.md` cited `tree.rs:918-933` for
`impl Drop for Tree`, which is some two thousand lines away, at a benchmark
note.

**The ones in frozen records are now unreliable and stay that way** — the
displaced sites are in `0005`, `0028`, `0034`, `0055` and `0059`, all
`implemented`, and a line-number erratum per site would bury
`decisions/README.md`'s real ones, so that file carries one erratum covering all
of them. *Unreliable* rather than *off by twenty-four*, for the reason two
paragraphs down. That is not a side effect to swallow quietly: it is the
ratchet's grandfathered set decaying on schedule, which is the argument the
ratchet was built on.

Every site that may be edited is repaired instead — `0019` and `0057`
(`ready`), `0027` and `0030` (`draft`), `docs/benchmarks/tf2.md`, and three in
Rust doc comments that the Markdown-only census had never looked at
(`crates/tf_tree_bench`'s `backing.rs` twice and `attach_bench.rs`). That list
was assembled from memory twice and then measured over a corpus that stopped at
tracked Markdown. It is stated once, in `decisions/README.md`, and no count of
the attempts appears in either place — the count was itself restated wrongly a
round after it was written.

**Repairing `0019`'s is also what showed this paragraph was overstating**: its
`tree.rs:1166-1181` was not displaced by twenty-four lines, it was *already*
pointing at the wrong item on `main` — the tail of `impl Drop for EdgeWriter`
and `impl Deref for EdgeWriter`, some six hundred lines from the `Tree::frame`
the surrounding sentence is about. So the
grandfathered set is not uniformly twenty-four lines out; it is
twenty-four-lines-out *on top of* whatever it already was, which nothing has
measured site by site.

*Two earlier versions of this paragraph: the first called all five records
frozen, and two are not; the second then stated a categorical over "the rest"
from the arithmetic rather than from reading them. A status and a citation are
both things to read, not to infer.*

### Fixed — `TFT014` accused a live publisher, and named a syscall it had not made

`tf_tree doctor --attach` reports `SlotLeak::Abandoned` against a `LIVE`
participant record whose lock byte is free. A byte-less
`TreeBuilder::build_shared` creator in a hand-served arena is exactly that
shape while it is **running and publishing**, and the check's own rustdoc said
such a record "reaches `slot_leak`'s `unknown` byte row and is judged by
`/proc` alone" — a claim about a run that read *no lock file*, offered as
though it were about the subject. `docs/RUNBOOK.md` and `docs/PHASE5.md` said
it too.

The verdict is unchanged, because it is the composition that `0031` refuses and
not the check that is wrong. What changed is everything the operator is told
about it:

- the finding's cause list named `Tree::attach_shared`, deleted by `0028` step
  0b, and did not name the live-publisher case. It names it now, and — **where
  a process is named at all** — says *check the pid is gone before you reap*;
- the evidence clause said *"/proc could not say what became of the process"*
  where the lock file had named no process for `/proc` to be asked about. It
  claims only that *this run* got no identity record, because a failed read and
  an absent one are folded together upstream, and it says which pid the finding
  is printing — the arena record's — so the remedy has something to point at;
- the `byte not probed` rendering said it meant a run that opened **no lock
  file**, in the check's rustdoc, in `PHASE5.md` §6 and in `docs/RUNBOOK.md`'s
  `TFT014` table. An `--attach` run reaches it too, for any slot whose
  `F_OFD_GETLK` returned an error — a failed probe is deliberately not reported
  as *free*, because that would be an accusation. The runbook's remedy had been
  telling such an operator to run the command they had just run;
- **and where neither the lock file nor the arena record names a process**, the
  finding prints no pid, no subject pid and no instruction to check one. A
  `RESERVED` record whose registrant died inside `fill_slot` is that shape, and
  a zero was reaching all three renderings — `slot_subject`'s own rustdoc had
  described that defect since it was written, over a fallback arm that printed
  the zero anyway. `named_pid` is the one predicate behind all of them;
- `Tree.reap_dead()`'s Python docstring carried the pre-retraction framing while
  the runbook was warning operators about that exact call — **and so did
  `python/tf_tree/_core.pyi`**, which is the one a wheel user's editor shows.
  The PyO3 docstring and the stub are a paired site that nothing gates, which
  `0057` had already found the hard way.

`a_byteless_record_in_a_served_arena_is_accused_of_leaking` executes the whole
shape, and its mutant is recorded against the first note written about it, which
predicted the wrong failure.

### Fixed — the line-citation ratchet scanned the wrong half of two documents

Found while writing the paragraph above, which first explained `docs/PHASE5.md`
reporting **zero** citations as "its citations are inside fenced blocks". They
are not. The fence-stripper is one day old — `ad22cef`, the commit directly
beneath this one on the branch — and it read
``re.sub(r"```.*?```", "", text, flags=re.S)``, which counts a triple backtick
written inside an *inline* code span, as `docs/PHASE5.md` and this file each do
once. That makes the marker count odd, and an odd count does not merely lose a
block: it **inverts the pairing** for the rest of the file, so prose is blanked
and the code blocks are scanned in its place.

`docs/PHASE5.md` therefore had no row in `scripts/line-citation-budget.txt` at
all, and **three** citations in prose that nothing gated — the row reads 2,
because this same commit converted one of the three to a symbol, so it records
what the file carries now rather than what it always did. The stripper is line-anchored
now — a fence is a fence only at the start of a line, blockquote markers
stripped — and the file gets the row it always should have had. **That row is an
addition to a file whose rule is that numbers only fall**, so the reason is
written in its header: the citations were always there, and the scanner could
not see them.

Two more silent passes in the same check are closed with it, both found by
asking what else the gate would swallow rather than by hitting them. **An
unclosed fence** now fails instead of blanking a file's tail. **A budget row
above its file** now fails too: each row is an equality, not a ceiling, because
a row added at 50 for a file carrying 2 used to print "within budget" plus a
"48 shed" *note* and exit 0 — which is the whole rule "a budget may only fall"
being enforced by prose. Both are proved by mutation: raising `PHASE5.md`'s row
to 50 fails, and appending an unclosed fence to `RUNBOOK.md` fails.

### Fixed — attach refusals did not fit the C ABI's message buffer (`0055` step 7)

`IpcError::HandshakeRejected` appended a per-status remedy to every rejection,
making messages of **112 to 378 bytes** against a `tft_error::message` of 256
that `set_message` truncates at 255. (The remedies themselves were 22 to 272
bytes; it is the rendering the buffer sees, and every figure here is one.) **Four of the seven reached a C
operator cut off mid-sentence** — four behind `tft_tree_open_named`'s 26-byte
wrapper, six behind the bridge's 35-byte one, and a count of truncations is a
statement about a prefix — and six were over the 220 bytes this crate's own gate
allows. They are **115 to 124 bytes** now over the statuses this library can
send (108 for `Ok`, which no code path here constructs; 133 at the widest owner
numbers) and every one of them fits.

The message states the status and the owner's `format_version` and
`layout_hash`, which a client can get no other way — and ends `(HandshakeRejected)`, the key that finds the runbook. **The
seven remedies are now `docs/RUNBOOK.md`'s `HandshakeRejected` section**, one row
per status, placed beside the header-validation checks that share two of their
names, because mistaking one for the other is the error an operator actually
makes: the handshake statuses are the *owner's* comparison against an attach
request, decided before this process ever saw a segment.

**`PHASE2.md` §3.7 asks a rejection to name *both* sides' values and this arm
prints one.** The hash half is older than this change — `tf_tree_ipc` depends on
`rustix` and `libc`, so it cannot read this build's `layout_hash()` to print
beside the owner's — and closing it means new fields on a published crate's
error type, so it is recorded in `0055` step 7 rather than fixed here. §3.7's
other clause for `LayoutMismatch`, that the message "must say exactly that",
*was* satisfied and moved to the runbook row in this change — which is the shape
`0059` settled for the sibling `ShmError::LayoutMismatch`, and that record
already calls this same §3.7 sentence stale for it. §3.7 has an erratum owed on
both clauses.

The remedy is unaffected either way; the runbook now says which build to read
the second number from, since `tf_tree doctor` prints the CLI's own and that is
a third value.

**Writing those rows found two of them false.** The `BootIdMismatch` remedy said
the arena had "outlived a reboot" and should be removed. A serving owner is
proof it did not, and the segment would not have survived one; the boot id
detects a stale *lock file*, not a served arena. `PHASE2.md` §13's failure-mode table carried the same false advice and now
carries the distinction instead; §3.7 gained the erratum it was owed; and
`HelloResponse`'s rustdoc — the published crate's own, where a caller
dispatching on the status reads — no longer says the two peers' boot ids "agree
by construction". The new
row says what that status actually means — the two processes disagree about which boot this is, so
one of them could not read `/proc/sys/kernel/random/boot_id` — and what to check.

`ModeNotPermitted` told a caller to attach read-only because the owner would not
let it write. **No owner in this workspace sends that status**: the wire carries
it because §3.7 lists it, and the check compares version, layout and boot id and
nothing else. The row now names all three places a rejection status can come from —
`OwnerServer::serve`'s decode failure, `check`, and the caller's `assign`
closure — and says that a peer which sends this one is not this implementation. Both errors survived every review this arm has had, because a
per-status list looks like it was derived from the producers and was derived
from the status names.

Both halves are gated. No rendering may name a status it did not get, which is
the original defect of this arm made unexpressible; each of the owner's two
numbers is asserted with the label that says which one it is; the runbook section must
carry a row per refusal status, each row's remedy cell must clear a length
floor, and the **remedy cells** must between them carry the words the message is
forbidden to carry — their union rather than each one, because requiring a word
per row would dictate vocabulary, and the remedy cells rather than the section
because the prose around the table says "rebuilding every participant" and the
`What the owner compared` column can hold a pointer the remedy then lacks;
`RejectionCarriedFd`, the sibling arm that also carries a `HelloStatus`, is held
to the same no-foreign-status rule, though its search key and runbook row are
recorded as owed rather than taken; and a new `HelloStatus` trips a wire-value
compile error in the test build: `status_is_a_refusal` is a total `match` kept
for no other purpose, because safe Rust cannot enumerate an enum and
`HelloStatus::from_u32`'s catch-all arm absorbs a new variant without complaint.
It lives in `#[cfg(test)]`, so it is `--all-targets` that fails and not a plain
`cargo check`. A downstream crate cannot have the same prompt — `HelloStatus` is
`#[non_exhaustive]` so that a newer owner's newer refusal keeps compiling — so
the other half is derived from the wire: `from_u32` is injective on the values
it names and folds the rest onto `Malformed`, so `tf_tree_cli`'s gate walks it
to get exactly the statuses the wire can deliver — over a range, not up to the
first repeat, so a status wired in at a gapped value is enumerated too — and a
new one owes a row the moment the codec can produce it. A status the codec cannot produce never reaches
a joining client at all. The per-status check
requires a table **row** rather than a mention, since the section's prose names
two of the statuses while telling them apart from the header checks that share
those names, and the worked example that section quotes is asserted to be
`Display`'s own output rather than a transcription of it, because a quoted
message is the shape that drifts. The runbook-reading half lives in
`tf_tree_cli`, which is not published: `cargo package` does not put `docs/` into
a tarball, so that `include_str!` beside the type would ship a crate whose tests
cannot build.

### Fixed — a C caller could not read the end of an error message, and `ArenaHeldButUnreachable` was 793 bytes of it (`0055` step 6)

`tft_tree_open_named` reports a failure by formatting
`could not open the arena: {e}` into `tft_error::message`, which is
`TFT_MESSAGE_LEN = 256` bytes. `tf_tree_c`'s `set_message` truncates at 255 and
substitutes `?` for **each non-ASCII byte**.

`IpcError::ArenaHeldButUnreachable`'s worst state rendered **793 bytes** at a
four-digit pid — 819 with that prefix, and more with a wider one. A C operator hitting a wedged arena read
to `"… so no forced create can pass this. Stop"` and no further, with every
em-dash as `???`. The remedy was entirely truncated away. Measured, not
inferred; the figure moves with the pid and the mask, which is itself part of
the problem.

**The message now states facts and ends with its own name; the remedy moved to
`docs/RUNBOOK.md`**, whose reader has every process in hand. The same state is
**143 bytes** and ASCII. The arm's range is **116 to 164**: the widest *ids* —
a 64-bit mask and two `u32::MAX`s — render 158, and the widest rendering of all
is 164, at **slot 0**, because `, the creator's` outweighs the nine digits a
wide slot adds. *This entry originally said "the widest ids those fields can
carry render 158" — true of the arm, and backed by nothing: the sweep behind it
pinned `first_pid` at 4242 and measured 152, and 164 was named nowhere. The
erratum that replaced it called 158 wrong, which it is not.*
The split is the point rather than the size: this type sees which lock bytes are
held and cannot see who holds them, so it cannot tell one process holding two
bytes from two holding one each — and a remedy that guesses is wrong in
whichever state it did not guess. It guessed wrong three times in a day. The
runbook's `ArenaHeldButUnreachable` section gained an eight-row table indexed by
the facts the message prints, placed where the search key lands a reader rather
than 165 lines into a bullet.

The messages end with `(ArenaHeldButUnreachable)` — `0059` convention (g)'s
parenthesised form, as `tf_tree_arena`'s `check.rs` and `frozen.rs` already
spell it. **Convention (e), at most 120 bytes, is not met**: these arms are **116 to
164** bytes, the upper end being the widest the fields can render, because (e) was derived for an arena error nested in two wrappers
whose payload is an errno, and this one carries a 64-bit mask and two 32-bit
ids.

**The gate that was missing.**
`every_ipc_error_message_fits_the_c_abis_buffer` now holds every `IpcError`
variant to ASCII and to a **220**-byte budget — 255 usable, less the longest
fixed C prefix, the bridge's 35-byte `shared arena could not be created: `,
which is the same wrapper `0059` measured. The worst message under it today is
205 bytes (`NetworkFilesystem`). The sampler sweeps every value each `Display`
switches on, not one value per variant, and the gate counts distinct
discriminants so a deleted sample fails it. `tf_tree_c`'s
`the_message_buffer_is_the_size_this_crates_budget_assumes` pins the 256-byte
buffer, the 255-byte truncation bound and the per-byte `?` substitution the
budget is derived from, because neither crate can see the other's constants.
Four message texts across two variants were non-ASCII and are now ASCII.

### Fixed — the escape hatch out of `ArenaHeldButUnreachable`, in the arm that did not say it (`0055` step 2)

`IpcError::ArenaHeldButUnreachable` has an arm for the state where the arena
creator's own slot 0 is still held. When other slots were held too, it sent the
operator to `docs/PHASE2.md` §3.4's escape hatch — *"that is when
`CreatePolicy::Always` becomes the escape hatch"* — and stopped there.

`CreatePolicy::Always` is two thirds of a create. A create also needs a layout
to build from, because decision `0004` sizes an arena from its declared edges,
and a read-write mode to build it in. The stranded-participant arm has said so
since 2026-09-10; this one had not, so **an operator who reached the hatch
through the slot-0 arm and followed it verbatim got a second, different error
instead of an arena** — `OpenError::NoLayoutToCreate` — at the moment they could
least afford it. Both arms now name all three parts, in the same terms.

**The same arm was telling an operator something false in a second way, and
that is fixed here too.** It matched `(Some(0), _)` — discarding
`ownership_held`, the one field whose own documentation says *"`Display` spends
it"* — so it printed *"it is the only holder, so an ordinary open will then
create"* even when the ownership byte was held by somebody else, where stopping
the slot-0 holder is necessary and not sufficient. The remedy now branches on
both bits.

**What each branch may claim is now the constraint, because the first repair of
this got it wrong.** `Display` sees which bytes are held; it cannot see who
holds them. That matters because the usual holder of both is **one** process: a
creator takes the ownership byte and then `CREATOR_SLOT` on the same `LockFile`
and keeps both for as long as it serves, so `holder_slots: 0b1, first_slot:
Some(0), ownership_held: true` is the steady state of every healthy
single-owner arena, and any joiner that times out without reaching the socket
reads that remedy. A first version of this fix told that operator to stop a
second process as well — after one that need not exist. Each branch now names
the bytes it knows about and hedges the holder, and
`a_live_owner_holding_both_bytes_is_not_told_to_stop_a_second_process` reaches
that state through the public API — a live, serving owner whose socket was
removed underneath it — and pins the hedge.

No type changed: errors stay `Copy` identifiers with the prose in the message
layer (`docs/API.md` R5). What is new is that the prose is now *pinned*.
`every_unreachable_state_reports_the_facts_and_prescribes_nothing`
(`crates/tf_tree_ipc/src/error.rs`; it was
`every_unreachable_remedy_names_what_the_operator_must_supply` until step 6
below took the remedy out of the message) asserts, per state, what the message
owes an operator, with a control so it cannot pass against a message that
promises nothing;
and `the_escape_hatch_creates_over_a_stranded_participant` asserts the verbatim
reading of the recommendation — in the stranded state, the policy alone returns
`NoLayoutToCreate`. Before this, every test of this error asserted `open()`'s
*behaviour*, which was already true, so the whole message could have been
reverted to a bare policy name with the suite green.

### Changed — `at_many` and friends read a whole chunk before they interpolate (`0060` Decision A)

`Plan::at_many`, `at_many_into` and `at_many_into_f32` no longer fold a batch
one stamp at a time. A monotone batch of three stamps or more is now walked in
chunks of sixteen and folded **step by step**: for each dynamic step, one loop
reads every lane's bracket through the seqlocked ring and a second loop
interpolates them. `Layout::QuatTwist` is deliberately unchanged.

**Every row is bit-identical to `Plan::at`**, per stamp, by `to_bits` — that is
what makes batch a layout and not a second answer, and
`crates/tf_tree/tests/batch_phases.rs` asserts it over eleven crafted numerical
regions, six plan shapes, both interpolation policies, both `inverted` flags,
and every stamp in lane 0, lane 1 and lane 15 of a chunk. A refused batch
behaves exactly as before: the rows before the first failing stamp are written,
that stamp's error is returned, the rest are untouched, and the counters still
count one lookup per row written.

Measured against `2524667`, interleaved paired runs, 7 reps, `taskset`-pinned,
on the workspace's own `[profile.bench]`:

- **−14.2% to −31.4%** on every 1024-stamp batch row, including −18.2% and
  −31.4% on the two plans that sweep the recorded `/tf` stream `0060` step 0a
  measured. The untouched `into_quat_twist_1024` control is flat to two decimal
  places.
- **No batch size loses.** A batch under three stamps takes the per-stamp fold,
  because the chunked path sets up its lanes whatever the batch holds; `N = 1`
  is −2.2%.
- At `[profile.embedder]` (cargo's `--release` defaults) the batch rows still
  win **10.4–24.2%**.

**Two things a consumer may care about, stated rather than buried.** The stack
frames of the batch entry points do **not** grow — 888 B and 344 B, the same as
before, with the lane buffers one non-inlined call in — so `API.md` §8.3's
page-fault residual is unchanged for a caller that never batches. And under a
live writer, the gap between two plan steps' reads for one stamp grows from one
fold to at most one chunk; a batch was never a snapshot and still is not.

### Changed — one bracket-read body behind `Plan::at` and the batch fold

`SampleRing::sample_from` and the new batch fold share one function: the
galloping search, the seqlocked slot reads and the trailing lap check exist
once, and the caller chooses at the type level whether it gets the bracket or
the interpolated pose. `Plan::at`'s result, its arithmetic and the position of
its lap check are unchanged.

**It costs, on one profile, and the number is here rather than in a footnote.**
At `[profile.bench]` every `lookup/*` row is within ±1.4% of `2524667`. At
`[profile.embedder]` — `lto = false`, `codegen-units = 16`, which is what an
embedder gets from a bare `cargo build --release` — the interpolating rows are
**+6% to +11%**. It is the split itself and not the batch fold: an arm carrying
this change's `sample.rs` with `2524667`'s own `plan.rs` reproduces it within a
percentage point. `docs/API.md` §2.3 already records that `lto = "thin"` in an
embedder's own profile erases a larger cost on this same path, and that is the
mitigation. `docs/decisions/0060` §11.3 carries the full reasoning, including
why two read bodies were not the answer.

### Added — the `at_many_shapes` bench group

One dynamic step, on-grid and off-grid stamps, moving and stationary edges,
under both interpolation policies. Benchmarks only. The shapes `0060` step 2
owed: on-grid is every bracket an exact hit and no `Interp::eval` at all,
off-grid is every bracket interpolating, and a stationary edge is the regime
four of the five dynamic edges of the one real recording in this tree are in.


### Added — the two `at_many` bench groups `0060` step 1 needed and nobody had run

`at_many_small` (N = 1, 2, 3, 4, 8, 16, 63 at both pose entry points) and
`at_many_recorded` (two plans over `indoor_atelier.tfstream`). Benchmarks only;
no shipped crate changes. `cargo bench -p tf_tree_bench --bench at_many`.

Every existing row in that file is 1024 stamps on the synthetic fixture, which
measures the steady state on data step 0a showed is not the shape real `/tf`
takes. What these two groups then measured (`0060` §10) amends its Decision A in
three places:

- **The two-phase fold's win is the phase buffering, and nothing else is close.**
  Hoisting each step's `(interp, ring)` out of the per-stamp loop — 3 072 arena
  lookups per batch where 3 would do — is **+0.71%** on the flagship. The loop
  order and the dropped out-of-line call are −1.3% to −3.4%. Deferring the
  `eval` carries the whole 23–25%. `0060` had all three as INFERRED hypotheses
  with *"none isolated"*.
- **Sixteen lanes beat sixty-four**: equal at N ≥ 63, ahead below it, at
  **6 600 B** of stack frame against 17 720 B. The record derived 4.8–5.0 kB for
  16 lanes and marked it *"not built"*; built, it is ~35% larger.
- **A batch smaller than a chunk is where it hurts**: **+82% at N = 1** even at
  16 lanes, crossing over between N = 2 and N = 3, because the lane buffers are
  initialised whatever the batch holds. Both sides of that threshold are now
  committed rows, so a small-N bypass has a measured boundary rather than a
  guessed one.

On the recorded stream the fold is −19.7% to −35.7% across two plans and both
entry points — better than on the fixture, and best on the plan that crosses a
motionless edge.

### Fixed — `shm_torture` could print `PASS` over an owner-kill arm that never fired

The harness's floor for *"the §3.5 arm is on and never fired"* tested
`duration >= OWNER_KILL_FIRST + every` — the schedule's **second** attempt, 12 s
at the defaults. A shorter run therefore got **one** attempt in its whole life,
and if that attempt deferred for want of a second eligible heir, the run printed
`PASS` beside its own `§3.5: 0 owner kill(s)`. That is the exact outcome the
guard's own comment says must land in the guard instead.

Test-harness only; no shipped crate changes.

- **Demonstrated, not argued.** Mutating the deferral test so every owner kill
  defers: `--duration 6s` exited **0** and printed `PASS`, `--duration 13s`
  exited 1. The 2026-09-16 scheduled nightly hit it for real — three children
  still in `D` state paging their own binaries in
  (`folio_wait_bit_common`), a census of 1, one deferral — and the
  `--no-inherit` self-test then failed asserting *"The owner was killed and the
  arena is ownerless"* over a run that had killed no owner.
- **The floor counts attempts now**, which is what the duration arithmetic was
  standing in for and cannot be off by a scheduling round. The message reports
  migrations *and* attempts.
- **A deferral re-arms in milliseconds, not at the next tenure.**
  `OWNER_KILL_DEFERRAL_RETRY` is 250 ms, clamped to `--owner-kill-every`. What a
  deferral waits for is a replacement's handshake — about a millisecond on an
  idle host, 106–173 ms on that runner by its own `[diag] slow-join` lines — so
  re-arming 8 s later made one transient thinness fatal to any run shorter than
  the schedule. A 6 s run now gets eight attempts instead of one.
- **The 24 s failure semantics are unchanged**, re-expressed as a budget
  (`3 × --owner-kill-every`) over one unbroken run of deferrals rather than as
  three consecutive schedule-paced attempts. Verified: a forced 40 s run wedges
  at 24.1 s of its 24.0 s budget.
- **The budget has a floor**, because it is derived from a caller's number:
  `--owner-kill-every 0s` is accepted and means "every round", which would
  derive a **zero** budget and make the *first* deferral fatal — stricter than
  the three this harness has always allowed. One second, four attempts at the
  retry cadence. Measured: with the floor the run reports a `1.0s budget` and
  tolerates six deferrals; without it, one.
- **`--defer-owner-kills N` is the positive control for the deferral path**, in
  the same family as `--stop-owner-ms`. Nothing else reaches it: `--children` is
  refused below the pool floor, and on an unloaded host a replacement's
  handshake is over in about a millisecond, so `--kill-hz 40` and six busy loops
  pinned to one core both leave every kill landing. Refused with
  `--no-kill-owner` rather than silently doing nothing.
- **The `--no-inherit` self-test no longer asserts a kill that may not have
  happened**, and names the population instead when the arm never fired.
- Three new self-tests, each run against a deliberate mutant and observed to
  fail.

### Added — `bracket_mix`: which interpolation region a real `/tf` stream lands in

[`0060`](docs/decisions/0060-the-batch-fold-that-reads-before-it-interpolates.md)
step 0a, the measurement that decides whether its SIMD kernel is worth having.
`cargo run --release -p tf_tree_bench --example bracket_mix` classifies the
brackets an `at_many` sweep reads into the five arms of `slerp` and
`screw_parts`, per edge, per policy and per 64-stamp chunk. **No engine code,
nothing timed, and no library surface changes** — it is a harness in
`tf_tree_bench`, which does not publish.

- **On the tree's one real recording, four of five dynamic edges never move**
  (202 of 202 intervals bit-identical) — four wheel-link frames published as
  *dynamic* `/tf` at 10 Hz that never change. The fifth is **99.2% series per
  bracket**; its 13.7% large-arc under a 100 Hz sweep is **two publication
  gaps**, 1.20 s and 5.30 s, carrying 650 of the sweep's 4 730 queries, and not
  fast motion.
- **Both interpolation policies share one series bound**, and it is an identity
  rather than a fit: `SIN_HALF_THETA_SMALL_SQ` is defined as
  `sin(THETA_SLERP_SMALL)²`, so both reduce to `θ ≤ 0.15 rad` between
  consecutive samples. The mix is therefore a function of publish rate, and that
  bound — not one recording's percentages — is what carries to another corpus.
- **A motionless `ScLerp` edge does not generally take its degenerate arm.**
  Whether `conj(q)·q`'s vector part cancels exactly is a property of the
  quaternion's zero pattern; with four non-zero components the residue leaves
  `sin²(θ/2) ≈ 5e-36`, which is 4.7e254 times `SCREW_DEGENERATE_SQ`, and the
  edge sits in the **series** region on rounding noise. `0060` §5 said otherwise
  and now carries the erratum. Nothing computes a different answer — the
  threshold was lowered ~280 orders of magnitude on purpose — but the *mix* does.
- **Every swept stamp is checked against `Plan::at` bit-identically**, because
  the bracket is not a public return and the harness mirrors `sample_from`
  rather than calling it. Four controls, one per class, and the stationary
  control as originally specified **failed**, which is how the `ScLerp` finding
  above was found.

- **`tf_tree_math`'s `SCREW_DEGENERATE_SQ` is now pinned** by a `const _: () =
  assert!(…)`, the way `interp.rs` already pins `SLERP_LERP_FALLBACK` and
  `THETA_SLERP_SMALL`. The harness above restates it, the constant is private,
  and the two crates cannot be checked against each other — so without the pin a
  change here would silently move a published classification. Verified by
  mutating it: the build fails at the assertion. No behaviour changes.

Registered in `docs/benchmarks/EVIDENCE.md`. `0060` remains a `draft` and this
authorises no engine change.

### Changed — Python exceptions carry the fields a handler branches on

[`0058`](docs/decisions/0058-the-fields-a-python-exception-only-printed.md),
which `docs/PHASE3.md` §4.4 had promised since Phase 3 and no exception had ever
kept. **Every new class subclasses `TfTreeError`**, so an existing `except`
clause catches exactly what it caught before, and message text is unchanged
except where an already-claimed edge's holder is still mid-claim.

- **Seven classes carry attributes on the instances the library raises**, set in
  the instance's `__dict__` with `args` left `(message,)`, so a pickled
  exception from a `multiprocessing` worker keeps them:
  `ExtrapolationError.edge`, `.requested`, `.oldest`, `.newest` and `.domain`;
  `DisconnectedError.target`, `.source` and `.cut_at`; `NoDataError.edge`;
  `TopologyChangedError.plan_generation` and `.current_generation`;
  `FrameNotDeclaredError.name`; `DerivativesUnavailableError.edge`; and
  `NoSegmentError.edge`. An edge is the arena's stored `(parent, child)` frame
  names, the shape `Tree.edges()` returns, and a frame its stored name; either
  is `None` where the arena holds no usable record. No attribute is an integer
  id. `.domain` is the query's time-domain tag, and the stamps are nanoseconds
  on that clock.
- **Migration:** nothing to change for a handler that reads only the class or
  `str(e)`. Code that asserted `vars(e) == {}`, or compared a raised instance's
  `__dict__`, now sees the attributes. **An instance you construct yourself
  carries none** — `tf_tree.ExtrapolationError("boom")`, a mock's
  `side_effect`, or one unpickled from an older build — and `_core.pyi`
  annotates the attributes precisely (`requested: int`, not `int | None`), so a
  handler that reads `e.requested` from such a double raises `AttributeError`
  and a type checker does not warn. Build test doubles by raising through the
  library, or set the attributes on them.
- **`TimeDomainMismatchError(TfTreeError)` is new**, with `.expected` (the
  path's or the plan's tag) and `.got` (the caller's). Both domain refusals
  raise it: `Tree.plan(..., domain=)` at plan time and `Tree.lookup(...,
  domain=)` per query. **Migration:** `except tf_tree.TfTreeError` still
  catches it; a check of `type(e) is tf_tree.TfTreeError` around either call no
  longer matches, and should become `except tf_tree.TimeDomainMismatchError`.
- **`NonMonotonicStampError(TfTreeError)` is new**, with `.edge`, `.last` (the
  newest published stamp) and `.got` (the refused one). `Publisher.push`,
  `Publisher.push_many` and `tf_tree.push` raise it for a stamp older than the
  newest on its edge. `.edge` is the arena's stored pair, resolved from the
  error's edge id, so for a frame name over 48 bytes it is the truncated pair
  `Tree.edges()` lists and not the one you typed; the message keeps your
  spelling. **Migration:** `except tf_tree.TfTreeError` still catches it; a
  `type(e) is tf_tree.TfTreeError` check around a push no longer matches.
- **`EdgeAlreadyClaimedError(TfTreeError)` is new**, with `.edge` and
  `.owner_slot`. `Tree.publisher` and `tf_tree.push` raise it when another
  participant holds the edge. `.owner_slot` is the holder's participant
  **slot**, never a pid, and is `None` while the holder's claim is still being
  taken (the message says so instead of printing slot `4294967295`, as it did).
  **Migration:** `except tf_tree.TfTreeError` still catches it; a
  `type(e) is tf_tree.TfTreeError` check around a claim no longer matches, and
  a handler reading `owner_slot` must allow `None`.
- **`ArenaHeldButUnreachableError(TfTreeError)` is new**, with
  `.holder_slots` (the held participant slots, ascending, as a tuple) and
  `.ownership_held`. `tf_tree.open` raises it when participants still hold an
  arena's lock bytes and nothing serves it — typically after an owner died and
  before a survivor calls `inherit_ownership`. It carries no pid: a recorded pid
  can name an unrelated process in another pid namespace, and `0` would make
  `os.kill` signal your own process group. The message is unchanged and still
  prints one. The class is registered on every platform. **Migration:** a
  `type(e) is tf_tree.TfTreeError` check around `open` no longer matches.
- **`ArenaAbsentError(TfTreeError)` is new**, a leaf with no attributes.
  `tf_tree.open` without `create=` raises it at once when nothing serves the
  name. A loop waiting for a robot to start catches `(tf_tree.ArenaAbsentError,
  tf_tree.ArenaHeldButUnreachableError)`; the two have no shared parent. The
  class is registered on every platform. **Migration:** a
  `type(e) is tf_tree.TfTreeError` check, or a match on the message "no arena is
  serving", around `open` should become `except tf_tree.ArenaAbsentError`.
- `just py-test` and `just py-test-freethreaded` now run `cargo build -p tf_tree
  --features shm --bin tf_tree_rendezvous_child` first: `TopologyChangedError`'s
  two attributes are held by a test that spawns that helper's `join-reparent`.

### Changed — the arena errors describe themselves, and five wrappers stop printing struct literals

[`0059`](docs/decisions/0059-the-arena-errors-that-cannot-describe-themselves.md),
extending [`0040`](docs/decisions/0040-the-error-that-cannot-be-returned.md) to
the four error types it did not reach. **Additive at the type level; message
text changes**, and message text is not a compatibility promise
(`docs/API.md` R5).

- **`ShmError`, `FrozenError`, `LayoutError` and `ParticipantError` implement
  `Display` and `core::error::Error`**, by hand, in `tf_tree_arena` and
  `tf_tree_core`, which stay `no_std` and gain no dependency. So `?` now works
  from `Tree::attach_shared`, `MappedArena::attach`, `FrozenArena::open`,
  `write_frozen` and `ArenaLayout::new` into `Box<dyn Error>` and
  `anyhow::Error`; `Tree::attach_shared`'s was `E0277`. `source()` is `None` on
  all four, and that is not promised either.
- **The text is ASCII and names the variant**, such as `LayoutMismatch`, the
  key `docs/RUNBOOK.md` is headed by. Layout hashes print in hex, the spelling
  `IpcError`'s `Display` and `tf_tree doctor --explain-version` already use,
  not in decimal, and an errno as `errno N` rather than the `Os { code, kind,
  message }` dump, so an operator no longer sees the strerror text the dump
  happened to carry. `FrozenError::LayoutMismatch` states that the `.tft` must
  be re-frozen, because `PHASE5.md` §2.4 requires that of a layout-hash
  mismatch and the Rust facade had never said it on its own.
  `FrozenError::Arena` states it too, for every arena-header failure inside a
  `.tft`.
- **`ShmError::ParticipantTableFull` no longer claims a full table**, in its
  text or its rustdoc: on `Open`'s joiner path it is what a taken or
  out-of-range granted slot is reported as.
- **`BuildError::Layout`, `BuildError::Shm`, `BuildError::Participant`,
  `OpenError::Map` and `FrozenFileError::Frozen` print their payload's
  `Display`** instead of `{0:?}`. `BuildError::Participant` loses its
  `participant table full:` prefix, which its payload now says; `OpenError::Map`
  and `FrozenFileError::Frozen` print the payload bare. The CLI's `open_frozen`,
  `freeze_to` and `--attach` errors improve with no change of their own, and so
  do the C ABI's `tft_tree_open_named` and bridge messages; **no status code,
  header or ABI changes**.
- **Python:** `open_file`, `build` and `open` no longer end a message with
  `The engine's reason, raw: ` and a `Debug` dump; they forward the engine's
  sentence. `open_file` on a `.tft` whose *arena* header's layout hash does not
  match now says to re-freeze, as the container header's mismatch already did
  (`PHASE5.md` §2.4).
- `just shm-check` now *runs* `tf_tree`'s `tests/error_payloads.rs` under
  `shm`; it had only clippied that target.

### Fixed — a dying owner is seen at the end of its exit, and the shipped docs said microseconds

[`0057`](docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)
step 3: the release-visible half of what step 2 corrected in the specs. **No
behaviour changes**; the docs a Rust, C and Python integrator reads now say
what the code has always done.

- **`Tree::owner_lost` read *"`POLLHUP` in microseconds, exactly"*.** It answers
  `true` once the attach connection has hung up and the last open file
  description holding byte 0 has closed — `PHASE2.md` §3.5's new NORMATIVE
  sentence — and for a dying owner that is the end of its exit: after any core
  dump and after its address space is torn down. On one host a small `abort()`
  dumping through a piped `core_pattern` was not visible, inheritable or
  joinable for about 1.1 s, and a 1 GiB `SIGKILL`ed owner for about 100 ms. The
  rustdoc, the C `tft_tree_owner_lost` doc and the Python `owner_lost` docstring
  and stub now say so, the C and Python texts as *"about a second"* with a
  pointer to `0057`. Only the rustdoc gives these figures and points at
  `RUNBOOK.md`'s per-process trade (a core limit of 1 byte suppresses the dump;
  nothing removes the teardown).
- **`tf_tree_ipc`'s crate doc and its crates.io page** said a `SIGKILL`ed
  participant's lock is released *"by the kernel, immediately"*; it is
  immediately **at the end of the holder's exit**, and the sentence now names
  any dead participant, `SIGKILL`ed or crashed, because only a crashing one
  dumps core and holds its lock through the dump. `client.rs`'s module doc,
  `peer_hung_up`, `server.rs`'s quote of D17, and the two NFS-refusal docs
  (`IpcError::NetworkFilesystem`, `reject_network_filesystem`) follow §3.3's
  corrected row. The NFS contrast they draw still holds.
- **A held byte 0 after a hangup is not always an heir, and one
  `Inheritance::OwnerAlive` or `Contended` is not final.** A fresh `open()`
  passing through §3.4 steps 2–4 takes byte 0, meets the survivors' participant
  bytes and gives it back. `0057` saw a first-call `Contended` or `OwnerAlive`
  in 21 of 120 trials with a joiner running, and `Inherited` on the next call in
  every one. `Inheritance::{OwnerAlive, Contended}`, `TFT_OWNER_ALIVE` and
  `TFT_CONTENDED` (and the unstable C header), `inherit_ownership`'s example
  (*"Contended is fine: somebody won"*), the Python snippet (*"somebody else
  won"*), `Session::take_over_ownership`'s *"somebody else is mid-bind"* and
  `LockFile::try_take_ownership`'s *"will be serving shortly"* now name that
  case. **A caller that treats one such answer as final can leave the arena
  ownerless**; the documented loop, which keeps no latch, retries by itself.
  `take_over_ownership` says what to retry on: a hung-up socket **and**
  `Session::ownership_held` reading byte 0 free, the pair `Tree::owner_lost`
  checks. A hangup alone is not vacancy, and retrying on it is the spin `0043`
  removed.
- **§3.5's NORMATIVE sentence has a pin, and it is two existing tests.**
  `a_read_only_survivor_reports_that_it_cannot_inherit` and
  `a_survivor_that_did_not_inherit_stops_being_told_the_owner_is_gone` already
  required the first `owner_lost()` after a `SIGKILL`ed owner's reap to answer
  `true`, with no timing threshold. Both now cite the sentence, and both fail
  against the two mutants `0057` names, run and recorded in their doc comments:
  a two-observation latch and a 100 ms grace period in `owner_lost`. Both of
  those fail at the heir's first poke, so a third mutant, a latch that withholds
  `true` only after byte 0 has been seen held following a hangup, was run
  against the migration assertion and fails it alone. They pin the
  event, not a duration, and each failure message names the one legitimate
  cause: another task, such as a `/proc/<pid>/fd` reader, holding a transient
  reference to the dead owner's socket or lock-file description.

### Changed — the two writer refusals name their edge (D11), and error payloads print as prose and can be named

**Breaking, on the `0.0.x` line.** Two variant shapes changed and one public
trait impl is gone; migration is mechanical:

- **`PushError::NonMonotonicStamp` gains `edge: EdgeId`** (`tf_tree_core`, and
  `tf_tree` re-exports it). It was the one `PushError` variant that did not name
  its edge, though the ring raising it always held one. A pattern that listed
  the fields — `NonMonotonicStamp { last, got }` — needs `, ..` or `edge`; its
  `Display` now begins `edge N:`.
- **`ClaimApiError::AlreadyClaimed(ClaimError)` is now
  `AlreadyClaimed { edge: EdgeId, cause: ClaimError }`.** `Tree::claim` knew the
  edge and dropped it on the `?` that converted the core error; it names it now,
  and the message reads `edge N: the edge is already claimed by participant slot
  S (one writer per edge)`. **`impl From<ClaimError> for ClaimApiError` is
  removed** — it was the conversion that lost the edge, and a bare `ClaimError`
  has no edge to give — along with a private shim whose fallback would have
  printed slot `0`, a real slot, for an unknown variant.
- **C ABI, no ABI change:** `tft_error.edge` is filled for
  `TFT_ERR_ALREADY_CLAIMED` and `TFT_ERR_NON_MONOTONIC` (it read
  `TFT_INVALID_ID`), and for a `TFT_ERR_TIME_DOMAIN` raised because a path's
  dynamic edges disagree among themselves. The field already existed and the
  header already allowed either value. `frame_a` still carries the owner slot on
  `TFT_ERR_ALREADY_CLAIMED` from `tft_tree_claim` — but not from
  `tft_bridge_create`, which overwrites `frame_a`/`frame_b` with the refused
  link's parent and child frame ids, as it did before. `tft_plan_create` gains
  an *Errors* section saying
  what `TFT_ERR_UNKNOWN_FRAME`, `TFT_ERR_NO_DATA` and `TFT_ERR_TIME_DOMAIN` mean
  when compilation raises them — a topology read that found no consistent
  snapshot or a corrupt parent index, a parent link with no edge, and a path
  whose edges disagree — and those three codes' docs point at it.

Additive and message-only:

- **Every public error variant's payload is nameable from `tf_tree`.**
  `TopologyError`, `ParticipantError` and `LayoutError` are re-exported at the
  root, and under `shm` so are `IpcError` and the eight types its variants
  carry (`EnvVar`, `HelloStatus`, `LockRole`, `NameProblem`, `ProcError`,
  `ProcParseError`, `RuntimeDirSource`, `WireError`). A caller could match
  `BuildError::Topology` but not name what was inside it without a second
  direct dependency. `docs/API.md` §6 row 19.
- **Payloads that have a `Display` are printed with it.** `BuildError::Frame`,
  `BuildError::Topology`, `ReparentError::Topology` and `AwaitError::Frame` used
  `{0:?}`, so a two-edge cycle read `topology error: WouldCreateCycle { child:
  FrameId(1) }`; it reads `topology error: attaching frame 1 under that parent
  would create a cycle`. `tf_tree_ingest`'s `IngestError::Push` likewise, under a
  comment that said the core had no `Display`. `ClaimApiError`'s three lease
  variants print `edge N`, not `edge EdgeId(N)`. **Not in this change:**
  `BuildError::Layout`/`Shm`/`Participant`, `OpenError::Map` and
  `FrozenFileError::Frozen` still print `Debug`, because their payloads
  (`LayoutError`, `ShmError`, `ParticipantError`, `FrozenError`) have no
  `Display`, and adding one to a published type is a trait commitment that
  wants a record extending `0040`.
- **`IpcError`'s sentence no longer splices in `Debug`**:
  `HandshakeMalformed` and `ProcError::Parse` describe the malformed reply and
  the unparseable `/proc/<pid>/stat` in words. No `Display` impl was added to
  `WireError` or `ProcParseError`.
- **`tf_tree participants`** reports an unopenable lock file through `anyhow`'s
  context chain (`opening <path>` / `cannot open the lock file (errno 13)`)
  instead of `LockFileOpen { raw_os_error: 13 }`, and loses a comment claiming
  `IpcError` was not `std::error::Error`, which it already was when the comment
  was written.
- **`Tree::lookup`'s `# Errors` says what `LookupError::UnknownFrame` also
  covers**: a name whose hash slot a different name holds (permanent) and a
  name an anonymous claimant is interning right now (transient). It names the
  write-free way to tell them apart and warns that `Tree::frame` on a writable
  tree is not one — it interns. `Tree::describe`'s remedy for an unknown frame
  mentions both causes rather than advising only a wait.

### Fixed — the Python binding's contract, where `PHASE3.md` §3 and §8.1 are NORMATIVE and the code was not

- **Every tf_tree exception pickles, and reaches a `multiprocessing` parent as
  itself.** All nine classes were declared with `create_exception!(_core, ...)`,
  which makes `_core` their `__module__`, and there is no importable module of
  that name: `pickle.dumps` raised `PicklingError` for every one, a
  `multiprocessing.Pool` worker's `FrameNotDeclaredError` reached the parent as
  `MaybeEncodingError`, and `ProcessPoolExecutor` handed back a bare
  `PicklingError`, so `except tf_tree.FrameNotDeclaredError` never matched in
  the parent. They are declared under `tf_tree` now, which is where `Tree`,
  `Plan` and `Publisher` already said they lived. **Visible change:** a
  traceback prints `tf_tree.ExtrapolationError`, not `_core.ExtrapolationError`.
  The class objects are unchanged, and `tf_tree._core.X is tf_tree.X` still
  holds.
- **A scalar float stamp meets §3's `TypeError` on every entry point.**
  `Tree.lookup`, `Publisher.push`, the module-level `tf_tree.push` and both
  stamps of `Plan.adaptive` took a bare integer, so a float was refused by
  PyO3's conversion (`'float' object cannot be interpreted as an integer`)
  without the 238 ns measurement §3 requires; they now go through the same
  refusal as `Plan.at`. That refusal, in turn, recognised only a Python `float`
  and its subclasses, so `np.float32` and `np.float16` scalars met PyO3's
  message on every entry point, `Plan.at` included; they meet the measurement
  now. The accepted types are unchanged, and so is `Publisher.push`'s
  `METH_FASTCALL`. **Not every float refusal carries it:** a float stamps
  *array* still raises numpy's own `TypeError` from `at` / `at_into` (on
  purpose — the two answer identically), and PyO3's `'ndarray' object is not an
  instance of 'ndarray'` from `Publisher.push_many`. `PHASE3.md` §14's box for
  this was ticked throughout and is split now; §3's amendment is the account,
  and says the ULP is still stated at a fixed 2026 epoch, not at the caller's
  magnitude.
- **`Plan.at_into` on the default `mat4` layout accepts an `np.int64` scalar
  and refuses a `float` with the measurement**, as `at` and `at_into(...,
  layout=...)` already did. It refused the first and reported the second as
  `tf_tree.BufferError`, and its own doc comment recorded that as "outstanding,
  not decided". **Behaviour change:** every stamps value now gets exactly
  what `Plan.at` does for it, where anything but an `int` or an `(N,) int64`
  array used to be `tf_tree.BufferError`. So integer-valued numpy scalars of
  any width (`np.int32`, ...) and 0-d integer arrays are **accepted**; anything
  else — a `list`, a float array — raises numpy's or PyO3's own conversion
  error, a `TypeError`, or an `OverflowError` for a `np.uint64` at or above
  2^63. `except tf_tree.TfTreeError` no longer catches a bad stamps argument on
  any path.
- **`from tf_tree import *` no longer shadows the builtins `open` and
  `BufferError`.** Both were in `__all__`, so a star-importer's `open("f")`
  raised `TypeError: open_arena() takes 0 positional arguments` and a bare
  `except BufferError` stopped catching the builtin — while `__init__.py` said
  the shadowing was "inside this module only". Both stay public as
  `tf_tree.open` and `tf_tree.BufferError`; neither is in `__all__`. **Visible
  change:** after `from tf_tree import *`, the bare names mean the builtins.
- **The `.pyi` types every scalar stamp as `int | np.int64`**, which is what
  §3 lists and the runtime accepts: `plan.at(np.int64(t))` and
  `plan.at(stamps.max())` were strict-mode errors on correct code. `just
  py-lint` now also runs `pyright` over `tests/python/typecheck_stamps.py`, a
  file of call sites, because nothing looked at what a caller's type checker
  said.

### Added — `ChildProcessDetachedError`, the class `PHASE3.md` §8.1 names

- **`Tree.freeze` in a fork child raised nothing: it killed the child with
  `SIGSEGV`**, and did from before this class existed. `Tree::freeze_to` reads
  the arena's manifest and bytes without asking whether the mapping survived the
  fork, and the binding did not ask either. It raises
  `ChildProcessDetachedError` now. **The Rust facade's `Tree::freeze_to` still
  has no check of its own** — recorded in `PHASE3.md` §8.1's amendment, not
  fixed here. **`Publisher.push_many` of zero samples** answered `None` in a
  fork child, since the fork check lives in the per-sample push; it raises too.
  **Not every call raises:** `is_shared`, `is_writable`, `Plan.depth`,
  `Tree.source`, `Publisher.release`, `owner_lost`, `reap_dead` and
  `inherit_ownership` answer without faulting, and §8.1's amendment
  lists what each says.

- **Every fork-child refusal raises `tf_tree.ChildProcessDetachedError`**,
  a subclass of `TfTreeError`, where it raised the base class — including
  `Publisher.push` and `push_many`, which chose their class apart from
  `detached_err`. §8.1 is NORMATIVE and names it. The earlier judgement (commit
  `4c040a3`) was that a detached tree is nothing a program branches on; a retry
  loop around the `TfTreeError`s that say "retry" is exactly such a program,
  and could stop on a dead handle only by matching message text. Existing
  `except TfTreeError` handlers keep catching it.
- **Out of scope, and said so:** §4.4's structured attributes (`.edge`,
  `.requested`, ...), a `KeyError` base for `FrameNotDeclaredError`, and the
  four classes §4.4 lists that were never built. No exception carries an
  attribute; `crates/tf_tree_py/src/errors.rs` claimed otherwise from its first
  commit and no longer does, and `PHASE3.md` §4.4 gains a dated amendment of
  what ships. The attributes need a decision record.

### Fixed — the one stored-name decode in `tf_tree` that did not clamp

- **`Tree::frame_name` sliced `&rec.name[..rec.name_len as usize]` directly.**
  `FrameRecord::name` is 48 bytes and `name_len` is a `u8`, so any stored length
  above 48 panics on that slice. `FrameRecord::for_name` clamps at intern time, so
  this is not reachable by writing a long name — but the bytes are read back out
  of a shared segment or a `.tft` on disk, and `validate_arena_header` validates
  the header, not per-record fields. **Six sites in the workspace decode a stored
  name and five already clamped**; this was the sixth, and it is on the
  *error-display* path reached through `Tree::describe`, which is where a caller
  looking at a bad arena already is.
- **It now goes through the file-local `stored_name`**, which is the same helper
  the other in-crate sites use, so the fix removes a third spelling as well as the
  panic. **One visible change**: an invalid-UTF-8 stored name used to collapse the
  whole name to `"<invalid-utf8>"` and now gets `from_utf8_lossy`'s per-byte
  replacement, which is what the other five already did and what a reader staring
  at one corrupt frame among ninety needs.

### Fixed — the catalogue is one table, so a check can no longer be declared and never run

- **`Tft::ALL` was the one of five parallel 19-entry lists the compiler did not
  enforce.** The variants, `ALL`, `id()`, `title()` and `severity()` were written
  out by hand; four are exhaustive `match`es, so a new variant does not compile
  until it is named in each. `ALL` is an array literal, and a twentieth variant
  left it at nineteen, compiling and never executed — `checks::run` walks `ALL`.
  Its doc claimed the opposite: *"a new variant cannot be added and then silently
  never executed"*.
- **Two hand-written tests failed to close it, and the second failure is the
  reason this is a macro.** The first sized a `seen` array from `Tft::ALL.len()`;
  dropping `TFT019` from `ALL` and its length to 18 moved both sides together and
  it passed. The second walked an exhaustive `succ` chain — but the arm rustc's
  error actually demands for a new variant is `Tft020 => None`, not the chained
  `Tft019 => Some(Tft020)` the comment asked for, and with it the walk still stops
  at nineteen and the test is green while `TFT020` never runs. Measured both
  times, not argued.
- **A `catalogue!` macro generates all four lists from one row per check**, so
  `ALL` cannot disagree with the enum. Verified by adding a twentieth row: the
  only errors are `checks::run`'s dispatch and one test's exhaustive match, which
  are the two places a new check genuinely needs per-check logic. The
  completeness test is deleted rather than kept — its premise is now structural,
  and a test that cannot fail is the thing this entry is about. Net 53 lines.

### Fixed — `Tft::ALL` is the one list a new check can be left out of, and its doc said the opposite

- **The guarantee the doc claimed is not one the type gives.** `Tft::ALL`'s
  comment read *"[`crate::checks::run`] walks this, so a new variant cannot be
  added and then silently never executed"*. `Tft::id`'s exhaustive `match` does
  refuse to compile until a new variant is named, so it cannot be added
  *unnamed* — but `ALL` is a hand-written `[Tft; 19]`, and a twentieth variant
  leaves it compiling at nineteen and never executed. Since `checks::run` walks
  `ALL`, that check would simply not exist, and `--json` consumers would see one
  fewer outcome with nothing reporting why.
- **`all_contains_every_variant` closes it by construction**: a `succ` chain that
  is an exhaustive `match`, walked from `TFT001` and compared against `ALL`. A new
  variant does not compile until it is named in `succ`, the walk then yields
  twenty against `ALL`'s nineteen, and the comparison fails until it is added
  there too.
- **The first version of this test was vacuous, and the mutant is what said so.**
  It sized a `seen` array from `Tft::ALL.len()` and asserted every slot was
  filled. Dropping `TFT019` from `ALL` *and* its length to `18` moved both sides
  together and it **passed** — a completeness check whose denominator is the thing
  being checked cannot fail. Against the chain version the same mutant fails,
  naming `TFT019`. Both runs are in the test's own doc comment.

### Fixed — two Miri gates: one that reported success having run nothing, and one that could not be made strict

- **`cargo xtask miri` printed a sentence and returned `ExitCode::SUCCESS`.** The
  arm read *"'miri' is wired up by its Phase 1 PR"* and did nothing else, while
  `main`'s usage line advertised `miri` beside `loom`, `bench-gate` and `headers`
  — so the one documented way to reach it was also the one way to get a false
  green from it. **Miri itself was never missing**: `just miri` runs
  `cargo +nightly miri test` over `tf_tree_arena`/`tf_tree_core` and `tf_tree`
  directly, and `just c-abi-check` runs four more rows over the C ABI. Nothing
  *executed* the stub, but **nine documents and comments advertised it** —
  `.cargo/config.toml`, `CLAUDE.md`, `README.md`, `CONTRIBUTING.md`,
  `docs/PHASE1.md`, `xtask/Cargo.toml` and two `tf_tree_bench` doc comments, all
  corrected here. (`docs/decisions/0003` keeps its copy: it is superseded and
  frozen.) Deleted; `cargo xtask miri` now exits 1 and names the two real
  recipes.
- **`just c-abi-check` assigned `MIRIFLAGS` where `just miri` appends to it, and
  that is the gate it mattered on.** `ci.yml`'s `miri` job sets
  `-Zmiri-strict-provenance` at the job level and the recipe folds it in; the
  argument for doing so is written out over that job at length. All four of
  `c-abi-check`'s Miri rows still spelled `MIRIFLAGS=-Zmiri-disable-isolation`
  outright, discarding whatever the environment set — so the **C ABI**, kind 4 of
  the unsafe budget, a foreign caller, and the highest-risk `unsafe` surface in
  the workspace, was the one Miri gate that could not be asked for the stricter
  run its sibling already gets. All four now append. This only makes the flag
  reachable; no job sets it there yet, and turning it on is a measurement, not an
  edit.
- **`RELEASE_VISIBLE` also covers `ros/` and the CMake package files.** Review
  found the first list blind to two surfaces that reach people who never clone
  the repository: the rclcpp bridge node, and what a `find_package(tf_tree
  CONFIG)` consumer installs. `#322` in this same release touched only `ros/` and
  the rule did not see it; it does now, and the flagged set over the 33 commits
  since `v0.0.5` grows by exactly that one commit.

### Changed — comments in the six remaining workspace crates say why, not what

- 33 files across `tf_tree`, `tf_tree_bench`, `tf_tree_bridge`, `tf_tree_c`,
  `tf_tree_cli` and `tf_tree_ingest`: 32990 -> 31795 comment lines. Same rule as
  the `no_std` pass — a sentence goes only when the fact it carries is still
  stated somewhere else in the same file — and the same verification: every file
  graded by a second reader against its parent, **zero must-restore**, and every
  line with code on it byte-identical.
- **3.6%, and the spread is the interesting part.** The concurrency and IPC files
  yielded 2-3%; `tf_tree_cli/src/top.rs` yielded 6% and the `tf_tree_bench`
  files similar. Benchmark and CLI code narrates; the arena protocols argue, and
  an argument has nothing to cut.
- Two files were reverted rather than kept. `tf_tree_c/src/bridge.rs` lost all
  six of its section banners, which would have left it the only file in a crate
  where three siblings still carry them; `tf_tree_c/src/layout.rs` lost the one
  marking where its tests stop exercising `write` and start on `read` (restored
  in place). Banners are navigation, not facts, so the survives-elsewhere test
  never applied to them — the rule permitted deleting them and the rule was
  wrong.
- Restored before landing, each caught by a grader rather than a compiler:
  `checks.rs`'s only statement of **why `TFT006` is a distance check and not a
  range check** (a publisher writing nanoseconds into a seconds field is off by
  10^9, which no plausible-range predicate distinguishes from a valid stamp) and
  its note that `TFT010`'s skip belongs to the same compared-nothing family as
  `TFT007` and `TFT008`; `bridge/ingest.rs`'s `Recreate` rung, without which
  `note_time_jump`'s pointer at "the argument for **both rungs**" led to a
  section arguing only `Halt`.
- Two comment defects fixed in passing. `impl Drop for Tree` carried a stray
  doc describing a boot id "folded to a `u64`"; `boot_id()` returns `[u8; 16]`
  and its own doc says **not** hashed to 64 bits (PHASE2 A7). `sample_interval`'s
  entire doc — the domain gate, the "`0` means never" contract, the clamp
  argument — was glued to the end of `recorded_offset`'s, so one function had no
  documentation and the other carried a contract that was not its own.
- Two more found in review of the PR, after grading. `tf_tree_ingest`'s
  `source.rs` had its reason that a short read compared against the clamped
  length cannot call a record complete on a file whose length does not change
  (`got <= file_len - 17 < want`) swapped for a pointer at "the paragraph
  above", a paragraph that does not make that argument; the inequality is back
  inline. And `tf_tree/tests/rendezvous.rs` justified bounding its deadline tests
  with a worker thread by saying the repository has no `.config/nextest.toml` —
  one doc, rewritten by the trim, "verified" there is no `.config/` directory at
  all — when that file exists and sets `terminate-after`. The thread is still
  right, for the reason now stated: it fails in 30 s naming the ignored
  deadline, where nextest would stop it at 180 s with a timeout that says only
  that something hung. The two `expect` messages that repeat the claim are code,
  and are left for a follow-up.
- **A C consumer sees this as `tf_tree.h` changing in comment lines only.**
  cbindgen copies `tf_tree_c`'s doc comments into the installed headers
  verbatim, so the trims reach it as text: no symbol, signature or constant
  moves, and the ABI version stays `0.8`. None of these 33 files' trimmed
  comments reach `tf_tree_unstable.h`; the third batch's do (below).
- **A third batch, 18 files in the same six crates: 5449 -> 5196 comment lines,
  4.6%**, every line with code on it identical to its parent. Graded the same
  way, and not zero this time. The banner loss above recurred — 18 banners
  across `tf_tree_c`'s `publisher.rs` and `unstable.rs` and `tf_tree_bench`'s
  `step_cost.rs`, `owner_migration.rs`, `dds_report_aggregate.rs` and `mp.rs`,
  all restored — and `unstable.rs` lost the sentence naming
  `TFT_LAYOUT_QVEC7_WXYZ_TWIST6`, without which the comment under it opened on
  "Every other layout"; restored. Four trimmed doc
  blocks reach `tf_tree_unstable.h` as comment lines only; `tf_tree.h` does not
  change and the ABI version stays `0.8`. And `tf_tree/tests/frozen.rs` carried
  the same false "no `.config/nextest.toml`" claim as `rendezvous.rs`: its
  300 ms budget is now justified by what it is, the whole cost of a guard-less
  build that polls until it answers `Timeout`, with nextest's 180 s bound beside
  it. Two stale comments fixed in passing: `owner_migration.rs`'s histogram doc
  said "10 ns linear buckets" where `BUCKET_NS` is 2, and now links the
  constant; and a `statics.rs` test doc said its fixture holds 2 contradicted
  edges against 7 observations, where it holds 3 and asserts 9, and named a
  `slot_for` that is `slot_or_insert`.
- **The follow-up: the same claim in code, and one more test.** The two
  `rendezvous.rs` `expect` messages now say what each outcome means — `Timeout`
  is an ignored deadline caught by the test's own 30 s bound, where nextest
  would have stopped a hang only at 180 s without saying why; `Disconnected` is
  a worker that panicked first — and the mutant note quoting one is re-quoted
  from a fresh run (30.008 s), not edited to match. `tf_tree_cli/src/web.rs`'s
  silent-client test said a hang would wedge `just test` with no diagnostic. Its
  20 s `recv_timeout` fails the `set_read_timeout` mutant in 20 s (re-run); only
  without it would the test hang, which nextest ends at 180 s and `cargo test`
  never does. Two comments that called the same situation a hang rather than a
  failure — `tf_tree_ipc`'s silent-client test and a `justfile` note — now say
  it is a failure at 180 s.

### Fixed — three broken intra-doc links, a stale complexity claim, and a spliced doc comment

- **Three intra-doc links resolved to nothing, and no gate could see them.**
  `sample.rs` linked `crate::edge::EdgeCfg`, which does not exist — the item is
  `EdgeRecord`, whose `capacity: u32` the sentence is about. `plan.rs` linked
  `[`Plan::new`]`, and `Plan` has no such constructor: `fold_into` derives those
  fields. `plan.rs` also linked a bare `[`EdgeCounters`]`, unresolved from that
  scope. All three are on **private** items, and `just doc` passes no
  `--document-private-items`, so rustdoc never looked at them.
- **`first_dynamic_edge` is O(1) and three comments still called it a scan.**
  Its doc justified being passed as a parameter because it is "an O(plan length)
  scan over the steps", and two hoist comments repeated that. It reads
  `dyn_count`/`first_dyn`, which `d546462` stored at compile time. The hoist is
  still right — the call is loop-invariant — but for a different reason, and the
  comments now say which. (A review attributed the change to `#264`;
  `git log -S dyn_count` does not support that.)
- **A citation to a test that does not exist.** `Plan::span`'s doc named
  `spans_agree_with_latest_common` in `tests.rs` as the pin for its agreement
  with `latest_common`. Nothing by that name exists anywhere in the workspace.
  The real one is `span_answers_exactly_at_the_ends_it_reports` in `tf_tree`'s
  `tests/behavior.rs` — wrong name and wrong file — and that test's own comment
  says it is "the agreement `Plan::span`'s doc comment claims".
- **Two doc comments were spliced together in `tests.rs`.** An eighteen-line
  block about `ExtrapPolicy`'s three variants sat on
  `by_ns_zero_is_never_claimed_for_a_pose_the_fold_invented` with its last
  sentence truncated at *"...under the narrower mutant of passing"*; the missing
  tail was standing alone 150 lines later as
  `extrapolation_is_selectable_and_reports_how_far_it_reached`'s entire doc.
  Rejoined and moved onto the test it describes.

### Changed — comments in the four `no_std` crates say why, not what

- 20 files, 9648 -> 9348 comment lines. No code changed: every line with code on
  it is byte-identical to its parent. What went is restatement — a sentence whose
  fact is still stated somewhere else in the same file — and nothing else.
- **3.1% is the measured yield, and that is the finding.** An earlier attempt
  with a "cut by half" target produced 24 must-restore and 193 should-restore
  losses across the same 13 files against 82 justified deletions, and was
  discarded. These comments are long because they carry rejected alternatives,
  consequences and measured numbers. Every file here was graded by a second
  reader against its parent; zero must-restore.
- Two trailing comments had quietly lost a why and are restored:
  `// -> odd (idempotent if already)`, and `// sin²(θ/2) — no sqrt taken`, which
  is the whole reason `norm_squared` is called there. A whole-line comment count
  does not see either.

### Added — `tft_bridge_close_startup_window`, and a reason code that does not say "authority"

- **The C ABI minor version is `0.8`.** `docs/decisions/0011` implementation
  step 6. The bump is for an added **unstable-tier** symbol, and the precedent
  is `1` → `2`, which bumped for `tft_bridge_note_time_jump` — also a bridge
  entry point, also unstable. The rule is `3` → `4`'s: the minor version answers
  *"can I name this symbol?"*, and a tier cannot answer that, because a tier is a
  statement about whether a symbol may later be **withdrawn**.
- **`tft_bridge_close_startup_window(b, out)` is the primary mechanism `PHASE4.md`
  §5.4 is normative about, and no C caller could reach it.** The only close the
  seam exposed was the 4096-transform backstop — a count, not a duration — so a
  bridge on a quiet robot sat with its window open for as long as it took to see
  4096 transforms, and the `rclcpp` node §5.4 says drives the close from a
  one-shot steady timer had nothing to call. It charges no counter, for
  `tft_bridge_note_time_jump`'s reason: it is not a transform, and
  `refused_after_halt` is a term in a ledger whose total is `transforms`. It
  routes through `fill` rather than formatting its own halt, so the latch, the
  `first_time` rate limiter and the halt's wording exist once. Called twice, with
  nothing recorded, or under a policy that is not `STRICT`, it reports the blank
  outcome — "nothing happened" as nothing having happened, not as a code a caller
  must learn in order to ignore.
- **`TFT_BRIDGE_REASON_STARTUP_CONFLICTS = 9`, because reason 5 named the wrong
  fault — and a `0.7` caller CAN observe this one.** A window-close halt was
  reported as `TFT_BRIDGE_REASON_AUTHORITY_CONFLICT` (5): the closest true code,
  and false as soon as the record contained a §5.7 static-value disagreement,
  which is a config-versus-robot fault and not an authority one. The two events
  also differ in shape — reason 5 is a judgment about the sample in hand and
  names both publishers in `owner`/`intruder`, while the close is about a *set* of
  edges counted long before, so it names none and leaves `parent`/`child` empty
  rather than printing whichever edge happened to be next on the wire. **The
  retest a `0.7` caller may owe is one `switch` arm**: that halt reaches it
  through `tft_bridge_offer`, whose signature and every other outcome are
  unchanged, so a caller that reached the 4096-transform backstop received 5 and
  now receives 9, and one that switched on 5 to print it falls through to its
  default arm. The action is `TFT_BRIDGE_HALT` either way.

### Added — `StaticStore::conflicts_by_edge()`, the accessor `0011` step 5 named and did not land

- **`Ingest` kept a second ledger of something `StaticStore` already knew.**
  `docs/decisions/0011` step 5 lists `StaticStore::conflicts_by_edge()` among the
  things it lands; it did not. A private `startup_static_conflicts: u32`,
  incremented off the store's `first_time` flag, stood in for it — and it could
  count the contradicted edges without being able to say *which* they were. The
  field's own doc comment recorded the debt by name.
- **The accessor yields `(parent, child, owner, intruder, count)`**, the shape
  `Authority::conflicts` already yields, because `docs/PHASE4.md` §5.4 asks one
  thing of both halves. **The iterator's length is the fault count; the `u64`
  beside each edge is how loud that one fault was** — `/tf_static` is
  `transient_local`, so one misconfigured publisher's latched sample is
  re-delivered to every late joiner, and `StaticStore::conflicts()` counts ten
  redeliveries of one misconfiguration as ten.
- **Reading `reported` is exactly equivalent to the counter it replaces**, because
  `Strict` closes its window once: at the close, a non-zero `reported[slot]` is a
  conflict seen before the close, which is what the counter accumulated.
- **The first shape was insufficient for the clause it claimed to enable.** It
  yielded `(parent, child, count)` and carried no publishers, while §5.4 is
  normative that the halt's `detail` enumerates every recorded edge *with both of
  its publishers* — and the data was not recoverable later either, since
  `StaticStore` retained only the owner. It now keeps the intruder of an edge's
  first conflict: **one clone per edge ever**, not one per observation.

### Fixed — the interval in which nobody can inherit, and the two controls that make it fail on purpose

- **`shm-torture-asan` was red on four of the last five nightlies, and the engine
  was never implicated.** The §3.5 trigger tally read `inherited=N` with zero
  `err-*` on every failing run, and a detached process cannot even ask
  (`NotApplicable` unless `is_joined()`). What drained was the population.
- **The mechanism is an interval in which the owner is dead and *undetectably*
  dead.** `kill_the_owner` censuses `heirs_before`, kills and `wait()`s the owner,
  then censuses `heirs_at_kill`. Linux `do_exit` runs `exit_mm()` **before**
  `exit_files()`, so the victim's page tables are torn down first and its
  rendezvous socket and participant lock byte are released only after — and
  `Tree::owner_lost` is a socket hangup. For the whole of that interval every
  survivor's trigger answers `false`, nothing can inherit, every survivor keeps
  drawing its 2 %-per-operation detach arm, and a survivor that leaves **cannot
  come back**, because §3.4 step 4 refuses `CreatePolicy::Never` against held
  participant bytes. If all of them leave, the census after the reap reads 0 and
  the arena is absorbing. The `kill()`-to-`wait()` timer *is* that blindness
  window — hangup and `wait4` measured equal to within 0.01–0.02 ms at every
  working-set size — so the census comment is right that the `wait()` cannot be
  moved.
- **It scales with dirty pages, not with ASan**: about 0.09 ms per resident MB. A
  plain torture child is 2.8 MB and reaps in **0.3 ms** median; an ASan child is
  43–49 MB and reaps in **6.7 ms**, about **22×** — and the victim is
  systematically the *oldest and largest* child, because the ordinary draw spares
  the role holder and the role holder extends its operation cap. **The plain build
  wedges too**: the ASan job's own `--children 4 --kill-hz 4`, no sanitizer,
  reached the same state at **900 s**. So the green `shm_torture` job is green for
  its `--children 6`, the two rows move two variables at once, and they were never
  a controlled comparison.
- **The fix is a `kill.in_progress` marker**, written before the signal and
  removed after the post-reap census, which a child checks immediately before
  leaving and stays instead. **Its cost is stated rather than implied**: §11.4's
  attach/detach churn pauses for the width of one reap — sub-millisecond
  normally (the 0.3 ms median above), tens of milliseconds under ballast — against a detach arm that fires
  about every fiftieth operation. It suppresses a *detach*, never a kill, an
  inheritance or a violation, and the run prints how many it suppressed, so a run
  leaning on it says so. The operation cap is the detach arm's quieter twin and
  had to answer the same question; that was missed in the first revision.
  Ninety-three consecutive owner kills recovered across four configurations including
  two-core ASan, and `--no-inherit` still fails naming §3.5.
- **Two positive controls, because this class had none and the obvious one is not
  portable.** Without one, a defect firing on ~1 % of kills is unfalsifiable in
  any run a person will wait for, and a fix can only ever be shown *not to have
  stopped it yet*. Both are siblings of `--inject-violation` and `--no-inherit`:
  deliberate failures.
- **`--victim-ballast-mb` gives each child N MB of dirty anonymous memory**,
  dirtied a page at a time because a `calloc`'d allocation sits on the shared zero
  page and an untouched page costs nothing to tear down. 512 buys a ~49 ms window
  on a plain release build and wedges an unfixed `--children 4` run at the
  **third** owner kill, in 25 seconds instead of once a night — that is how this
  was found. **It depends on the victim's pages being 4 KiB, and GitHub's runners
  set `transparent_hugepage=always`**: 256 MB becomes 128 huge pages rather than
  65 536 small ones, the reap drops from ~25 ms to **1.2–1.8 ms**, and the control
  silently does nothing. The first revision of the regression test was built on it
  and failed on CI for exactly that reason. Chunking does not rescue it — glibc
  serves 1 MiB requests from one ~64 MiB arena heap, which is huge-page eligible,
  with and without `MALLOC_MMAP_THRESHOLD_` (both measured).
- **`--stop-owner-ms` is the portable one, and it is what the regression test
  uses.** `SIGSTOP` the owner for N ms before killing it and the same state
  arrives with no memory physics in it at all: a stopped owner holds its
  rendezvous socket open, so no survivor's `owner_lost` answers `true` and nothing
  can inherit, and it has stopped serving, so no fresh process can join. At 300 ms
  an unfixed `--children 4` run wedges on its **first** owner kill, with `4 heir(s)
  attached before the kill, 0 after it`; with the fix, 20 of 20 recovered and 251
  detaches were suppressed.
- **Two diagnostic lines that a failing nightly could not print.** `heirs_before`
  was recorded, read by exactly one consumer (the `starved` test) and never
  printed, so a failing kill could not say how many heirs it had to lose —
  `heirs_at_kill = heirs_before - 1 - departures`, and with only the left-hand
  side in the log the departure count is not recoverable. And a DEFERRED line said
  "no read-write survivor besides the role holder was attached" without the number,
  so a reader could not tell a fleet holding only the role holder from one holding
  nobody at all.

### Added — a deliberately broken launch file, over a real RMW and across processes

- **`docs/PHASE4.md` §9's box asked for multi-publisher detection "verified against
  a deliberately broken launch file", and `find . -name '*.launch*'` found nothing
  anywhere in this repository**, `ros/` included. The box said so.
- **What `test_attribution.cpp` could not supply.** It constructs two publishers on
  one edge and asserts the second is dropped and both nodes are named — **in one
  process**. Two `rclcpp::Node`s in one process typically share a DDS participant,
  so the half of §5.3 that matters — that `rmw_message_info_t::publisher_gid` and
  `TopicEndpointInfo::endpoint_gid()` are the same sixteen bytes *across
  processes* — is not exercised by it at all. That is the configuration an operator
  actually misconfigures, and it had no test.
- **Three processes now**: `tf_tree_bridge` plus two `conflicting_broadcaster`s on
  `odom -> base_link`, over a real RMW, asserted through §5.4's operator-visible
  `RCLCPP_ERROR` in the bridge's own log. Run in `docker/tf2`; `just ros-test` is
  its gate, because `cargo` cannot see `ros/`.

### Added — PHASE3 §12.2 criterion 4 is measured, and this host cannot settle it either way

- **Neither half of criterion 4 had ever been run.** §12.2 criterion 4 is *"thread
  scaling >= 6x from 1 to 8 threads on `3.14t`"* and §7.3 requires *"a scaling
  test: 1/2/4/8 threads calling `plan.at` on a shared `Tree`, asserting near-linear
  aggregate throughput"*. The item had been classified as hardware-blocked; the
  interpreter is on this host — `python3.14t` 3.14.2, `Py_GIL_DISABLED == 1`,
  `sys._is_gil_enabled() == False` — so it was **unmeasured**, not blocked.
  `crates/tf_tree_bench/python/thread_scaling.py`, run by `just py-thread-scaling`
  and `just py-thread-scaling-gil`. **The readings live in
  [`docs/benchmarks/EVIDENCE.md`](docs/benchmarks/EVIDENCE.md) and are deliberately
  not repeated here** — they were written into four places at once and the copies
  disagreed in the third digit within one revision.
- **The verdict is the third state: not met, not failed, not producible on this
  host.** The free-threaded half *straddles* the 6× floor and the GIL build's
  1→8 arm sits below it. Under the one-sided argument each clearing run is a
  conservative pass on a host with half the cores the criterion's "8 threads"
  implies — but the margin is inside this instrument's own spread, so **this host
  cannot settle criterion 4 either way**; eight physical cores would. **"Passes" is
  too strong and "the host blocks it" is false — both were published in the
  evidence row and both were wrong**, which is why the row now carries the third
  state rather than a verdict.
- **The `at_into` arm the GIL half owed, priced by interleaved pairs.** The
  code-side suspect for the GIL shortfall was `Plan::at` allocating its (N,4,4)
  output before `fill` detaches; `--call at_into` prices exactly that with a
  caller-owned per-thread buffer and nothing allocated per call. Alternating
  `at`/`at_into` so both share the window, `at_into` is higher at 1→8 in **6 of 6**
  pairs and has a quarter of `at`'s spread, while sitting within ~2 % of it on
  *one* thread — the shape of a cost that is cheap alone and serialises under
  contention. **It still misses the floor.** So neither "the host blocks it" nor
  "the allocation is the cause" survives, and both had been published.
  **`--gate --call at_into` is refused**: §7.3's criterion names `plan.at`, and a
  criterion re-pointed at the faster call stops meaning anything.
- **Verdicts and refusals, all exercised**: `PASS`; `INVALID` on a shortfall where
  cores < threads; `FAIL` where the host has a core per thread, which `--gate`
  exits 1 on, because a verdict with no failing state is a gate that cannot fail;
  `INVALID` where no physical core count is derivable; a `--gate --serialize`
  refusal; `--batch 0` and `--seconds 0` rejected at parse time; and a 1-thread arm
  completing zero calls refused with exit 2 rather than dying of
  `ZeroDivisionError`, which would have exited 1 and read as a FAIL. Core counting
  is per-processor over `sched_getaffinity` **and** floored by cgroup `cpu.max` —
  `taskset -c 0,1` (two SMT siblings) reads **1** core and `-c 0,2` reads 2, where
  an earlier cap by *logical* count read 2 for the first and would have let 16
  sibling-paired CPUs pass as 16 cores.
- **Two earlier revisions of the evidence row were wrong in opposite directions**,
  and the detector for the first is now printed. The first published a **debug**
  build's curve and concluded the host blocked the criterion; the second published
  a clean PASS taken with the timer started *after* the workers were released,
  which counts work outside `elapsed` and inflates the ratio with thread count. The
  single-thread arm now prints ns/sample beside `tree.rs`'s documented **328
  ns/elem** for a release, pinned, depth-3 `at`, so a release run reads ~0.9× it and
  a `develop` run ~5.8×.

### Added — two measurements nobody could see: the quiet-host precondition and a nightly that notifies

- **Both halves are the same defect one layer out from the code: a result that
  reaches no reader cannot fail in the only sense that matters.**
- **`0023` step 5's instrument.** `docs/PHASE4.md` §7's gate wants twelve runs each
  recording `busy <= 0.10`, and nothing in `abi_cost` measured or printed a busy
  fraction, so no run could be *shown* to have been quiet. The step's own text
  settled the two traps: the sample must be taken **before** the run, because
  `mp::busy_fraction` reads `/proc/stat`'s aggregate line and `abi_cost`
  saturating one core is already ~12.5 % of eight CPUs — an in-run sample could
  never pass, for a reason that has nothing to do with the host; and the sampler is
  in the wrong crate to call, `abi_cost` being an *example of `tf_tree_c`* while
  `tf_tree_bench` depends on `tf_tree_c`. So: one entry point (`tf_tree_bench`'s
  `quiet_check`), not a second copy of the sampler, with `just abi-cost`
  bracketing the two runs — `after` once `abi_cost` has exited so its own core is
  out of the window, which is how a host that went loud *during* the run gets
  caught.
- **It exits `2`, where `abi_cost` exits `1` on a missed ratio: INVALID is not
  FAIL.** All three branches exercised: QUIET at 1.7 %, NOT QUIET at 15.6 % and at
  100 % (naming the top consumers), and `TF_TREE_BENCH_FORCE` at 100 %, which
  passes and **says so in its line** — a silently unfailable override is this
  entry's own subject, one layer down. What is still owed is the measurement:
  twelve readings at or below 0.10 need a window when nothing else is building on
  this multi-tenant box.
- **The nightly notified nobody.** Four of the last five nightlies were red and it
  was found on 2026-09-11 because a human guessed to look. The new `notify` job
  reports **jobs, not the run** — read from the jobs API, so a job added later
  cannot be silently dropped from the report, and so a reader is not sent back to
  the log-digging the job exists to spare them (the 2026-09-11 run was six green
  and one red). One standing issue rather than one per night, and it **closes**
  when the nightly comes back green, because an alert that never clears stops being
  read. `continue-on-error`: the messenger must never become the message. A first
  cut of the query returned **pull requests as well as issues** — GitHub's
  `GET /repos/{o}/{r}/issues` does — so a green night would have closed one.
- **`just abi-cost` hard-coded `./target/`**, so a set `CARGO_TARGET_DIR` sent the
  build elsewhere and the recipe ran a stale binary or none — the same trap
  `bench-check` and `c-header-check` carry.
- **`0023` is `ready`, and three sites still called it `draft`** — two justfile
  paragraphs and `EVIDENCE.md`'s §7 row, one of which told the reader to treat the
  ratified allowances as "a proposal a human ratifies by merging". A second reason
  for not wiring `abi-cost` into a workflow ("the thresholds are still a proposal")
  has expired outright and is marked as expired rather than deleted; the
  runner-variance reason was always the one doing the work.

### Fixed — PHASE5 §9.3's honesty section had a falsifier that could not fire

- **`docs/decisions/0021` step 4 asks for a deliberate revert to make the gate
  fail, and the second half could not happen.** The step says "give
  `idle_arena_resident_bytes` a direction and a tolerance … verified by
  `just bench-check` passing, **and by a deliberate revert of step 2 making it
  fail**."
- **`arena_memory_floor` is a `where_we_are_worse` *entry*, not a row.**
  `baseline::compare` diffed the *set* of those entries' ids and never looked
  inside one; `Report::validate`'s "prints numbers, gates none of them" rule was
  written over `self.rows` only; and `Comparison::compared_nothing()` missed it
  because the one gated *row* kept `checked` at 1 — a whole-artifact anti-vacuity
  check does not catch a per-entry one.
- **Run, not argued.** With the alignment fix reverted — the idle arena back to
  ~100 % resident — `main`'s gate printed `PASS — 1 directional metric held`, exit
  0: the defect the record exists to remove passes the gate the record says will
  catch it. After the fix, the same revert fails, far past the `RESIDENCY_SLACK`
  the baseline allows. `just bench-check` now holds two directional metrics
  instead of one, and the second is the first host-dependent number the gate has
  ever compared across two machines — CI's `bench-gate` on `ubuntu-latest`
  confirms it holds there too. The readings stay in
  [`docs/benchmarks/EVIDENCE.md`](docs/benchmarks/EVIDENCE.md); this entry does not
  repeat them.
- **The tolerance that shipped was not the tolerance every document explained.**
  Restoring `report.rs` wholesale after the falsifier experiment reverted
  `RESIDENCY_SLACK` 3.0 → 1.0, and the baseline was then regenerated *from the
  reverted code*, so the committed file agreed with the wrong number and every gate
  passed. `bench-check` structurally cannot catch that — the tolerance it reads is
  the baseline's own, by design — so `tests/baseline_file.rs` now pins the
  committed file against the constant.

### Fixed — documentation (2)

- **`TFT015`'s participant numerator is the lock file's, and the code said the
  opposite.** `docs/PHASE5.md` §6 defines `TFT015` as *"arena occupancy > 80 %
  (frames, edges, **participants**)"*. The participants row is absent, disclosed to
  the operator in `Meta.notes`, and the codebase was honest about the gap and
  **wrong about the remedy**: `checks::occupancy_of`'s doc closed with *"restore the
  row in the same commit that makes the engine maintain the counter"*. That
  instruction produces a wrong row twice over. **A header counter cannot be
  maintained** — a participant that is *killed* cannot decrement
  `ArenaHeader::participant_count`, so it drifts up for the life of the arena and
  never comes back down, which is the whole reason `PHASE2.md` keeps liveness in a
  lock byte the kernel releases (D17). **And the arena participant table is not a
  numerator either**, failing in the same direction: a read-only attachment is
  D18's default and Python's. [`0056`](docs/decisions/0056-the-participant-numerator-is-the-lock-files.md)
  (`draft`) records the correction: the numerator is the lock file's held
  bytes, which is the same source D17 already trusts for liveness.
- **The four "hardware-blocked" items, measured — three were misclassified and the
  fourth by a factor of nine.** Four items across `0013`, `0023`, `PHASE3` and
  `PHASE4` were recorded as blocked on hardware this repository does not have.
  Measured on the development host on 2026-09-11, in release builds: **one is
  genuinely blocked and much narrower than its text said, two are not blocked at
  all, and the fourth is half-blocked in a way its box does not distinguish.**
  `Fitness::probe` is four axes and every one of these items read it as a boolean —
  on this host `fair_for_timing` is **false** (SMT on, 8 logical / 4 physical, no
  cpufreq sysfs), `fair_for_ratios` **true** (`busy_fraction` 0.000–0.021 against
  `QUIET_ENOUGH` = 0.10), `fair_for_memory` **true** (`smaps_rollup` readable), and
  `enough_cores` yes for 1 consumer and no for 4 and 16.

### Added — decision records (2)

- [`0055`](docs/decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)
  (`draft`) — **recovery capacity cannot be added after the role falls vacant.** The three-night
  `shm_torture` nightly failure was root-caused on 2026-09-10 and **the ownership
  path is not defective**: §3.5 fired correctly nearly two thousand times in the
  investigation's own tally with zero errors. What the harness does is drive its
  eligible-heir population to zero while the ownership role is vacant, and that
  state is **absorbing**. An ownerless arena with any participant byte held admits
  no new rendezvous attachment — nothing is serving so §3.7's join cannot start,
  and the create path is refused by §3.4's split-brain check — and the rendezvous is
  the only door a would-be heir can come through, because `attach_shared`/
  `attach_shared_at` refuse `AttachMode::ReadWrite`, `attach_joined_at` is
  `pub(crate)` and reachable only from `Open`'s `Joined` arm, and
  `inherit_ownership` requires `is_joined()`. **So the set of processes that could
  inherit is fixed at the instant the role falls vacant and can only shrink.** The
  axis is **eligibility, not mode**: a candidate must be attached, read-write *and*
  actually polling `owner_lost()` at that instant, so `RUNBOOK.md`'s "open one
  process read-write even if it never publishes" is necessary and **not
  sufficient** — a fleet whose one read-write attachment never polls satisfies it as
  written and is squarely in the absorbing state.
- **The absorbing state happened again after the harness repair, measured here this
  time.** `nightly` run 34453737033 (2026-09-10): six of seven jobs green,
  `shm-torture-asan` red, and the harness classified the failure itself as
  **POPULATION, not engine**, with `inherited=9` and **zero `err-*`** over ten owner
  kills. `0055`'s *What forced it now* had cited the earlier failure with an
  explicit caveat that its numbers were quoted rather than reproduced; they are
  reproduced now at the failing job's own parameters (`--duration 120s --children 4
  --kill-hz 4`), where three runs without ASan pass 15/15 owner kills with zero
  deferrals and one *with* ASan passes too. **The number that settles the mechanism
  is the attached fraction**: `writers=1.6-1.7/4`, identically in CI and on this
  host, so ASan does not starve the pool. `--children 4` is exactly the harness's
  own `children_floor`, so `heirs_before` is essentially always **exactly 2** — the
  role holder plus one — every owner kill runs at zero margin, and the nightly's
  thirty minutes is ~15x the exposure of these runs.

### Fixed — an owner that died mid-handshake failed the joiner instead of being retried

- **A zero-byte handshake reply was reported as a protocol violation.** The §3.7
  client read the owner's response with `recvmsg` and handed whatever arrived to
  `HelloResponse::from_bytes`. When the owner died between `accept(2)` and its
  `sendmsg`, what arrived was **nothing**: on a `SOCK_SEQPACKET` connection a
  0-byte read is the orderly end of the peer's writing end, not an error. Parsing
  it produced `WireError::BadLength { got: 0, expected: 56 }`, so the failure
  reached §3.4 as `IpcError::HandshakeMalformed` — which is **terminal by
  design** — and an `open()` with its whole deadline left failed in milliseconds,
  blaming the owner for breaking the protocol. Seen once as `BadLength { got: 0 }`
  in a twelve-minute `shm_torture` run.
- **`IpcError::HandshakeClosed` is the new spelling, and it is a transient.** It
  classifies with `HandshakeIo` as "no server", so §3.4 goes round its loop and
  joins whatever serves the arena next. A new `IpcError` variant on a published
  crate, taken deliberately on the `0.0.x` line, rather than an errno nobody
  produced stuffed into `HandshakeIo`: there is no OS error here to report, and
  the two ways a dying owner reaches a client are worth telling apart — a
  connection the dead listener never accepted gives `ECONNRESET`, one it did
  gives this. Both were measured before the variant was written.
- **`docs/decisions/0005` had already specified this**, in its client-reachability
  table: *"`connect` succeeds, peer HUPs or times out mid-handshake"* → *"`Absent`.
  The ownership byte will be free and the §3.4 loop proceeds."* The timeout half
  was implemented and the HUP half was not, so no spec changed here — the code
  moved to the record.
- **`Absent` is safe for this arm, and the code now says why in place.** A
  spurious `Absent` cannot produce a second arena beside a live one: it leads to
  §3.4 step 2, where a live owner still holds byte 0, and step 4, which refuses
  to create while any participant byte is held — for every policy except
  `CreatePolicy::Always`, which is written to skip step 4, so the argument is
  not unconditional and the code says so where it is made. A forced create asked
  to abandon whatever was there, so this arm does not make it less safe. The
  dangerous misfiling is a *local* failure (`ClientSocketSetup`), and that arm is
  untouched.
- **Tested at both ends, and both tests were run against the mutant.** A unit
  test stages a real `accept(2)` and a close with no reply and pins the error and
  its verdict; `an_owner_that_dies_mid_handshake_is_retried_until_the_heir_serves`
  drives the whole §3.4 loop across real processes — an owner killed, a survivor
  holding a byte, and a child that aborts *inside its own slot assigner* — and
  asserts the joiner reaches the heir's arena and reads what the dead owner
  published. **No §11.3 crash point was added**: the assigner runs after the
  accept and before any response is built, so a child aborting there is the
  window, through public API only.

### Changed — the error a wedged joiner reads now says what the retry actually needs

- **`IpcError::ArenaHeldButUnreachable`'s operator text named a policy switch and
  stopped there.** It said to "create a fresh arena and abandon this one", which is
  the right advice and not a runnable instruction: a create also needs a layout to
  build from and a read-write mode to build it in. Through the `tf_tree` facade
  those are `Open::layout_if_creating` and `AttachMode::ReadWrite`, and without
  them the retry the message recommends fails with a second, different error. The
  message now names all three. Operator-facing text on a published crate, so it is
  recorded here rather than left to the diff.
- **The torture harness drove its own eligible-heir population to zero**, and
  three arms drew from one pool with none of them knowing it: `kill_the_owner` now
  censuses *before* the kill and **defers** rather than taking the last eligible
  heir; the ordinary victim draw no longer takes the role holder and stops against
  a pool already at its floor; and a worker that is serving the rendezvous no
  longer abdicates on its detach arm or its operation cap — that arm fires on ~2 %
  of operations, so a worker abdicated roughly every fifty against an owner-kill
  interval measured in seconds. Every reduction is counted and printed, **including
  the zeroes**, and a run that killed nobody now fails: an unfailable throttle is
  the shape of a gate that has quietly stopped testing. That remedy did not end the
  condition — see the entry above on the `kill()`-to-`wait()` window, which is where
  it went next.

### Fixed — `0048` step 4: D4 now holds for every root that carries `unsafe`

- **Twelve crate roots carried `unsafe` with no D4 posture**, and `0048` named
  that as its own outstanding step: eight `tf_tree_c` tests and examples, plus
  `heap_alignment.rs`, `zero_alloc.rs`, `relocation.rs` and
  `steady_state_alloc.rs`. Each now declares `#![allow(unsafe_code)]` +
  `#![deny(unsafe_op_in_unsafe_fn)]` with the kind stated **by number and name**,
  and says why the posture is declared rather than inherited — the parent
  library's attribute does not govern a test or example, which is a separate
  crate root. That is `0048`'s whole subject.
- **Kinds are per root, not per crate**, which is the amendment `0048` made to
  `0007` rule 1: kind 5 (our own C ABI called from Rust to measure it) on the
  eight `tf_tree_c` targets; kind 6 (a trait the language requires be
  implemented unsafely, in a target that never ships) on the three allocator and
  `Send`/`Sync` roots, each naming its actual `unsafe impl`; kind 1 (the arena's
  raw memory) on `heap_alignment.rs`.
- **No new register rows were needed.** `scripts/unsafe-budget.txt` already
  covered every one of these files by file set, and `just unsafe-budget` was
  green before and after at 30 files / 488 sites / 25 selectors — so step 4 was
  posture, not coverage, and the census could not have caught its absence.
- **`0048` is `implemented`.** It was promoted from `draft` to `ready` earlier in
  this release with step 4 named as the remaining work; that work is done.

### Fixed — spec sections that describe an arena no participant would attach to

Seven NORMATIVE or near-NORMATIVE claims in `PHASE1.md`, `PHASE2.md` and
`CONTRIBUTING.md` were checked against code and are wrong. Each correction is
recorded in place rather than overwritten.

- **`PHASE1.md` §4.1 carried `**NORMATIVE layout**` over a `FORMAT_VERSION = 1`
  header.** The shipped one is **3**, the struct is 320 bytes (was 256), and
  nine fields are missing from the block. The block is kept as Phase 1 history;
  `crates/tf_tree_arena/src/header.rs` is named as the normative source.
- **§4.3's region table listed eight regions; there are eleven.** Missing: the
  participant table and both counter regions. Wrong: the header (256 → 320), the
  frame-hash stride (`8 + 4` → 16, widened by A8's `claiming` array), and the
  topology stride (10 → 12 bytes/frame, because `edge_of_child` lives there).
  `layout_hash` folds these strides, so a table that disagrees with them
  describes an arena no participant would attach to.
- **§5.2 said the topology is "two blocks, double-buffered"**; `TOPO_BLOCKS` is
  **4** and A1 replaced the double-buffer.
- **§4.3 asked for `ArenaLayout::from_edges`**, which does not exist; the
  shipped constructor is `ArenaLayout::new`.
- **§8's API snippet had four defects** — the block a reader copies, and nothing
  checks it: `Interp::ScLerp` where the builder takes `InterpPolicy` (`Interp`
  is a trait); `Publisher` annotated on `tree.claim`, which returns
  `EdgeWriter<'_>`; `claim(odom, base)` inverted against `claim(child, parent)`;
  and `plan(cam, map)` and `lookup("map", "camera_optical", …)` naming opposite
  directions while both take `(target, source)`.
- **`PHASE2.md` §5's `ParticipantRecord` was its Phase 1 shape.** Six
  differences, two load-bearing: `state` has no `3 = detaching` (a departing
  participant goes straight to `FREE`; the socket is the liveness signal, D17),
  and every field is atomic — the old listing's plain `pid`/`start_time` would
  be a data race, since two processes read them while a third publishes.
  `incarnation` is new; `mode` and `name` are gone.
- **`PHASE1.md` §1 and `CONTRIBUTING.md` both said `tf_tree_math`'s property
  tests "run under Miri in seconds".** They never have: `just miri` names
  `tf_tree_arena`, `tf_tree_core` and `tf_tree`. What the crate's freedom from
  `unsafe` actually buys is being cheap for Miri to interpret as a *callee*.

### Fixed — PHASE1 §13 was nine unticked boxes over a phase called "implemented whole"

- **Six of the nine were satisfied and had never been ticked**: §11.3's written
  explanation (it is `0013`'s *Resolution*), the unsafe attributes and the
  single `#[allow]`, all seven `doctor` checks with a positive *and* negative
  test each, the five §3.1 conventions, and `PHASE2.md`'s A1–A8. Two are
  genuinely open and now say what is missing rather than nothing at all.
- **`loom` and `miri` now run on aarch64.** Both jobs were `runs-on:
  ubuntu-latest` with no matrix for the life of the project, while `test` and
  `shm` have carried `ubuntu-24.04-arm` since aarch64 CI became real — so the
  model checker that exists to catch `PROJECT.md` §6's *"weakening an atomic
  ordering because a test passes on x86-64"* ran only on x86-64. Loom remains
  the argument rather than a weak-memory proof; what the row buys is that the
  argument is no longer produced solely on the architecture the smell names.
  `just test-doc-error-codes` stays on one row: rustdoc diagnostics are
  identical on both.
- **`#![deny(missing_docs)]` is on the four publishable roots that lacked it.**
  The workspace has set it to `warn` with `just lint`'s `-D warnings` promoting
  it all along, so the gate was effective — but only inside that recipe, and the
  box asks for it at the root. It binds this repository and its path dependents,
  **not** a downstream consumer: cargo builds registry dependencies with
  `--cap-lints allow`, which caps an attribute-level `deny` too.
- **§11.2's cold-cache row is measured by nothing**, which is why the
  bench-gate box stays open: a runner that "reports the full table" cannot exist
  until that row has an artifact or the row is withdrawn.
- **Box 5 cannot be closed as written**: 449 `unsafe` blocks against 463
  `// SAFETY:` comments, but "naming a §2 invariant" is unsatisfiable for the
  majority, whose subject is the OS, a foreign runtime or our own C ABI rather
  than the arena. Restating it against `0007`'s kinds is an edit to the box.

### Changed

- **`SampleRing::mask` is a method, not a `pub` field** (breaking, `tf_tree_core`
  only — the type is not re-exported from the `tf_tree` facade). The struct
  stored the ring capacity twice, as `poses.len()` and as a `pub mask: u64` whose
  relationship to it its own `# INVARIANT` asserted and nothing enforced. A ring
  built with the two disagreeing returns a **silently wrong pose**: `capacity`
  and `retained` compute a window the mask cannot address, so `oldest_stamp`
  reports a sample it excludes and the sampler interpolates the wrong pair — no
  error, no panic, and `push`'s own `debug_assert` does not fire. `mask()` is
  derived from `poses.len()`, so the two cannot disagree. Callers outside the
  crate read `ring.mask()`.

- **`PHASE5.md` §1.2's region-table clause is retracted, and the stride array is
  coupled to the region count** (`docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md`,
  now `ready`). `FORMAT_VERSION = 3` reserved Phase 6's **header fields** and not
  a region slot, so a Phase 6 spline region is a twelfth region and costs another
  `FORMAT_VERSION`; the clause said the opposite in `PHASE5.md` §0, §0.0, §1.1,
  §1.2 and §13 and in `PROJECT.md` §4, and `CLAUDE.md` had already been
  corrected past it.
  `docs/PROJECT.md` §5.1 opens the ledger that break's entries queue in.
  **The one code change is one line**: `layout_hash`'s stride array is declared
  `[u32; N_REGIONS + 1]` rather than `[u32; 12]`, which turns a forgotten stride
  from a `HeaderInconsistent` a peer reports at attach time into `error[E0308]`
  at the build that forgot it. `layout_hash` is unchanged. `0032`'s question 1
  and the `frozen.rs` doc comment corrected beside it carry the measurement.

- **The C ABI's count-returning entry points carry a panic guard**
  (`d5dd109`, `docs/PHASE4.md` §3.4 / §9). `error::guard` returns a
  `tft_status`, so it fitted only the boundaries that report through one, and
  `tft_tree_frame_count` / `tft_tree_edge_count` — both in the *unstable* tier —
  had no guard at all. They now run under `error::guard_value` and **return `0`
  instead of aborting the process** if the body panics. No signature changes and
  no header changes; what moves is the behaviour of a panicking build, from a
  process abort in the C or C++ host to a count that reads as "nothing to
  report". `tft_test_panic_value` forces the panic and is the test; it is in the
  `TEST_ONLY` tier (`xtask/src/headers.rs`), so it reaches neither shipped
  header.

- **`tf_tree doctor` reports the resolved rendezvous runtime directory**
  (`973e594`, `docs/PHASE2.md` §15). The human renderer prints a `runtime dir`
  line and `--json` gains a top-level `"runtime_dir"` key, `null` when
  resolution fails. It is resolved independently of the arena — the run in which
  an operator most needs to know which directory was searched is the run in
  which nothing was found in it — and it carries the rule that produced it
  (`$TF_TREE_RUNTIME_DIR`, `$XDG_RUNTIME_DIR/tf_tree`, `/run/tf_tree`,
  `/tmp/tf_tree-<uid>`). **The `--json` schema tag is unchanged at
  `tf_tree.doctor/1`**, which `crates/tf_tree_cli/src/catalogue.rs` says is
  bumped only for an incompatible change; an added key is not one.

### Added

- **`docs/PHASE5.md` §12 criteria 2 and 5 are measured and gated**
  (`just gate2`, `just gate5`). Both read *held by nobody — never measured* until
  now, and both PASS with margin, so each is a **regression guard rather than a
  discovery**.
  - *Criterion 2, `.tft` open under 10 ms for a 233 MB index*:
    `crates/tf_tree_bench/src/bin/frozen_open.rs`. The gated resident arm clears
    the 10 ms budget by more than two orders of magnitude, worst of 8 fresh
    processes; the recipe prints the run's own numbers and no interval is quoted
    here, because a range over a handful of runs is a sample rather than the
    instrument's spread. **Only the resident arm gates**, together with a scale-invariance ratio
    against a fixture two orders of magnitude smaller: an open takes exactly one
    major fault when the cache was dropped and zero when it was not, *at both
    sizes*, so the evicted arm's size dependence lives inside that single fault
    and is a property of the storage rather than of `open_frozen`.
  - *Criterion 5, ingest throughput ≥ 10× real time*:
    `crates/tf_tree_bench/src/bin/ingest_throughput.rs`, recorded in
    [`0050`](docs/decisions/0050-what-ten-times-real-time-divides.md). The
    **grouped** arm clears the 10× floor by more than an order of magnitude, and
    it is the gated one because §12's own
    four-hour representative recording does not fit `DEFAULT_MAX_MEMORY_BYTES`
    and takes two fill passes. The corpus is generated at run time, and the
    density is measured off the survey and floored, because "10× real time" is a
    statement about the corpus as much as about the code.
  - Both publish an absolute duration on a host that fails `Fitness::probe`,
    under **`docs/PHASE5.md` §9.3's new one-sided-budget amendment**: every
    check that probe fails can only make the measurement slower, so a PASS with
    margin is conservative and a FAIL is not attributable to the code. Neither is
    a `bench_report` row; `tft_open_vs_bag_parse` stays `unavailable` — both of
    its halves have a recipe now and no recording is on both sides.
  - Each verdict is red-testable **without editing a threshold** — `--prefault`
    for gate 2, a denser corpus for gate 5 — and a **gated** run refuses rather
    than passing when its own premise fails. Ungated, gate 2 voids its evicted
    arm and prints why instead: that arm gates nothing, and the ordinary reason
    an eviction does not take is a filesystem whose pages are RAM.
    `crates/tf_tree_bench/tests/gate2.rs`
    (per-PR, `just shm-check`) and `crates/tf_tree_bench/tests/ingest_throughput.rs`
    (per-PR, `just test`) drive the shipped binaries in both directions.
  - Gate 5's grouped `--max-memory` and its density floor both come off the
    **survey**, so `--edges`/`--rate-hz`/`--seconds` describe nothing under
    `--reuse-corpus` and the run says so.

- **An SBOM per release** (`d5dd109`, `docs/PHASE5.md` §10). `scripts/sbom.py`
  writes CycloneDX 1.5 from `cargo metadata`, `just sbom <version>` produces the
  same file locally, and `release.yml`'s `github-release` job generates it,
  prints its component count, appends it to `SHA256SUMS` and uploads it. **No
  release has produced one yet**: that job is gated on a `v*` tag push and the
  newest tag predates the script.

- **`just no-network`** (`0787b9e`, `docs/PHASE5.md` §5.1 / §13 box 4) — the
  `AF_UNIX`-only assertion, under `strace -f`, over the five published crates'
  test binaries, with `tf_tree top --web`'s `AF_INET` listener as a required
  positive control. Run by `ci.yml`'s `shm` job. It refuses rather than skips.

- **`just split-brain-soak`** (`0575311`) and **`just reclaim-latency`**
  (`6e8b19b`) — `docs/PHASE2.md` §11.2 scenario 9 a thousand times, and §12.3
  criterion 4's kill-to-re-claimable measurement.

### Changed — PHASE5 §5.4's long-lived per-thread `Guard` is WITHDRAWN

- **The requirement contradicted the paragraph that justified it.** §5.4 argues
  batching over a `Guard` that *spans a batch of lookups* and then required one
  with no end of batch. `Guard::drop` is the **only** publisher of `lookups_ok`
  while `note_err` writes the error counters through by design, so a guard that
  never ends holds the denominator and publishes the numerator.
- **Two shipped checks would have inverted.** `TFT010` computes
  `errs / (errs + lookups_ok)`, so a healthy edge with a handful of
  extrapolation errors would read **100 %** and fire; `no_counter_evidence`
  skips every counter check at a zero sum, so a process hammering the tree
  would look like one that never ran. The catalogue tests perform no lookups,
  so neither would have been caught.
- **It was also unsound as specified**: a `thread_local!` needs a
  `Guard<'static>` — a second lifetime extension, which is a decision record
  (`0017`) and not a patch — and `0017`'s `Arc` soundness argument is not
  available to `Tree::lookup`, which takes `&self` on a `Send + Sync` type.
- **The cost it avoided is real but not decisive**, and is quoted from the
  registered artifact rather than by hand: `just guard-cost` reads **+38.3 ns**
  on a heap arena with counters on (203.1 hoisted -> 241.4 per call), +29.3 on a
  memfd one, and +19.1/+20.0 with counters off. An earlier revision of this
  entry said 21.7–22.9 ns and called it 4 % — that was a hand measurement over a
  **three-edge** plan, and `Guard::drop` credits an edge only when the batch went
  through exactly one, so it priced the configuration in which the guard does
  least work. Review caught it against `backing.rs`'s own summary. The
  correction does not reopen the decision: a requirement that inverts a shipped
  diagnostic is withdrawn whether it costs 4 % or 16 %.
- **What the withdrawal keeps is now pinned**: the convenience path credits its
  denominator once per call, visible before any loop ends —
  `the_convenience_path_publishes_its_denominator_on_every_call`, whose
  per-iteration assertion is the whole test and which is mutation-verified
  against one hoisted guard across the loop.
- §11's bullet, §13's box 6 and `Tree::lookup_tagged`'s own doc now agree with
  §5.4, and the standing disagreement between that box and
  [`0022`](docs/decisions/0022-the-per-call-guard-and-the-unwatched-gate.md) is
  resolved in `0022`'s favour. §11's bullet also asked for a symptom as if it
  were a property — nothing counts atomic flushes.

### Added — a gate outcome has three meanings and had two exit codes

- **`tf_tree_bench::gate`** fixes the contract a gate binary leaves with:
  `0` PASS, `1` FAIL, `2` REFUSED (not evaluated). This crate already classified
  *rows* three ways — `Fitness` (can this host produce a trustworthy number),
  `Ground` (the machine-checked claim a refusal rests on) and `Status`
  (Measured/Indicative/Unavailable) — but the only interface CI reads, the exit
  code, had no such split. `reclaim_latency` got it right and nothing else did:
  every other gate binary leaves through `anyhow`'s `Termination` path or a bare
  `exit(1)`, so **"this host cannot evaluate the criterion" and "the code
  regressed" arrive at a workflow byte-identical**.
- **A refusal is a measurement, not a literal.** Both refusal constructors quote
  the probe's own reason string and return `Option<Outcome>` — `None` means the
  host is fit and there are no grounds to refuse. Review caught the first
  version returning a refusal unconditionally, so
  `refused_on_host(&f, Sensitivity::HostIndependent)` produced
  `REFUSED (HostFitness) — ` with an **empty** reason and exit 2: under
  `may-refuse` a permanently green gate whose recorded reason is the empty
  string. The test named for that property never called the constructor, so it
  would have passed against the bug; it calls it now and fails against it.
- **`scripts/gate-run.sh` and `just gate RECIPE POLICY`** are the only place an
  exit code is interpreted, so a workflow cannot re-spell the reading and drift
  from it. Three policies, and the third is the point: **`must-refuse` fails
  when a gate that could not be evaluated suddenly can**, so a permanent refusal
  cannot quietly go vacuous — the day a criterion is re-cut or a fixture grows,
  the job goes red naming the document that still says it is unmeasurable.
  `must-pass` treats a refusal as a failure; `may-refuse` passes on one and
  emits a `::warning::`, and emits a `::notice::` when the refusal lifts.
- **The reader has a nine-cell self-test** (`--self-test`), because a policy
  runner that mis-reads one cell is invisible: every gate still prints its own
  verdict and only the job colour is wrong. It earned its place immediately —
  its first run reported six of nine cells wrong, which was its own stub not
  modelling `just --show`.
- **PHASE2 §12.3 criterion 4 is gated in CI** (`ci.yml`'s `shm` job, both matrix
  rows) under **`may-refuse`**, because `reclaim_latency`'s exit 2 is its own
  *"INVALID, not FAIL"* refusal about the runner rather than the code — and
  `must-pass` would map it to exit 1, reproducing the very collapse this module
  removes, in its first customer. It **was met and ungated for the life of the
  project**: `just
  reclaim-latency` existed, printed a verdict, and ran in no workflow, so
  nothing would have gone red if a reap regressed to a timeout. Measured here at
  200 trials, 200/200 contended, p50 0.112 ms, p99 0.159 ms in 0.2 s wall — a
  ~60x margin against the 10 ms budget. An absolute duration is gateable on a
  host that fails `Fitness::probe` because every check the probe fails makes a
  reclaim *longer*: a PASS with margin is conservative, and a FAIL is not
  attributable to the runner.

### Fixed — an ordering no model was watching

- **`PHASE1.md` §10.2's mutation test has now been run, and one of the five
  §6.2/§6.3 orderings was unguarded.** Weakening `SampleRing::push`'s
  `self.head.store(h + 1, Release)` to `Relaxed` passed the **entire** loom
  suite, while the other four each kill a model. The store is not unnecessary —
  `sample`'s `stamp_at` reads stamps `Relaxed` and rests that on this edge in
  its own doc, and `docs/design/fast-path.md` builds a proposed optimisation on
  it — so this was §10.2's *"test coverage is insufficient"* branch, unanswered
  since the models were written.
- **No `sample`-shaped fixture could have caught it**, which is why the new
  model is not a third one. `push`'s `fence(Release)` sits *before* that push's
  stamp store, so observing `head == h` orders stamps `0 ..= h-2` and leaves the
  newest unprotected — and `sample` reads exactly that one as `t_new`. A stale
  stamp reads as the zero-initialised `0`, stamps only increase, so a stale
  `t_new` is always low and every positive `t` leaves through the tolerated
  `Extrapolation` arm before reaching an assertion.
- **New loom model `head_publishes_every_stamp_below_it`** asserts the invariant
  directly: observing `head == h` makes all `h` stamps visible. Verified both
  ways — it fails under the `Relaxed` mutant and passes at `Release`. The loom
  suite is 21 models, was 20.
- **Two claims that were false are corrected rather than deleted.**
  `buffer.rs`'s module doc said "Every ordering below is load-bearing and is
  exercised by the loom tests"; it was true of four and asserted of five.
  `sample.rs`'s `stamp_at` rested its `Relaxed` load on an edge nothing checked.
  Both now name the model and record what they used to claim. The failure this
  admitted is not an error return: a reader brackets against a stamp `head` had
  not published and returns a finite, plausible, **wrong pose**, with `just
  loom`, `just test`, `just miri` and both CI architectures green.

### Fixed — gates a caller could green, and one that could not see its subject

- **`docs/PHASE5.md` §12 gates 2 and 5 refuse a loosened threshold under
  `--gate`.** `--floor` is the *whole* of gate 5's gated comparison and was
  accepted in the loosening direction, so the identical failing run exited 0 —
  `§12 gate 5 — FAIL (gated)` at exit 1 became `PASS (gated)` at exit 0 with
  `--floor 0.5` and no change to the measured ratio. `--budget-ms` is the same
  shape in gate 2, mitigated only by that verdict being a conjunction. Gate 4,
  the precedent both cite for `--gate`, has no threshold flag at all. Both now
  refuse a loosened threshold that would produce a gated PASS, *before* printing
  any verdict; tightening stays legal, and so does loosening a run that still
  FAILs, which is how `tests/gate2.rs` isolates each half.

- **Gate 2's span floor — one of its two declared anti-vacuity refusals — was
  reached by no test, no recipe and no seeded mutant**, and an ungated run over
  two fixtures too close in size printed a `GATED` line over a comparison that
  is structurally green. It has a case and a disclosure now.

- **`--reuse-corpus` deleted the corpus it exists to reuse, and the next run
  fabricated a different one at that path and labelled it "written by this
  process".** The cleanup was guarded on `--keep-corpus` alone; measured, an
  829 485 B corpus written with `--keep-corpus` was gone after one
  `--reuse-corpus` run, and the run after that reported a 31.980 s span where
  the original was 7.980 s. The cleanup now removes only what the process wrote,
  and `--reuse-corpus` on a missing path refuses — recorded as an amendment to
  `docs/decisions/0050`, since the second is a behaviour change rather than a
  repair.

- **`scripts/evidence-audit.sh` matched target names as unanchored substrings**,
  so `crates/tf_tree_bench/benches/lookup.rs` and `benches/push.rs` were
  executed by nothing, registered nowhere, and the gate was green over both —
  `lookup` excused by the recipe name `profile-lookup`, `push` by the bare
  `push:` GitHub Actions trigger key. `docs/PHASE1.md` §11.3 is NORMATIVE about
  `benches/lookup.rs`. The coverage tests now match the shapes that *execute* a
  target, the register arm requires a table ROW rather than any backticked
  mention, both benches have rows, and an empty subject set is red.

- **Host-drift detection could be retired from the producer side.**
  `runstore::diff` reports a fact only when two runs disagree, so a key absent
  from both compares equal forever — and the test written for that class pinned
  the *list* while writing each key into both synthetic runs, so it never
  consulted `Provenance::collect`. Renaming the `target` push would have made an
  x86_64 run and an aarch64 run compare as the same machine with no `HOST DRIFT`
  banner.

- **`scripts/sbom.py` dropped an unresolvable root silently** and emitted a
  zero-component CycloneDX document at exit 0; **`check_recipe_references` had
  no anti-vacuity floor** on either of its two subject sets; and **the unsafe
  budget's clippy matrix cannot see a pass written across a `\`-continued
  line**, which the justfile already contains — now reported by name rather than
  absorbed by the `MIN_SELECTORS` floor.

- **`check_recovery`'s settle loop could never take its early exit** (its
  predicate was the whole participant table, read before the exemption partition
  that is never empty), so it always spent its full 2.00 s while its comment
  sold it as a poll. It is written as the fixed window it always was, because a
  reachable exit would be the weaker check. And **`record_inheritance`'s
  documented "one `write`" was two `write(2)` calls** — `writeln!` forwards each
  format piece to `write_all` — so an `O_APPEND` claim about torn records was
  false; measured under `strace`, it is one call now.

### Fixed

- **A record body is sized against the file, not against its own header**
  (`crates/tf_tree_ingest/src/source.rs`). `read_tf` bounded a top-level MCAP
  record's declared length by `--max-record-size` and by nothing else, then
  sized the read buffer to it. A **seventeen-byte** file — the MCAP magic plus
  one record header declaring 256 MiB — allocated and memset 256 MiB before
  finding it had read nothing: measured at 265 856 KB peak RSS and 0.16 s
  through the release binary. `--max-record-size` saturates to `u64::MAX`, so
  the same seventeen bytes reached a `RawVec` "capacity overflow" panic (exit
  101) at a declared `2^63` and a `SIGABRT` at `2^47`. The buffer is clamped by
  the file's own length now, which is the comparison the two branches that do
  not allocate already made. All three files exit 1 with the truncation
  diagnosis, at 3 968 KB. **Nothing about a well-formed recording changes**: for
  a record whose body is in the file, the clamp is the declared length.

### Changed — `--max-memory`

- **The stable sort's own scratch is inside the cap now, and inside the reported
  peak.** `slice::sort_by_key` is stable because `docs/PHASE5.md` §3.2's "last
  occurrence in the recording wins" needs it to be, and a stable sort allocates
  up to a full extra copy of the buffer it sorts. That copy is a sample buffer
  like the ones `--max-memory` enumerates, and it is live while every buffer the
  pass has not drained yet is still held — but `plan_groups` packed against
  `sum(buffers) <= cap`, `spill_budget` sized a run against one copy of itself,
  and `peak_buffer_bytes` was computed before the sort ran. Measured with a
  counting allocator on `tests/memory.rs`'s fixture: at `--max-memory 1048576`
  the report read exactly 1 048 576 and the process used **2 046 121 B**.
  `plan_groups` reserves the group's largest member now, `spill_budget` divides
  by `2 * ENCODED`, and the reported peak carries the term — so the number a user
  sizes a container against is the number that was enforced.
  **It costs re-reads**, which is the trade and is stated: a group holds one edge
  fewer, an edge between `cap / 2` and `cap` takes the spill path, and a spill
  run is half as long. `docs/PHASE5.md` §3.1's new amendment carries the
  measurements, the alternative that was weighed, and what still has no
  instrument. §12 gate 5's arm boundary did **not** move — its cap is derived
  from the survey by the same rule.

### Changed — the ingest report

- **`tf_tree.ingest/1` -> `tf_tree.ingest/2`: `undecodable_channels` is split
  into `filtered_channels` and `non_cdr_channels`.** The withdrawn key was the
  **sum** of channels this build cannot decode and channels the operator's own
  `--tf-topic` excluded, so it named one of its two terms: a consumer pinning
  the schema read its own narrowing as a defect in the recording. The field's
  doc comment ("messages on a TF channel this build could not decode") was
  wrong about the unit as well — these are channels, one per channel id. The
  terminal summary was already correct and still prints one row for the pair,
  now naming both numbers. The `filtered_channels` arm had no test at all;
  `a_topic_filter_is_not_an_undecodable_channel` is it.

### Fixed

- **`docs/PHASE5.md` §12 gate 4's binary now exits non-zero on a FAIL**
  (`db9502e`). `frozen_workers.rs` printed its verdict and returned `Ok(())` on
  both branches, so `nightly.yml`'s `gate4` job could not go red. Not a shipped
  surface — `tf_tree_bench` is `publish = false` — and recorded here because a
  gate that could not fail is what the `[0.0.5]` section below calls the
  costliest kind of stale claim.

### Changed — `tf_tree doctor`

- **`TFT013` waits out a grace period, which its `docs/PHASE5.md` §6 row has
  always required and the predicate never had.** Before this, `head == 0` alone
  was the rule, so a `doctor` run at bringup — before the publishers start,
  which is the run an operator is most likely to make — reported every dynamic
  edge in the arena. No arena field was added to get a clock: the grace is
  measured against how long the arena's longest-running publisher has been
  going, `(head - 1) x median period`, which is the quantity that keeps growing
  after a ring wraps. An arena in which nothing has published at all is a
  *stated skip* rather than a verdict, because bringup and a total outage are
  the same arena and no fact in it separates them.

- **`TFT013`'s second skip stated something false about the arena it printed on,
  and there are three skips rather than two.** The sentence above — and
  `docs/PHASE5.md` §6's amendment, §0.0's skip list, `docs/RUNBOOK.md`'s row and
  the check's own doc comment, and this entry is another — described the condition as
  *nothing has published at all*. The predicate was *no dynamic edge yields a
  median period*, and a median needs two retained samples: `Capacity::history` is
  `next_pow2(ceil(rate_hz * secs))` and a ring retains `capacity - 1`, so an edge
  declared `rate_hz * secs <= 2` retains one sample for the life of the arena.
  An operator whose publisher had accepted 3 600 pushes into a two-slot ring was
  told nothing in the arena had published and sent to `TFT017`. The fact that
  separates the two was already read three lines earlier: `publish_activity`
  returns a three-valued `PublishActivity` now, and the second reason names the
  publishers that exist and the busiest edge's push count. **That reason then
  had the same shape one level down and branches now**: `doctor::median_period`
  declines a stream shorter than two samples and a non-positive median alike, so
  a ring-size remedy is false about a 512-slot ring holding one sample — every
  `doctor --attach` at bringup, and a `--from-bag` run whose recording carries
  one dated record for an edge — and about a publisher stamping one instant into
  a full ring, which is `TFT009`'s and `TFT018`'s subject. `Unmeasurable`
  carries the largest ring's retained capacity and the largest number of samples
  recovered, and each branch names only what its own arena can act on.
  Whether the grace can be *cleared* in that
  state is deliberately not decided — a **declared** rate substituted for a
  **measured** one is a §6 amendment, not a patch.

- **`TFT009` reports `not run` rather than `pass` on an arena where it judged no
  edge at all.** It was the one check in this group with no skip arm. Both of its
  halves run only over edges `interval_shape` accepted, so a four-sample stream
  holding a five-second hole at 500x its own cadence reported `pass` — beside
  `TFT008`, in the same document, skipping over the identical empty set. The
  floor is five retained samples, which is every arena for its first four pushes
  per edge, every publisher restart, and permanently any edge sized
  `rate_hz * secs <= 4`. The reason is three-valued, because
  `interval_shape` declines for three conditions whose remedies are opposite:
  too few intervals, a stamp that goes backwards (`TFT018` is that fault), and
  every retained stamp at one instant. **No verdict moves**: the edges the
  trailing-silence half names are a subset of the ones counted as judged, so the
  finding list is provably empty wherever this now skips, and
  `Report::has_error` counts findings and never statuses. A `--json` consumer
  sees `not_run` where it saw `pass` on a sparse arena. **The disclosure in
  `Meta.notes` is silenced with it**: `silence_coverage_note` says the check
  measured the gaps between retained samples and only skipped the trailing
  silence, which is false beside a `not run` line for the same id — and the two
  conditions are independent, the skip being about the arena's samples and the
  note about the clock and the source, so neither predicate could catch the
  other. It reads the check's own outcome now, which is the rule `TFT011`'s two
  disclosures already followed.

- **`TFT007` and `TFT008` no longer report `pass` about a publisher `TFT009` is
  reporting as stopped.** Every rule in the catalogue measures *between*
  retained stamps, so a full ring of evenly spaced samples from a publisher that
  died compares equal to its declared rate and has a coefficient of variation of
  ~0. `doctor --attach` printed those verdicts side by side: `TFT009` calling the
  edge dead, `TFT008` clearing it, and — wherever the topology declared a
  `rate_hz` for that edge — `TFT007` clearing it too. Neither gains a finding
  — a second warn id for one fault would inflate what `--exit-code warn` gates
  on — they **withhold** such an edge, disclose it in `notes`, and skip rather
  than pass when withholding leaves nothing judged. `TFT008` also skips, rather
  than passing, on an arena where no edge retained the intervals a spread needs
  (`doctor::SPREAD_MIN_INTERVALS`, which its skip reason quotes).

- **`--json` is schema-validated, and the schema block is now one of the
  spellings that is read.** The `tf_tree.doctor/1` document is parsed and held to
  the schema `render_json` documents, by a test that runs the real binary. That
  sentence was an overclaim when first written: the block, the `writeln!`
  emitter and the test's key literal are three spellings and only the last two
  were compared, so emitting a key and adding it to the literal left every check
  green — the block is fenced ```` ```text ````, so `cargo test --doc` does not
  reach it either. The test parses the block out of `catalogue.rs` and holds the
  literal to it, at the **top level**; the nested object shapes are still
  literals compared to nothing else, and the test's own doc says which. No schema file is shipped and the document is still written by
  hand. The id sequence is pinned against a literal rather than folded from
  `Tft::ALL`: the emitter walks that array too, so a comparison against it
  asserted membership and not order.

### Fixed — documentation

- **Public surface that shipped in 0.0.5 with no entry, and stale claims a
  pre-release audit confirmed.** All are corrected in the 0.0.5 section itself
  rather than only here, because a consumer on 0.0.5 reads that section; this
  entry records that the corrections were made after the tag. `CHANGELOG.md`
  ships in no artifact — `cargo package --list` shows none of the five carry it,
  and PyPI's long description is `README.md` — so amending it costs no version.

- **`Plan::at_extrapolating`'s rustdoc described the opposite ordering to the
  code**, and that one *did* ship, to docs.rs. It said the `newest_stamp` walk is
  taken "after the fold"; the implementation takes it **before**, and its own
  comment says why the order is a soundness guarantee — measuring after would let
  a `push` landing mid-fold report `by_ns == 0`, "not extrapolated", for a pose
  the fold invented.

- **`NOTICE` said `cargo deny check` is run by `just lint`. It is `just audit`.**
  `just lint` has never had a `deny` line in its body; CI runs both, as separate
  steps of the job named `lint`, which is where the confusion came from. This
  one matters more than its size: `NOTICE` is packed into every crates.io
  tarball, every wheel and every release archive, so a false statement about
  this repository was shipped to two indexes.

- **Two relative documentation links resolved to nothing**, and now nothing can
  add a third: `docs/PHASE1.md`'s citation of `0013` climbed one directory too
  far — `../decisions/` from inside `docs/` is the repository's parent — and
  `docs/decisions/0046` cited `0010` by a title that record does not have.
  `just artifact-versions` gained a relative-link scan over every tracked
  Markdown file, with a floor on the number of links it found, because a scan
  that silently stops matching reports every document as clean.

- **Sites across the tree still priced the residual FFI boundary at the
  withdrawn `~21 ns / 8%`.** `docs/benchmarks/tf2.md` replaced that figure with
  45.3 ns / 10% — 498.2 ns through the binding against 452.9 ns native — for
  having no derivation recorded anywhere. The first sweep enumerated the sites
  it had corrected, which is what let `docker/tf2/native_ratio.cpp` — a file
  that *produces* one of the two rows the corrected figure is derived from —
  keep citing the document that withdrew it; `grep -rn '21 ns'` is the
  enumeration, and no list of sites is written down anywhere. The gate does not
  move with it: `FLOOR` is bounded by an estimate with no binding in either
  half.

### Fixed — gates and release wiring

- **`just embed-cost`'s new structural self-check ships with a disclosed escape,
  `EMBED_COST_KNOWN_COLLAPSED=1`, which CI's `bench-gate` job sets.** The check
  asserts that `PHASE5.md` §9.2's `embedding_cross_crate` row still has an
  independent variable — that its two columns compile to different bodies — and
  it is red on this tree, on a defect in the code under test that predates it:
  since 2026-08-29 `Plan::at_tagged` has sat between `Plan::at` and the fold
  with no `#[inline]`, so both columns are the same call stub and the row's
  quotient is 1.0 by construction. Repairing that is a trade (`docs/API.md`
  §2.3's 2026-09-06 amendment prices it) and not a CI decision, so the escape
  keeps `bench-check` and `bench-baseline-update` runnable instead of leaving a
  required job permanently red — which is how a check gets deleted rather than
  answered. **It suppresses nothing**: the full diagnosis prints on every run
  and is followed by a line saying the run's quotient is not a measurement. It
  is deleted by the commit that restores the row's variable.

- **The same recipe failed for anyone who exports `CARGO_TARGET_DIR`.** It
  looked for the two `embed_cost` binaries under a hard-coded `./target/`, so
  the symbol lookup refused for the environment rather than for its subject.
  It reads `${CARGO_TARGET_DIR:-target}` now, as `just sbom` and
  `just release-archive` already do.

- **`scripts/sbom.py` had never executed on any path, and no release carries an
  SBOM.** Its one caller is `release.yml`'s `github-release` job, gated on
  `refs/tags/v*`, and the commit that added the step (`d5dd109`) is not an
  ancestor of `v0.0.5` — so the step has never run, and `v0.0.5`'s assets are
  four archives and `SHA256SUMS`. `just lint` now depends on `sbom`, which costs
  one `cargo metadata` and no compile, so the generator is exercised before a
  release rather than during one. `docs/PHASE5.md` §0.0's §10 row is corrected
  in both directions: the SBOM is *generated on demand and attached by wiring
  that has never fired*, and its dev-graph exclusion is **derived by
  construction, not asserted** — no assertion phrased over the same graph and
  the same `dep_kinds` rule could fail.

- **`just sbom` failed outright for anyone who exports `CARGO_TARGET_DIR`**
  (`FileNotFoundError`, exit 1). It writes under `${CARGO_TARGET_DIR:-target}`
  now, and its `VERSION` defaults to the workspace number through the same
  `cargo pkgid` idiom `release-archive` uses, so `just sbom` with no argument is
  the check and `just sbom <tag> <path>` is what a release passes.

- **`wheels.yml`'s licence check passed on an empty subject set.** If `PKG-INFO`
  declared no `License-File:` header it printed "no License-File headers to
  check" and exited 0 — green in exactly the state it exists to prevent, an
  sdist that declares no licence file and is therefore required to carry none.
  It now requires at least one header. Red-tested on three shapes: declared and
  present (0), declared and missing (1), none declared (1, and 0 before).

- **`just artifact-versions` reads the tracked lockfiles.**
  `crates/tf_tree_tf2_sys/Cargo.lock` recorded `0.0.1` four releases on, held by
  nothing: cargo rewrites a lock only where somebody builds that crate, and that
  one builds only in the ROS 2 container. Entries are filtered to the package
  names read out of this repository's own manifests *and* to those with no
  `source` key, so a third-party version is never read as one of ours. The set
  of lockfiles is `git ls-files '*Cargo.lock'` rather than a list in the script:
  a hand-kept list is how an artifact nobody thought to add drifts for four
  releases, which is the defect being fixed. Each lockfile joins the coverage
  assertion, so a lock that stops carrying our packages fails rather than
  passing on nothing. **Nothing else reports this drift** — a stale lock entry
  for a path dependency is silently relocked by the next build that resolves,
  and no invocation here passes `--locked` to either excluded crate.

- **The version-in-prose rule covered five of the project's six package-index
  front pages.** It reads each publishable crate's `[package] readme` — the
  pages crates.io renders — and `pyproject.toml`'s `[project] readme` names the
  **root** `README.md`, which is what PyPI renders for `transform_tree`. The
  page a Python user lands on was the uncovered one. Zero findings today; the
  value is the same regression guard the other five are, and #236's lesson stops
  depending on anybody re-reading it.

### Added — decision records

- [`0051`](docs/decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md)
  (`ready`) — **no per-file licence headers.** §10's three-word `license
  headers` clause was neither done nor declined, which is why the §0.0 row
  enumerating what remains had omitted it. The obligation is Apache-2.0 §4(a)'s
  and it is on the artifact, asserted at all three distribution surfaces; a pass
  would rewrite every source file in the repository, and a header nothing gates
  drifts. REUSE is named as the shape a future record would take.

- [`0052`](docs/decisions/0052-the-first-five-minutes-nobody-runs.md) (`draft`)
  — **the mdBook site and the path it is supposed to open with are one
  question.** §10 asks for `pip install transform_tree`, three lines, a real
  result; what runs on every pull request is the README's snippet under a
  from-source `just quickstart`, and nothing anywhere installs the published
  distribution and imports it. The record's first open question is *where such a
  smoke would run*, because putting it in `wheels.yml` would repeat the SBOM
  defect this same change fixed.

- **`PHASE2.md` §10's recorder is declined rather than pending**
  (`docs/decisions/0047-the-recording-this-reader-would-refuse.md`, now `implemented` — the plan had landed and the status line had not).
  There is no `tf_tree_record` crate and none is owed: §10's own MCAP channels
  carry no `tf2_msgs` schema, and `tf_tree_ingest` accepts a channel only by
  schema, so a recorder built as specified would emit a bag the only reader here
  refuses. §10(c) — the NORMATIVE heap-against-mapped bit-identity test — is met
  and is the half that shipped. Six prose sites carried the recorder, the `/tf`
  bridge or the fault harness as still owed after each had moved, across
  `PHASE2.md`, `PROJECT.md` and `docs/benchmarks/tf2.md`; all are corrected in
  place with what they used to say.

- **`crates/tf_tree_cli/tests/replay_bit_identity.rs` stops claiming a round
  trip it does not perform.** Its module doc said the recording is *"written to
  MCAP and read back"* and an inline comment said the messages had been
  *"serialised and parsed"*; the test writes the file, asserts it exists, and
  replays the in-memory fixture into both arenas, importing no reader. The
  assertion §10 asks for is unaffected — it is about two read paths, not about
  serialisation. `PHASE2.md` §15's box recorded this correction on 2026-09-05
  and deferred the code; this is the code. The same box now also names where its
  evidence runs (`just shm-check`, and the CI step that invokes it — **not**
  `just test`, which never compiles the `shm` feature).

### Changed — `tf_tree doctor` (2)

- **`TFT016`'s finding stops predicting a call it cannot predict, and names the
  flag that does not undo `0024`**
  (`docs/decisions/0049-the-flag-that-prefaults-the-arena.md`, now `implemented` — same wave, same omission). Its
  detection rule and severity are unchanged. Two things in the *message* were
  wrong. It recommended `mlockall(MCL_CURRENT|MCL_FUTURE)`, which — measured by
  the new `mlock_probe` example — takes an untouched 64 MiB `memfd` mapping from
  `Rss` 0 to 65 536 kB the moment it is issued, and prefaults mappings made
  after the call as well: per-arena population at address-space scope, which is
  what `0024` removed at 5.2×. And it said the call "will fail", which this
  check cannot know: `mlockall` charges the whole address space and the check
  compares a limit against the *arena*, so the call returns `ENOMEM` at limits
  well above a small arena while the check is silent. The finding now names
  `MCL_ONFAULT` and says outright that its silence is not a clearance. The same
  correction lands in `hostfacts.rs`'s module doc, `docs/API.md` §8.3,
  `docs/PHASE2.md` §0.0 and §7.4, and `docs/PHASE5.md` §6 — where the `TFT016`
  row also stops naming `getrlimit`, which this crate has never called.

### Added — evidence

- **`crates/tf_tree_bench/examples/mlock_probe.rs`**, registered as a probe in
  `docs/benchmarks/EVIDENCE.md`. `docs/API.md` §8.3 asserted two syscall
  behaviours and reproduced no probe, against `PHASE2.md`'s own preamble rule;
  one of them was wrong and one was a reclaim-*policy* conclusion written in the
  grammar of a mechanism *fact*, and neither could be doubted without a C
  compiler and an afternoon. Eight arms, each `mlockall` arm in its own process
  because the call is process-wide, plus two organic-memory-pressure arms with a
  file-backed positive control. It is a cargo example rather than a fenced block
  in an appendix on purpose: `scripts/evidence-audit.sh` takes its subject set
  from `cargo metadata`'s bin/example/bench targets, so a markdown fence could
  never have been audited at all.

### Added — the unsafe budget has a gate

- **`docs/decisions/0007` rule 1 had no enforcement of any kind**
  (`docs/decisions/0048-a-kind-is-not-a-crate-name.md`, now `ready`; D1-D6 are in force and gated, and its step 4 is the outstanding work) — no script, no
  recipe, no CI step, no lint, and the root `[workspace.lints.rust]` does not
  name `unsafe_code`. `scripts/unsafe-budget.sh` is the first: a **compiler**
  census (`RUSTFLAGS="--force-warn unsafe_code"`, which overrides
  `#![forbid(unsafe_code)]` rather than being suppressed by it), taken over a
  matrix read out of the justfile's own `cargo clippy … --all-targets` lines so
  it cannot drift from them, compared against `scripts/unsafe-budget.txt` in both
  directions. `just lint` depends on it. It pins a **file set**, not a kind —
  the lint's output carries none — and its header says so.
- **What the census found is not a fifth boundary.** `0007` rule 1 wrote a crate
  name in brackets beside each kind and every downstream reader copied the
  bracket, so three of the four existing kinds had been occurring in crates the
  brackets do not name — for months, with every recipe green. `0048` makes the
  kinds properties and moves the names into the register, widens kind 3 to *"a
  foreign runtime **or library**"*, and admits two new ones: our own C ABI called
  from Rust to exercise it, and a trait the language requires be implemented
  unsafely in a target that never ships.
- **The budget binds a crate ROOT, not a package.** `#![forbid(unsafe_code)]` on
  a `src/lib.rs` governs no bin, test, bench or example of the same package.
  Several claims in this repository rested on the other reading and are corrected
  in place, each keeping what it used to say — including `docs/PHASE2.md` §0.0's
  §11.4 row, which deferred an invariant on it. Every one of those design choices
  survives on `0007`'s rule; none of them survives on the attribute.

### Changed — `tf_tree_c`

- **Four safe `blank()` constructors replace every
  `unsafe { core::mem::zeroed() }` in this crate's tests and examples and in
  `tf_tree_bench`'s bins.** `tft_error::blank()` and `tft_bridge_outcome::blank()`
  existed privately; `tft_extrapolated::blank()` and `tft_bridge_stats::blank()`
  are new. **`tft_bridge_outcome::blank()` fills its five string fields with the
  static empty string, not NULL** — a `ptr::null()` twin would have put two
  contradictory blanks in one crate and handed a C consumer a null where the
  convention is a valid empty string. Three hand-rolled zeroing helpers in the
  test and bench crates are deleted with them. No `extern "C"` symbol changes.

## Released versions

Each release's entry is frozen history in its own file under
[`docs/changelog/`](./docs/changelog/) — newest first. **That directory is a
record, not a working document: leave it out of searches and sweeps.** The
release commit dates the `## [X.Y.Z]` section here, and the next release
commit moves it there.

- [`0.0.5`](./docs/changelog/0.0.5.md) — 2026-08-29 (the entry paths that need no toolchain)
- [`0.0.4`](./docs/changelog/0.0.4.md) — 2026-08-22 (the slot a killed participant keeps)
- [`0.0.3`](./docs/changelog/0.0.3.md) — 2026-08-19 (first with a source distribution)
- [`0.0.2`](./docs/changelog/0.0.2.md) — 2026-08-17 (wheels, no sdist)
- [`0.0.1`](./docs/changelog/0.0.1.md) — 2026-08-17 (crates.io only)

[0.0.1]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.1
[0.0.2]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.2
[0.0.3]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.3
[0.0.4]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.4
[0.0.5]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.5
