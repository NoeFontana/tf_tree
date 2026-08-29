//! `Tree` and `Plan` (`docs/PHASE3.md` §4).

use numpy::{PyArray1, PyArray2, PyArray3, PyArrayMethods, PyUntypedArrayMethods};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tf_tree::{
    Capacity, EdgeCfg, ExtrapPolicy, InterpPolicy, Layout, OwnedWriter, Stamp, SystemDomain, Tree,
};

#[cfg(target_os = "linux")]
use crate::errors::open_err;
#[cfg(target_os = "linux")]
use tf_tree::AttachMode;

use crate::errors::{
    build_err, claim_err, edge_label_of, lookup_err, plan_domain_err, push_err, push_msg,
    resolve_frame, unknown_frame_err, BufferError, TfTreeError,
};

/// Releasing the GIL costs a measured 40 ns; a depth-3 lookup costs ~193 ns
/// (`docs/PHASE3.md` §6.1's amendment — the re-baseline, not §2's superseded
/// ~150 ns). So releasing for a scalar would add ~21% for parallelism nobody can
/// use inside a 193 ns window, and *not* releasing for a large batch would stall
/// other threads.
///
/// The rule is expressed in estimated work rather than element count, because
/// depth varies. Below the threshold the estimated worst case is just under
/// 1 µs of GIL retention — three orders of magnitude below CPython's 5 ms switch
/// interval, so no other thread notices. Above it the worst-case overhead is
/// 40 ns against ≥1 µs, so ≤4%. **Both sides are cheap, which is why the exact
/// constant does not need tuning**; what matters is that neither branch is ever
/// badly wrong.
pub(crate) const GIL_RELEASE_THRESHOLD_NS: u64 = 1_000;
/// Rough per-step cost used only to place the threshold above.
///
/// **64 ns/step, re-derived from a measurement rather than from a budget.**
/// `docs/PHASE3.md` §6.1's amendment is the single account of where it comes
/// from and what it moved; in one line, it is the median of nine pinned
/// `benches/lookup.rs` runs of `lookup/depth3/sclerp` at the interpolating stamp
/// `fixture::QUERY_NS` — 192.7 ns over three dynamic steps — taken in the commit
/// that re-baselined that benchmark (`docs/decisions/0013`). The 55 it replaces
/// came from `docs/PHASE1.md` §11.3's 150 ns *budget*, by way of a benchmark
/// that queried on-grid stamps and never ran the interpolator.
const NS_PER_STEP_ESTIMATE: u64 = 64;

/// §6.1's rule, in one place so the two callers cannot drift apart.
///
/// # Why `Layout::QuatTwist` does not get a multiplier here
///
/// A twist row *is* more work per element than a pose row: it folds through
/// `fold_at_with_derivatives`, which samples each edge's bracketing segment for
/// a derivative as well as a pose. So `est` under-estimates a twist batch — but
/// **it already under-estimates a pose batch by nearly as much**, and that is
/// what settles the question.
///
/// Depth 3, n = 4000, `ScLerp` (the only policy that answers a twist at all),
/// release build, pinned: **328 ns/elem** for `layout="quat"` against
/// **369 ns/elem** for `layout="quat_twist"`, best of five within a process.
/// Treat both as indicative rather than as a gate — the host was **not quiet**
/// and one of three repetitions was discarded as polluted; what they establish
/// is the *magnitude*, which is what this decision turns on. They are Python-side
/// numbers and were not re-taken by the re-baseline below; the engine work they
/// contain is the same fold it re-derived the constant from.
///
/// Against [`NS_PER_STEP_ESTIMATE`], which predicts 3 × 64 = 192 ns/elem:
///
/// | layout | measured ns/elem | `est` says | ratio |
/// | --- | --- | --- | --- |
/// | `"quat"` | 328 | 192 | **1.7×** |
/// | `"quat_twist"` | 369 | 192 | **1.9×** |
///
/// So the twist adds ~1.1× on top of an under-estimate the constant already
/// carries on the pose row, and correcting only the twist would correct the
/// smaller of the two errors while leaving the larger.
///
/// **That residual is expected and is not a second miscalibration.** `est` is a
/// per-*step* estimate of the engine's fold, re-derived from the interpolating
/// depth-3 lookup (`docs/PHASE3.md` §6.1's amendment — the one place this
/// arithmetic is written down). A batch element through Python is that fold
/// *plus* the layout write into the caller's buffer, so it costs more, and the
/// error is in the safe direction: `est` too low releases the GIL later, never
/// sooner. The largest batch that does *not* release at depth 3 is `n = 5`
/// (`est` = 960 ns), which at the rates above really costs ~1.6 µs for a pose
/// batch and ~1.8 µs for a twist one — still three orders of magnitude under
/// CPython's 5 ms switch interval, which is §6.1's own criterion and the reason
/// a layout multiplier would buy nothing.
#[inline]
const fn release_the_gil(n: usize, depth: usize) -> bool {
    let est = (n as u64)
        .saturating_mul(depth as u64)
        .saturating_mul(NS_PER_STEP_ESTIMATE);
    est >= GIL_RELEASE_THRESHOLD_NS
}

/// The depth-3 crossover, pinned at compile time.
///
/// `docs/PHASE3.md` §6.1 states where the release begins for the depth the whole
/// design is anchored to, and a constant re-derived from a re-baselined
/// benchmark is exactly the kind of number that moves without anyone re-reading
/// the sentence describing it. This is that sentence, checked by the compiler:
/// at 64 ns/step a depth-3 batch releases from `n = 6` and not at `n = 5`.
///
/// It is an assertion rather than a `#[test]` on purpose — `tf_tree_py` is
/// outside the cargo workspace, so `cargo nextest run --workspace` never sees
/// its test targets, while *every* build of the crate (`just py-test`,
/// `just py-lint`, the wheel jobs) evaluates this.
///
/// Mutant (applied, confirmed fatal): restore `NS_PER_STEP_ESTIMATE = 55` —
/// `error[E0080]: evaluation panicked: the depth-3 GIL crossover moved`.
const _: () = assert!(
    release_the_gil(6, 3) && !release_the_gil(5, 3),
    "the depth-3 GIL crossover moved; docs/PHASE3.md §6.1 says n = 6"
);

/// Parse the `layout=` keyword into the core's [`Layout`].
///
/// **No default and no inference** (`docs/API.md` R4): row-major versus
/// column-major differ by a transpose, which for a rotation is its inverse, and
/// `wxyz` versus `xyzw` is a different, still-unit quaternion. Both produce a
/// valid-looking transform pointing the wrong way, so an unrecognised spelling
/// is refused rather than guessed at.
fn layout_from_str(name: &str) -> PyResult<Layout> {
    match name {
        "mat4" => Ok(Layout::Mat4),
        "quat" => Ok(Layout::Quat),
        "affine32" => Ok(Layout::Affine32),
        "quat_twist" => Ok(Layout::QuatTwist),
        other => Err(PyValueError::new_err(format!(
            "unknown layout {other:?}; expected one of 'mat4' (N, 4, 4) float64, \
             'quat' (N, 7) float64, 'affine32' (N, 12) float32, 'quat_twist' \
             (N, 13) float64"
        ))),
    }
}

/// Parse the `policy` argument of [`PyPlan::at_extrapolating`] into the core's
/// [`ExtrapPolicy`].
///
/// **Strings, not the four integer constants `0038` exported for domains**, and
/// the two cases are genuinely different. A domain tag is an *open* set — the
/// trait invites a driver to declare its own from `4` up — so a closed
/// vocabulary there would have to leave a hole. `ExtrapPolicy` is a closed set
/// this crate dispatches on, so the vocabulary is complete by construction, and
/// a string keyword is what this binding already uses for every other closed
/// choice (`layout=`, `interp=`).
///
/// No default and no inference, for `layout_from_str`'s reason one axis over:
/// the three policies differ in what the answer *is*, not in how it is
/// written, so an unrecognised spelling is refused rather than guessed at.
fn extrap_from_str(name: &str) -> PyResult<ExtrapPolicy> {
    match name {
        "error" => Ok(ExtrapPolicy::Error),
        "hold" => Ok(ExtrapPolicy::Hold),
        "constant_twist" => Ok(ExtrapPolicy::ConstantTwist),
        other => Err(PyValueError::new_err(format!(
            "unknown extrapolation policy {other:?}; expected 'error' (refuse, \
             what at() does), 'hold' (the newest pose) or 'constant_twist' \
             (extend the screw the two newest samples imply)"
        ))),
    }
}

/// Write one extrapolated pose in `layout`, refusing the twist-carrying one.
///
/// **`QuatTwist` is refused, and the C ABI refuses it for the same reason.**
/// There is no extrapolating `at_with_derivatives`, so a twist beside an
/// extrapolated pose would be computed under `ExtrapPolicy::Error` while the
/// pose beside it was computed under the caller's — two policies in one
/// 13-`f64` row, which is not a row anybody can interpret.
fn write_pose_unchecked(pose: &tf_tree::Iso3, layout: Layout, dst: &mut [f64]) {
    match layout {
        Layout::Quat => tf_tree::write_quat(pose, dst),
        // `write_extrapolated` has already refused everything else, and it is
        // called once per *call* rather than once per element — this runs inside
        // the fold, under `detach`, where a `PyResult` cannot be constructed.
        _ => tf_tree::write_mat4(pose, dst),
    }
}

fn write_extrapolated(pose: &tf_tree::Iso3, layout: Layout, dst: &mut [f64]) -> PyResult<()> {
    match layout {
        Layout::Mat4 => tf_tree::write_mat4(pose, dst),
        Layout::Quat => tf_tree::write_quat(pose, dst),
        other => {
            return Err(PyValueError::new_err(format!(
                "layout {other:?} cannot carry an extrapolated pose: an f32 layout \
                 needs at_extrapolating's f32 sibling, and a twist layout would pair \
                 a twist computed under 'error' with a pose computed under your \
                 policy. Use 'mat4' or 'quat'."
            )))
        }
    }
    Ok(())
}

/// A transform tree.
#[pyclass(name = "Tree", module = "tf_tree", frozen)]
pub struct PyTree {
    /// The engine, behind the `Arc` [`tf_tree::Tree::claim_owned`] requires.
    ///
    /// `Arc<Tree>` rather than `Tree` because that is the receiver type of
    /// `claim_owned` (`self: &Arc<Tree>`), and [`PyPublisher`] is the one thing
    /// here that has to outlive the scope it was created in. It is also the
    /// embedding idiom the facade documents (`docs/API.md` §2.2), spelled here
    /// alongside — not instead of — the `Py<PyTree>` refcount [`PyPlan`] holds:
    /// the `Arc` keeps the *arena* alive, the `Py` keeps the Python object
    /// alive, and only the first of those is what a claim points into.
    ///
    /// # Hot-path cost
    ///
    /// This field was a plain `Tree`, so **every** read entry point — `at`,
    /// `at_into`, `latest`, `adaptive`, `lookup`, `edges` — now takes one extra
    /// dependent load before `guard()`, where before the `Tree` was inline in
    /// the pyclass. **Not measured**, and it is the same trade `tf_tree_c`'s
    /// `TreeShare` records for the same reason: a pointer chase into an
    /// allocation that is warm — `PyPlan::tree`
    /// dereferences the `Py<PyTree>` in the instruction before — against a call
    /// that then does a seqlock read and a depth-N fold. If it is ever worth a
    /// number, `at`'s scalar `mat4` path is where to take it.
    pub(crate) inner: Arc<Tree>,
}

/// Whether this plan samples anything at evaluation time.
///
/// `Plan::has_dynamic` is private to `tf_tree_core`, and it is the condition
/// `Plan::check_domain_tag` runs on. `docs/decisions/0038` §4 is explicit that
/// the check *moves* to plan time without changing when it fires, so this
/// reproduces the predicate from the public `steps()` rather than approximating
/// it with `Plan::domain() != domain` alone. The difference is a whole class of
/// plan: an all-static path has no domain of its own — `Plan::domain()` answers
/// `0` for one, and the core folds it without consulting a clock — so refusing
/// `plan("a", "b", domain=SIM_DOMAIN)` over one would be a *new* refusal
/// smuggled in beside an earlier one.
fn samples_anything(plan: &tf_tree::Plan) -> bool {
    plan.steps()
        .iter()
        .any(|s| matches!(s, tf_tree::Step::Dyn { .. }))
}

/// Reject a `float` stamp with the measurement that justifies it (§3).
///
/// This is the most-questioned decision in the API, so the exception carries
/// the number rather than an opinion: users argue with rules and accept
/// measurements.
fn stamp_from_any(obj: &Bound<'_, PyAny>) -> PyResult<i64> {
    if obj.is_instance_of::<pyo3::types::PyFloat>() {
        return Err(PyTypeError::new_err(
            "stamps are integer nanoseconds, not float seconds. At a 2026 epoch \
             the ULP of float64 seconds is 238 ns, so every interval in a 1 kHz \
             stream is wrong after a round trip. Use tf_tree.from_sec(x) if you \
             genuinely have float seconds and accept the loss.",
        ));
    }
    obj.extract::<i64>()
}

#[pymethods]
impl PyTree {
    /// Compile a plan from `source` to `target`.
    ///
    /// Compiling once and reusing is the whole point: the path walk and the
    /// per-edge metadata lookup happen here, not per sample.
    ///
    /// # `domain=`
    ///
    /// The time domain the *queries* on this plan will be in
    /// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`). `tf_tree::Domain`
    /// is an open trait and its tag is a `const`, so Python cannot name the type
    /// a Rust caller would instantiate; it carries the tag as data instead, and
    /// [`SYSTEM_DOMAIN`](crate::SYSTEM_DOMAIN) and its three siblings are the
    /// names for the four built-in tags. A user-declared domain is just the
    /// integer it chose, from `4` up (`docs/API.md` §2.5).
    ///
    /// **The default stays `0` rather than the plan's own domain**, which is the
    /// tempting one-line version and is wrong: it would make a mistaken caller
    /// silently correct too, deleting the check for the population D9 exists to
    /// protect (`0038`'s *Rationale*). On a sim or sensor arena the default is
    /// wrong, and loudly so — which is the point.
    ///
    /// **Not [`open_arena`]'s `domain=`.** That one is the `u32` *rendezvous*
    /// domain — which arena to attach to, `$ROS_DOMAIN_ID`'s analogue — and this
    /// one is the `u8` time-domain tag of the edges inside it. They are
    /// unrelated numbers that share a word; the arena's own docs call the first
    /// a namespace and `docs/API.md` R3 calls the second a clock.
    ///
    /// # Checked here, not per query
    ///
    /// A mismatch is refused at plan time, with both frame names still in hand,
    /// rather than on every `at()` in the hot loop — a domain is a property of a
    /// route through the tree, not of an instant, so it cannot legitimately vary
    /// between two queries on one plan. The core still re-checks on every call
    /// (`0038` §4: the check moves, it does not disappear); nothing here is an
    /// "I already checked" fast path.
    #[pyo3(signature = (target, source, /, *, domain = 0))]
    fn plan(slf: &Bound<'_, PyTree>, target: &str, source: &str, domain: u8) -> PyResult<PyPlan> {
        let this = slf.get();
        // [`resolve_frame`], not `Tree::frame`: compiling a plan is a read, and
        // `Tree::frame` interns. A typo here used to declare the typo — see that
        // function for what that costs on an arena with headroom.
        let t = resolve_frame(&this.inner, target)?;
        let s = resolve_frame(&this.inner, source)?;
        let plan = this
            .inner
            .plan(t, s)
            .map_err(|e| lookup_err(&this.inner, e))?;
        // The one place a domain disagreement is cheap to report *and* nameable:
        // `target` and `source` are still strings here. Guarded on
        // [`samples_anything`] so this fires exactly where the core's own
        // `check_domain_tag` fires and not one call earlier.
        if samples_anything(&plan) && plan.domain() != domain {
            return Err(plan_domain_err(target, source, plan.domain(), domain));
        }
        Ok(PyPlan {
            plan: Box::new(plan),
            domain,
            // The refcount that makes the borrow real.
            tree: slf.clone().unbind(),
        })
    }

    /// Claim `child`'s edge and return a publisher for it (§4.3).
    ///
    /// Argument order is **(child, parent)**, matching `Tree::claim`. An edge
    /// names the frame it moves, and reversing it silently would make a
    /// reversed transform a runtime mystery rather than an import-time error.
    ///
    /// Use it as a context manager; the claim is released on exit.
    #[pyo3(signature = (child, parent, /))]
    fn publisher(slf: &Bound<'_, PyTree>, child: &str, parent: &str) -> PyResult<PyPublisher> {
        let this = slf.get();
        // Read-only resolution even on this, the one *write* entry point of the
        // three: topology is builder-time (decision `0004`), so a name with no
        // record has no edge either and interning it could not produce one — it
        // would spend a frame slot to reach the same refusal one call later.
        let c = resolve_frame(&this.inner, child)?;
        let p = resolve_frame(&this.inner, parent)?;
        // `claim_owned`, not `claim`: this crate no longer extends a lifetime
        // itself. `docs/decisions/0017` step 6 — the writer that comes back owns
        // its `Arc<Tree>`, so the arena outlives it by construction and the
        // `unsafe` that used to live here is the facade's single reviewed one.
        let writer = this
            .inner
            .claim_owned(c, p)
            .map_err(|e| claim_err(&this.inner, parent, child, e))?;

        Ok(PyPublisher {
            edge: edge_label_of(parent, child),
            inner: Mutex::new(Some(writer)),
        })
    }

    /// Write this tree to `path` as a frozen `.tft` (`docs/PHASE5.md` §2.3).
    ///
    /// The replacement of `path` is atomic — the bytes go to a sibling
    /// temporary and are renamed over it — so an interrupted freeze leaves the
    /// previous index intact rather than a half-written one under the name
    /// somebody will open next week.
    ///
    /// `source` is the recording these poses came from, and is recorded in the
    /// manifest as `null` when there is none.
    ///
    /// `path` is any `os.PathLike`, and **the GIL is released for the copy** —
    /// see [`freeze_impl`](crate::offline::freeze_impl) for why that is not
    /// optional at the sizes a freeze is for.
    #[pyo3(signature = (path, /, *, source = None))]
    fn freeze(&self, py: Python<'_>, path: PathBuf, source: Option<&str>) -> PyResult<()> {
        crate::offline::freeze_impl(py, &self.inner, &path, source)
    }

    /// The interval over which `tree.plan(target, source)` is answerable.
    ///
    /// `(t0, t1)` in nanoseconds, or `None` when every step on the path is
    /// static and the plan therefore answers at any stamp. See
    /// [`span_impl`](crate::offline::span_impl) for the three cases and why an
    /// empty intersection is returned rather than raised.
    #[pyo3(signature = (target, source, /))]
    fn span(&self, target: &str, source: &str) -> PyResult<Option<(i64, i64)>> {
        crate::offline::span_impl(&self.inner, target, source)
    }

    /// The frame names on this tree, in declaration order (§4.4).
    ///
    /// A notebook user's first question about an arena is "what is in it", and
    /// the only answer today is to shell out to the CLI. This is a tier-1 call
    /// — one list, once, at import frequency — so R2's "the hot tier never
    /// allocates" is not in tension with the `String`s it builds.
    ///
    /// See [`frames_impl`](crate::offline::frames_impl) for what the list
    /// promises on a *live* arena, why a slot mid-intern is skipped, and why a
    /// tree inherited across a `fork()` raises instead of answering `[]`.
    fn frames(&self) -> PyResult<Vec<String>> {
        crate::offline::frames_impl(&self.inner)
    }

    /// The edges on this tree as `(parent, child)` name pairs (§4.4).
    ///
    /// **Names only.** No rate, no jitter, no gaps, no sample count: that is
    /// `docs/PHASE5.md` §4.2's `ds.edges()`, which stays held back until §3's
    /// counting pass exists, because a rate computed from what a ring *retained*
    /// answers a different question than its name promises.
    ///
    /// `(parent, child)` is `tf_tree.build`'s order, so the list can be handed
    /// straight back to it — but that rebuilds the *graph* only: this list does
    /// not report an edge's kind and `tf_tree.build` cannot declare a static
    /// edge, so a static edge comes back dynamic and empty. See
    /// [`edges_impl`](crate::offline::edges_impl), which also covers the one
    /// case where the pair and the live topology can disagree.
    fn edges(&self) -> PyResult<Vec<(String, String)>> {
        crate::offline::edges_impl(&self.inner)
    }

    /// Whether this tree's arena is shared with other processes.
    fn is_shared(&self) -> bool {
        self.inner.is_shared()
    }

    /// Whether this process may publish into this tree.
    fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }

    /// Which arena instance this tree is attached to, as 32 hex characters.
    ///
    /// All-zero for an in-process tree. Two processes that resolved the same
    /// name can still hold *different* segments if the owner was replaced
    /// between their `open()` calls; this is what tells them apart, and
    /// comparing names cannot.
    ///
    /// # Errors
    ///
    /// [`detached_err`](crate::errors::detached_err) on a tree inherited across
    /// a `fork()`. `Tree::instance_uuid` is `self.view().header().instance_uuid`
    /// and [`Tree::view`](tf_tree::Tree) substitutes the zeroed poison arena for
    /// a detached tree, so answering would report the one arena identity whose
    /// entire job is to be comparable as *all-zero* — the spelling this binding
    /// documents as "in-process". Two peers debugging a split brain would
    /// conclude they were never shared. Same refusal as
    /// [`frames`](Self::frames), for the same reason: a walk of the view names
    /// the fork rather than describing the poison arena.
    fn instance_uuid(&self) -> PyResult<String> {
        if self.inner.detached() {
            return Err(crate::errors::detached_err());
        }
        Ok(self.uuid_hex())
    }

    fn __repr__(&self) -> String {
        // **The one accessor that describes a detached tree instead of
        // refusing, and deliberately.** A `__repr__` that raises breaks
        // `print`, the REPL echo and every debugger pane — precisely where a
        // fork victim is standing when they need to be told. So it does not
        // raise; it says the word. `is_shared` and `is_writable` read the
        // backing rather than the view, so they stay true either way.
        let instance = if self.inner.detached() {
            " detached-by-fork".to_string()
        } else {
            let uuid = self.uuid_hex();
            // Show the instance only when there is one. An all-zero field on an
            // in-process tree is noise that reads like a bug.
            if uuid.chars().all(|c| c == '0') {
                String::new()
            } else {
                format!(" instance={}", &uuid[..8])
            }
        };
        // `True`/`False`, not Rust's `true`/`false`. A repr is read by a Python
        // programmer, and lowercase booleans there look like a stringly-typed
        // field rather than a bool.
        format!(
            "<tf_tree.Tree shared={} writable={}{instance}>",
            py_bool(self.inner.is_shared()),
            py_bool(self.inner.is_writable())
        )
    }

    /// One transform, without compiling a plan first (§4.2).
    ///
    /// The plan is cached per **thread**, keyed on
    /// `(arena, target, source, topology generation)` — a shared cache behind a
    /// lock would turn the convenience API into a contention point on exactly
    /// the workload free-threading exists to serve (§7.2).
    ///
    /// The `arena` component is why two `tf_tree.Tree` objects on one thread do
    /// not answer for each other (issue #196). This binding calls the Rust
    /// facade's `Tree::lookup` and keeps no cache of its own, so it inherited
    /// that defect and inherits the fix with no change on this side.
    ///
    /// Prefer `tree.plan(...)` in a loop: this pays a cache probe per call, and
    /// a plan compiled once pays nothing.
    ///
    /// # `domain=`
    ///
    /// [`PyTree::plan`]'s, with the same default and the same meaning, and
    /// keyword-only for the same reason. It is here because this entry point
    /// hard-coded tag `0` exactly as the plan path did, and a convenience tier
    /// that cannot reach a sim arena is a second half of the same defect
    /// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`) — not a
    /// smaller one, since this is the tier a notebook reaches for first.
    ///
    /// The check cannot move to plan time here: there is no handle to hang it
    /// on, the plan being cached rather than returned. So it stays where the
    /// core does it, per call, and the refusal names two tags rather than a
    /// route.
    #[pyo3(signature = (target, source, stamp_ns, /, *, domain = 0))]
    fn lookup<'py>(
        &self,
        py: Python<'py>,
        target: &str,
        source: &str,
        stamp_ns: i64,
        domain: u8,
    ) -> PyResult<Bound<'py, PyArray2<f64>>> {
        let iso = self
            .inner
            .lookup_tagged(target, source, stamp_ns, domain)
            // **The one arm that has to be caught at the call site**, because it
            // is the one whose identity the error cannot carry:
            // `LookupError::UnknownFrame` holds a BLAKE3 prefix, BLAKE3 does not
            // invert, and `lookup_err` has nothing else to work with. *Here*
            // both names are in scope — this is the entry point that takes them
            // as strings — so the message can name the one that is missing
            // instead of a hash the caller cannot search their source for.
            //
            // The probe is [`unknown_frame_err`] and it is a pure read.
            // Re-probing with `Tree::frame` — which is what shipped — *interns*
            // on a writable tree, so formatting this error spent a frame slot
            // and then, because the intern succeeded, found nothing wrong and
            // printed the hash anyway. That function's doc comment carries the
            // whole account.
            .map_err(|e| match e {
                tf_tree::LookupError::UnknownFrame { .. } => {
                    unknown_frame_err(&self.inner, [target, source], e)
                }
                other => lookup_err(&self.inner, other),
            })?;
        let out = PyArray2::<f64>::zeros(py, [4, 4], false);
        // SAFETY: freshly allocated here; no other reference exists.
        let slice = unsafe { out.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(out)
    }
}

impl PyTree {
    /// The instance uuid as 32 lowercase hex characters.
    ///
    /// Outside the `#[pymethods]` block on purpose: everything inside one
    /// becomes a Python method, and this is shared between
    /// [`PyTree::instance_uuid`] — which refuses a detached tree — and
    /// [`PyTree::__repr__`], which may not. It reads the header either way, so
    /// **both callers check `detached()` first**; this is the formatting, not
    /// the policy.
    fn uuid_hex(&self) -> String {
        self.inner
            .instance_uuid()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// A compiled lookup path.
///
/// `frozen` and `Sync`: a `Plan` is `Copy` in Rust and holds no interior
/// mutability, so several free-threaded Python threads may evaluate the same
/// plan concurrently — which is the workload free-threading exists to serve.
/// What [`PyPlan::adaptive`] returns: `(K,)` stamps and `(K, 4, 4)` poses.
type Knots<'py> = (Bound<'py, PyArray1<i64>>, Bound<'py, PyArray3<f64>>);

/// # A `#[pyclass]` may not contain an over-aligned type
///
/// `Plan` is `align(64), size 4160` — it holds `[Step; MAX_DEPTH]` and `Step`
/// carries an `Iso3`, which is one cacheline by design. (2112 before `0034`
/// moved `MAX_DEPTH` 16 → 32; the alignment, which is what this section is
/// about, is unchanged and is what forces the `Box`.) **CPython's object
/// allocator guarantees 16-byte alignment at best**, so storing a `Plan`
/// directly in a pyclass makes every field access a misaligned dereference.
/// Under a debug build that aborts the interpreter outright:
///
/// ```text
/// misaligned pointer dereference: address must be a multiple of 0x40
/// thread caused non-unwinding panic. aborting.
/// ```
///
/// In release it is silent UB. So the plan lives behind a `Box`, whose
/// allocation *does* honour Rust's alignment. The cost is one allocation per
/// `tree.plan(...)` — a setup call, not a lookup — and one indirection per
/// evaluation, against 2 KB that would otherwise be copied into every pyclass.
///
/// **Anything added here must be checked the same way.** `Tree` happens to be
/// `align(8)`; that is luck, not design.
#[pyclass(name = "Plan", module = "tf_tree", frozen)]
pub struct PyPlan {
    plan: Box<tf_tree::Plan>,
    /// The time-domain tag every query through this handle carries
    /// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`).
    ///
    /// Fixed at [`PyTree::plan`] and validated there against
    /// [`tf_tree::Plan::domain`], so by the time it reaches a `*_tagged` core
    /// method it already agrees with the route — which is what makes the
    /// per-query check the core still performs a predictable-branch no-op rather
    /// than a diagnostic anybody reads.
    ///
    /// **It costs the pyclass eight bytes, not one** — measured, because the
    /// "it lands in the padding" guess is wrong here: the two pointers beside it
    /// pack to 16 with none to spare, so `align(8)` rounds 17 up to 24. That is
    /// a `tree.plan(...)` — a setup call that already allocates a 4 KiB `Plan`
    /// behind the `Box` above — and nothing per sample, which is the only reason
    /// it is affordable. See this type's own note on what may be added here.
    domain: u8,
    /// A **reference-counted handle** to the tree this plan reads through.
    ///
    /// An earlier version held a `*const Tree` and a doc comment asserting it
    /// was "kept valid by the `_tree` attribute Python holds on every `Plan`".
    /// There was no such attribute, so this was a use-after-free:
    ///
    /// ```python
    /// p = tree.plan("map", "base")
    /// del tree          # last reference gone, arena freed
    /// p.at(1500)        # reads freed memory
    /// ```
    ///
    /// It surfaced as a wrong *error* rather than a crash, which is the worse
    /// failure: the allocation was still mapped, so the read succeeded and
    /// returned nonsense. A `Py<PyTree>` is an actual refcount, so the arena
    /// cannot outlive its readers — and it removes the two `unsafe impl`s that
    /// stood in for the guarantee.
    tree: Py<PyTree>,
}

impl PyPlan {
    /// The tree this plan reads through.
    ///
    /// `get` rather than `borrow` because [`PyTree`] is `frozen`: there is no
    /// interior mutability to guard, so no runtime borrow check is needed and
    /// no GIL token is required — which is also what lets this work unchanged
    /// on a free-threaded interpreter.
    fn tree(&self) -> &Tree {
        &self.tree.get().inner
    }
}

#[pymethods]
impl PyPlan {
    /// Evaluate at one stamp, or at an array of stamps.
    ///
    /// Scalar in, `(4, 4)` out; `(N,)` in, `(N, 4, 4)` out. Dispatching on the
    /// argument is the NumPy idiom and is what makes the vectorized path the
    /// *obvious* one — which matters more than any single optimisation here,
    /// because a Python loop over scalar lookups costs ~200 ns per iteration
    /// while the same work through an array amortises to near-native.
    ///
    /// **`stamps` is positional-only.** `METH_FASTCALL` is a measured 29 ns
    /// cheaper than `PyArg_ParseTuple` for one argument (§4.2).
    ///
    /// # `layout=`
    ///
    /// Keyword-only, and the one keyword §4.2 permits here — "accepted as a
    /// keyword only on the non-hot overload". Four values, each an explicit
    /// statement of memory layout with no default that could be silently wrong
    /// (`docs/API.md` R4):
    ///
    /// | `layout=` | scalar | batch | dtype |
    /// | --- | --- | --- | --- |
    /// | `"mat4"` (default) | `(4, 4)` | `(N, 4, 4)` | `float64` |
    /// | `"quat"` | `(7,)` | `(N, 7)` | `float64` |
    /// | `"affine32"` | `(12,)` | `(N, 12)` | **`float32`** |
    /// | `"quat_twist"` | `(13,)` | `(N, 13)` | `float64` |
    ///
    /// `"quat_twist"` is `at_with_derivatives` as a layout
    /// (`docs/PHASE5.md` §4.4 item 1): `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy
    /// vz]`, the body twist in the plan's **source** frame, angular first. It
    /// is the only layout whose emission can fail for a reason the others
    /// cannot — a `LerpSlerp` edge has no exact body twist and raises
    /// `DerivativesUnavailableError` rather than emitting a finite difference
    /// that would look like an answer.
    ///
    /// **What the keyword costs the caller who does not pass one is not
    /// established here, and saying so is the honest report.** §4.2 provides an
    /// escape hatch — "if that measurably costs, add `at_quat` /
    /// `at_affine32` as separate positional-only methods" — which needs a
    /// number to trigger. An A/B was attempted: two release builds of this
    /// crate differing only in whether `at`/`at_into` carry the keyword-only
    /// parameter, `p.at_into(t, out)` at depth 3, best of seven over 300 k
    /// iterations, pinned. It produced **nothing usable** — the run-to-run
    /// spread on a *single* binary was 283–378 ns, several times any plausible
    /// effect — because this host is shared and was not quiet. The number is
    /// owed; it is not invented here.
    ///
    /// What is known without measuring: `stamps` stays **positional**, so the
    /// 29 ns §4.2 actually measured is untouched, and PyO3 keeps the vectorcall
    /// convention for a signature with keyword-only arguments rather than
    /// falling back to `PyArg_ParseTuple`. **That second half is verified, not
    /// assumed** — §4.2 is NORMATIVE that it be checked, and
    /// `tests/python/test_api.py::test_the_hot_methods_are_emitted_as_meth_fastcall`
    /// reads the `PyMethodDef::ml_flags` PyO3 emitted: `at`, `at_into` and
    /// `push` are `METH_FASTCALL | METH_KEYWORDS` (`0x82`) and `latest` is
    /// `METH_NOARGS` (`0x04`), on both the GIL and the free-threaded build.
    /// `METH_FASTCALL | METH_KEYWORDS` is still vectorcall — CPython calls it
    /// through `_PyCFunctionFastWithKeywords` with an args array and a names
    /// tuple, and builds no argument tuple — so the exposure is bounded by the
    /// difference between two vectorcall shapes, not by the 29 ns.
    #[pyo3(signature = (stamps, /, *, layout = None))]
    fn at<'py>(
        &self,
        py: Python<'py>,
        stamps: &Bound<'py, PyAny>,
        layout: Option<&str>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // Anything but the default layout leaves the hot path entirely, before
        // the scalar/array dispatch below, so `mat4` pays one `Option` test for
        // the other three existing.
        if let Some(name) = layout {
            let layout = layout_from_str(name)?;
            if layout != Layout::Mat4 {
                return self.at_layout(py, stamps, layout);
            }
        }
        // **Scalar first, and it is not a style choice.** `cast::<PyArray1<i64>>`
        // on an `int` *fails*, and a failed downcast builds a `DowncastError` —
        // type-name lookups and a formatted message — that is then thrown away
        // by the `if let Ok`. Measured on a depth-3 chain: probing the array
        // first cost ~150 ns of the scalar call's ~313 ns, against 114 ns of
        // actual engine work.
        //
        // `is_instance_of::<PyInt>` is a pointer comparison against the type
        // object. The array path pays it too, but amortises it over N samples,
        // where the scalar path pays the failed cast on every single tick — and
        // a control loop is all scalar ticks.
        if !stamps.is_instance_of::<pyo3::types::PyInt>() {
            if let Ok(arr) = stamps.cast::<PyArray1<i64>>() {
                let n = arr.len();
                let out = PyArray3::<f64>::zeros(py, [n, 4, 4], false);
                self.fill(py, arr, &out, n)?;
                return Ok(out.into_any());
            }
        }
        let stamp = stamp_from_any(stamps)?;
        let g = self.tree().guard();
        let iso = self
            .plan
            .at_tagged(&g, stamp, self.domain)
            .map_err(|e| lookup_err(self.tree(), e))?;
        let out = PyArray2::<f64>::zeros(py, [4, 4], false);
        // SAFETY: freshly allocated by us, so nothing else holds a reference and
        // the slice is exactly 16 contiguous f64.
        let slice = unsafe { out.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(out.into_any())
    }

    /// Evaluate a batch into a caller-provided `(N, 4, 4)` float64 array.
    ///
    /// The tier that allocates nothing (§5.2). `np.empty((n,4,4))` is a flat
    /// ~270 ns — irrelevant for a 65 536-sample batch and ~50% of the call for a
    /// 64-sample control loop, which is the case this exists for.
    ///
    /// The array is validated **completely before any element is written**: a
    /// half-written output is worse than none, because it looks like data.
    ///
    /// `layout=` is [`at`](Self::at)'s, and `out`'s required shape and dtype
    /// follow it: `(N, layout_elems)` — or `(layout_elems,)` for a scalar stamp
    /// — and `float32` for `"affine32"`, `float64` for the rest. R2's corollary
    /// is why this takes the keyword at all: **every** batch entry point has an
    /// `_into` form, and a layout reachable only through the allocating one
    /// would be a batch path with no allocation-free tier.
    ///
    /// # The two paths disagree about what a stamp is, and the `mat4` one is
    /// the one that is wrong
    ///
    /// This method has two stamp dispatches — one for the default `mat4`
    /// layout, in the body below, and one in `at_into_layout` for the other
    /// three. They are **not** the same, and until one of them moves a caller
    /// can observe which one they are on:
    ///
    /// | `stamps` | `at`, and `at_into(layout=..)` | `at_into` (`mat4`) |
    /// | --- | --- | --- |
    /// | `np.int64(t)` | accepted (§3 lists it) | `BufferError` |
    /// | `1.5` | `TypeError`, 238 ns ULP (§3) | `BufferError` |
    /// | a `list`, or a non-`int64` array | numpy's or PyO3's own conversion `TypeError` | `BufferError` naming `(N,) int64` |
    ///
    /// `docs/PHASE3.md` §3 is NORMATIVE about the first two rows, so the
    /// `mat4` column is a defect on both: `np.int64` is what `stamps[i]` hands
    /// you, and a `float` must meet the measurement rather than a complaint
    /// about a buffer. The `layout=` path was fixed to match `at`; **the
    /// `mat4` path is outstanding, not decided.** It is deferred rather than
    /// done because closing it moves the third row too — the shape-naming
    /// `BufferError` that this path, alone, still gives — and that is a
    /// change to the default overload's error surface with its own tests,
    /// not a line inside a layout feature. Recorded rather than silently
    /// tolerated: a reader who finds this table is looking at the last place
    /// the two shapes differ.
    ///
    /// The third row is the price the fix charged, and it is charged
    /// **symmetrically**: `at` has always answered a `list` or a `float64`
    /// array that way, because there is no cast left to fail once the array
    /// probe has been fallen through — [`stamp_from_any`] has the last word
    /// and raises PyO3's or numpy's own conversion error. Matching `at`
    /// exactly was the point, so `at_into(.., layout=..)` gives up the
    /// shape-naming `BufferError` for it. §3's two rows are worth more than
    /// one message: they are what a caller *writes*, and the message is what
    /// they read once.
    #[pyo3(signature = (stamps, out, /, *, layout = None))]
    fn at_into(
        &self,
        py: Python<'_>,
        stamps: &Bound<'_, PyAny>,
        out: &Bound<'_, PyAny>,
        layout: Option<&str>,
    ) -> PyResult<()> {
        if let Some(name) = layout {
            let layout = layout_from_str(name)?;
            if layout != Layout::Mat4 {
                return self.at_into_layout(py, stamps, out, layout, name);
            }
        }
        // **`reject_device_memory` is not called on the numpy path, and that is
        // a measurement, not an omission.** It does `getattr("__dlpack_device__")`
        // and then *calls* it — a full Python method call and a tuple extract,
        // measured at ~120 ns, on every invocation. NumPy has had
        // `__dlpack_device__` since 1.22, so a plain `np.empty((4,4))` paid all
        // of it.
        //
        // A successful `cast::<PyArrayN<f64>>` proves the object is a genuine
        // `numpy.ndarray`, whose data pointer is host memory by construction —
        // CuPy and torch arrays are *not* numpy subclasses, so they fail the
        // cast and reach the probe below, where the cost is worth paying. The
        // §5.5 guarantee is unchanged; only the objects that can trip it now pay
        // for the check.

        // The scalar form, which mirrors `at`'s scalar/array overload. A control
        // loop does **one** lookup per tick and cannot batch, so the allocation
        // `at` performs is paid once per tick forever — and it is the majority
        // of the call. Measured on a depth-3 chain, in-process, release build:
        //
        //     np.empty((4, 4))          177 ns   <- object construction
        //     plan.at(t)                330 ns
        //     plan.at_into(t, buf)      ~146 ns
        //
        // The 177 ns is NumPy building the array *object*, not zeroing it —
        // replacing `zeros` with an uninitialized `new` was measured at no
        // change and reverted. The only way past it is not to allocate.
        // **Dispatch on `stamps`, then validate `out` against it.** Probing
        // `out` first blamed the wrong argument: a scalar stamp with an
        // `(1, 4, 4)` buffer reported "stamps must be an (N,) int64 array", and
        // an `(N,)` stamps array with a `(4, 4)` buffer escaped as numpy's own
        // `TypeError: only integer scalar arrays can be converted to a scalar
        // index` from `stamp_from_any`. Both named the argument the caller had
        // got right.
        //
        // `is_instance_of::<PyInt>` is a pointer comparison against the type
        // object, so the scalar path still leads.
        if stamps.is_instance_of::<pyo3::types::PyInt>() {
            let stamp = stamp_from_any(stamps)?;
            let arr = out.cast::<PyArray2<f64>>().map_err(|_| {
                BufferError::new_err(
                    "a scalar stamp needs out to be a writable, C-contiguous (4, 4) \
                     float64 numpy array",
                )
            })?;
            if !arr.is_c_contiguous() {
                return Err(BufferError::new_err(
                    "out must be C-contiguous; pass np.ascontiguousarray(...) \
                     explicitly if you meant to copy",
                ));
            }
            let shape = arr.shape();
            if shape != [4, 4] {
                return Err(BufferError::new_err(format!(
                    "a scalar stamp needs out of shape (4, 4), got {shape:?}"
                )));
            }
            // **Writability, before anything is evaluated.** `as_slice_mut` is
            // `unsafe` because it checks neither `NPY_ARRAY_WRITEABLE` nor
            // aliasing, and skipping the check does not merely produce a wrong
            // answer: a read-only `np.memmap` is a `PROT_READ` page, and storing
            // into it is `SIGSEGV`, not an error. §5.5's rule is refuse rather
            // than fault, and it applies to host memory the caller cannot write
            // exactly as much as to device memory.
            //
            // `try_readwrite` is the safe API and was tried first: it also
            // consults rust-numpy's borrow registry, which costs a global
            // lookup and measured **+50 ns on a 173 ns call** — enough to put
            // `at_into` back above `at` and undo the reason it exists. The
            // registry answers a question this code was not getting wrong;
            // writability is the one it was. `is_writeable` reads the same
            // `flags` field `is_c_contiguous` already reads, for about a
            // nanosecond.
            if !is_writeable(arr.as_untyped()) {
                return Err(BufferError::new_err(
                    "out is not writable (NumPy reports NPY_ARRAY_WRITEABLE clear); \
                     a read-only mapping cannot receive a transform",
                ));
            }
            let g = self.tree().guard();
            let iso = self
                .plan
                .at_tagged(&g, stamp, self.domain)
                .map_err(|e| lookup_err(self.tree(), e))?;
            // SAFETY: checked C-contiguous, (4, 4) and writable above, so this
            // slice is exactly 16 writable f64. Aliasing remains the caller's
            // to avoid, as it was before — `as_slice_mut` documents that, and
            // handing the same array to two threads is already a data race in
            // NumPy's own terms. Nothing is written before every check passes:
            // a half-written output is worse than none, because it looks like
            // data.
            let slice = unsafe { arr.as_slice_mut()? };
            tf_tree::write_mat4(&iso, slice);
            return Ok(());
        }

        let stamps = stamps
            .cast::<PyArray1<i64>>()
            .map_err(|_| BufferError::new_err("stamps must be an (N,) int64 array, or an int"))?;
        let arr = match out.cast::<PyArray3<f64>>() {
            Ok(a) => a,
            Err(_) => {
                // Not a numpy array, so it may be device memory. Refuse rather
                // than fault (§5.5): a CPU store to a `cudaMalloc` pointer is
                // undefined, not slow.
                reject_device_memory(out)?;
                // **Only `numpy.ndarray` is accepted, subclasses included.** The
                // message used to offer "an object exposing the buffer
                // protocol", and `PHASE3.md` §5.5 still describes pinned torch
                // and CuPy allocations as qualifying — but `cast` matches the
                // numpy type, so a `memoryview` or a pinned torch tensor is
                // refused here whatever its layout. Advertising a path that does
                // not exist sends people to debug their buffer instead of their
                // expectations.
                return Err(BufferError::new_err(
                    "out must be a writable, C-contiguous (N, 4, 4) float64 numpy array \
                     — or (4, 4) for a scalar stamp. Other buffer-protocol objects are \
                     not accepted yet; np.asarray(...) it first",
                ));
            }
        };
        let n = stamps.len();
        self.fill(py, stamps, arr, n)
    }

    /// Evaluate past the newest sample under an explicit policy, and get back
    /// how far past it that was.
    ///
    /// `at()` refuses a stamp newer than every published sample on the route,
    /// which is right for a caller that must not act on invented data and
    /// wrong for a controller running at 1 kHz against a 100 Hz state
    /// estimate — that one is *always* asking past the newest sample, and the
    /// honest answer is a bounded prediction with its bound attached
    /// (`docs/decisions/0039`).
    ///
    /// Returns `(poses, by_ns)`. **There is no spelling that returns the pose
    /// alone**, which is the whole design rather than an inconvenience: the
    /// danger in extrapolation is a pose that looks fresh, so the distance
    /// comes back in the same value and ignoring it takes a deliberate `[0]`.
    ///
    /// `policy` is `"error"` (refuse — what `at` does, with a distance
    /// attached on success), `"hold"` (the newest pose, for a latched or
    /// displayed value) or `"constant_twist"` (extend the screw the two newest
    /// samples imply, which is what a controller wants). It is **required**:
    /// extrapolation is opt-in per query, and a default would make it
    /// something a caller could get without asking.
    ///
    /// `at` is untouched and still refuses. This is a second entry point, not
    /// a mode on the first.
    ///
    /// # `by_ns` is per stamp, and in a batch that means an array
    ///
    /// | `stamps` | `poses` | `by_ns` |
    /// | --- | --- | --- |
    /// | `int` | `(4, 4)` float64 | `int` |
    /// | `(N,)` int64 | `(N, 4, 4)` float64 | `(N,)` **int64 array** |
    ///
    /// **A scalar `by_ns` for a batch would be wrong, not merely coarse.** The
    /// distance is `max(0, stamp - newest_common)`, so it is a function of the
    /// stamp; a batch that straddles the newest sample has interpolated
    /// elements (`0`) and extrapolated ones in the same call. Collapsing that
    /// to one number means either a `max`, which marks fresh elements stale,
    /// or a `min`, which marks stale ones fresh — and the second is exactly
    /// the failure this surface exists to prevent. The array costs 8 bytes per
    /// element beside a 128-byte pose, and `by_ns.max()` is the one-liner a
    /// caller who genuinely wants the scalar writes.
    ///
    /// # What it does not carry, and where the breakdown lives
    ///
    /// The Rust `Extrapolated` also names the **edge** that ran out of data
    /// first. That is an `EdgeId`, and this binding has never handed one to
    /// Python — `crates/tf_tree_py/src/errors.rs` resolves every id to
    /// `edge "parent" -> "child"` before a caller sees it, and doing that
    /// resolution per query would be an arena walk on the path this method
    /// exists for. `Plan.edges()` is the route's dynamic edges and
    /// `tf_tree doctor` is the per-edge breakdown; `0039` points at the same
    /// two for the same reason.
    ///
    /// Only the default `mat4` layout. The Rust method this mirrors returns
    /// one pose type, and the `layout=` dispatch belongs to the batch fold,
    /// which carries no policy.
    ///
    /// # Cost
    ///
    /// The batch is a loop over the scalar form under one `Guard`, not the
    /// engine's batch fold: `Plan::at_many_into` passes `ExtrapPolicy::Error`
    /// and `0039` §4 deliberately did not thread a runtime policy through it,
    /// because that would leave the match live on `at`'s hot path. So this
    /// pays an `O(log n)` bracket search per stamp per step rather than riding
    /// the monotone cursor, plus one `newest_stamp` load per dynamic edge per
    /// stamp for the distance.
    #[pyo3(signature = (stamps, policy, /, *, layout = None))]
    fn at_extrapolating<'py>(
        &self,
        py: Python<'py>,
        stamps: &Bound<'py, PyAny>,
        policy: &str,
        layout: Option<&str>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
        let policy = extrap_from_str(policy)?;
        let layout = match layout {
            Some(name) => layout_from_str(name)?,
            None => Layout::Mat4,
        };
        // `at`'s dispatch, to the letter: `PyInt` first because a failed
        // `cast::<PyArray1<i64>>` builds and throws away a `DowncastError`, and
        // the array cast is *fallen through* rather than `else`-d so a `float`
        // still meets `stamp_from_any`'s `TypeError` with the 238 ns ULP in it
        // (§3) instead of a complaint about array dtypes.
        if !stamps.is_instance_of::<pyo3::types::PyInt>() {
            if let Ok(arr) = stamps.cast::<PyArray1<i64>>() {
                return self.extrapolate_batch(py, arr, policy, layout);
            }
        }
        let stamp = stamp_from_any(stamps)?;
        let g = self.tree().guard();
        let x = self
            .plan
            .at_extrapolating_tagged(&g, stamp, self.domain, policy)
            .map_err(|e| lookup_err(self.tree(), e))?;
        // `mat4` keeps `at`'s `(4, 4)` shape; every other layout is the flat
        // `(elems,)` row `at(.., layout=..)` already returns, so the two methods
        // agree on shape for the same `layout` argument.
        let out = if layout == Layout::Mat4 {
            let a = PyArray2::<f64>::zeros(py, [4, 4], false);
            // SAFETY: freshly allocated here, so nothing else holds a reference
            // and the slice is exactly 16 contiguous f64.
            write_extrapolated(&x.pose, layout, unsafe { a.as_slice_mut()? })?;
            a.into_any()
        } else {
            let a = PyArray1::<f64>::zeros(py, [layout.elems()], false);
            // SAFETY: as above, `layout.elems()` contiguous f64.
            write_extrapolated(&x.pose, layout, unsafe { a.as_slice_mut()? })?;
            a.into_any()
        };
        Ok((out, x.by_ns.into_pyobject(py)?.into_any()))
    }

    /// [`PyPlan::at_extrapolating`] writing into caller memory.
    ///
    /// **`docs/API.md` R2 makes this NORMATIVE, not optional**: *"every batch
    /// entry point has an `_into` form writing into caller memory"*, and its
    /// justification is this exact caller — the allocation *"is noise at
    /// n = 65536 and half the call at n = 64, and n = 64 is the control loop"*.
    /// `at_extrapolating` is the method a controller reaches for, so shipping it
    /// without this was the rule broken on the one path the rule was written
    /// about.
    ///
    /// `poses` takes the shape `at_extrapolating` would have returned —
    /// `(4, 4)` or `(elems,)` for a scalar stamp, `(N, 4, 4)` or `(N, elems)`
    /// for an array — and `by_ns` is `()`-shaped or `(N,)` `int64`.
    ///
    /// # A partial write is possible, and that is the trade
    ///
    /// The allocating form fills arrays it owns, so a failure part-way drops
    /// them and the caller sees only the exception. Here the buffers are the
    /// caller's, so a `LookupError` on element *k* leaves `0..k` written and the
    /// rest as they were. That is the same contract `at_into` carries and the
    /// reason both exist: a caller who wants all-or-nothing uses the allocating
    /// form and pays the allocation for it.
    #[pyo3(signature = (stamps, policy, poses, by_ns, /, *, layout = None))]
    fn at_extrapolating_into(
        &self,
        py: Python<'_>,
        stamps: &Bound<'_, PyAny>,
        policy: &str,
        poses: &Bound<'_, PyAny>,
        by_ns: &Bound<'_, PyAny>,
        layout: Option<&str>,
    ) -> PyResult<()> {
        let policy = extrap_from_str(policy)?;
        let layout = match layout {
            Some(name) => layout_from_str(name)?,
            None => Layout::Mat4,
        };
        // Refuse the layout before touching either buffer, for the same reason
        // `extrapolate_batch` does: the fold cannot refuse per element.
        write_extrapolated(&tf_tree::Iso3::IDENTITY, layout, &mut [0.0; 16])?;
        let e = layout.elems();

        // `at`'s stamp dispatch, to the letter — see `at_extrapolating`.
        let src_arr = if stamps.is_instance_of::<pyo3::types::PyInt>() {
            None
        } else {
            stamps.cast::<PyArray1<i64>>().ok()
        };
        let scalar = src_arr.is_none();
        let src_owned = match &src_arr {
            Some(arr) => {
                if !arr.is_c_contiguous() {
                    return Err(BufferError::new_err(
                        "stamps must be C-contiguous; pass np.ascontiguousarray(...) \
                         explicitly if you meant to copy",
                    ));
                }
                [0i64]
            }
            None => [stamp_from_any(stamps)?],
        };
        // SAFETY: checked C-contiguous above; the borrow is held across the
        // fold below for §6.2's reason.
        let src: &[i64] = match &src_arr {
            Some(a) => unsafe { a.as_slice()? },
            None => &src_owned,
        };
        let n = src.len();

        let want_poses: &[usize] = if scalar {
            if layout == Layout::Mat4 {
                &[4, 4]
            } else {
                &[e]
            }
        } else if layout == Layout::Mat4 {
            &[n, 4, 4]
        } else {
            &[n, e]
        };
        let want_dist: &[usize] = if scalar { &[] } else { &[n] };

        let pose_arr = poses.cast::<numpy::PyArrayDyn<f64>>().map_err(|_| {
            BufferError::new_err("poses must be a C-contiguous, writable float64 array")
        })?;
        check_out(pose_arr.as_untyped(), want_poses)?;
        let dist_arr = by_ns.cast::<numpy::PyArrayDyn<i64>>().map_err(|_| {
            BufferError::new_err("by_ns must be a C-contiguous, writable int64 array")
        })?;
        check_out(dist_arr.as_untyped(), want_dist)?;

        // **Refuse a `by_ns` that aliases `stamps`, before either mutable slice
        // exists.** This is the only `_into` form on this binding whose input
        // and one of whose outputs are both `int64`, so it is the only one where
        // `f(stamps=a, .., by_ns=a)` type-checks all the way down: `check_out`
        // wants `(N,) int64` and `a` *is* one. The fold would then hold `&[i64]`
        // and `&mut [i64]` over one allocation — undefined behaviour reached
        // from safe Python. Every other `_into` is safe from this by dtype
        // alone, which is why no existing helper checks for it.
        //
        // Both buffers are C-contiguous by `check_out`, so comparing byte ranges
        // is the whole of the question — and it catches a *view* of the same
        // memory, which comparing array identity would not.
        if let Some(a) = &src_arr {
            let (s, d) = (a.data() as usize, dist_arr.data() as usize);
            let len = core::mem::size_of_val(src);
            if s < d + len && d < s + len {
                return Err(BufferError::new_err(
                    "by_ns must not alias stamps: they are both int64 and this call \
                     writes one while reading the other. Pass a separate array.",
                ));
            }
        }

        // SAFETY: `check_out` proved both C-contiguous, correctly shaped and
        // writable, and the range check above rules out `by_ns` aliasing
        // `stamps`; aliasing beyond that stays the caller's, as `as_slice_mut`
        // documents.
        let (pd, dd) = unsafe { (pose_arr.as_slice_mut()?, dist_arr.as_slice_mut()?) };

        let plan = *self.plan;
        let tree = self.tree();
        let domain = self.domain;
        let mut run = move || -> Result<(), tf_tree::LookupError> {
            let g = tree.guard();
            for (i, &t) in src.iter().enumerate() {
                let x = plan.at_extrapolating_tagged(&g, t, domain, policy)?;
                write_pose_unchecked(&x.pose, layout, &mut pd[i * e..(i + 1) * e]);
                dd[i] = x.by_ns;
            }
            Ok(())
        };
        let res = if release_the_gil(n, self.plan.len()) {
            py.detach(run)
        } else {
            run()
        };
        res.map_err(|err| lookup_err(self.tree(), err))
    }

    /// The most recent transform on this path.
    fn latest<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyArray2<f64>>> {
        let g = self.tree().guard();
        let iso = self
            .plan
            .latest(&g)
            .map_err(|e| lookup_err(self.tree(), e))?;
        let out = PyArray2::<f64>::zeros(py, [4, 4], false);
        // SAFETY: freshly allocated here; no other reference exists.
        let slice = unsafe { out.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(out)
    }

    /// The minimum set of knots whose linear interpolation stays within `tol`.
    ///
    /// Returns `(stamps, poses)` — an `(K,)` int64 array and a `(K, 4, 4)`
    /// float64 array, strictly increasing in stamp. The consumer LERPs between
    /// adjacent knots on whatever device they live on, with the reconstruction
    /// error bounded by construction.
    ///
    /// This is what makes the device story small (§5.6). A 100 ms sweep at
    /// 1 cm / 1e-4 rad is tens of knots — roughly a kilobyte — so the transfer
    /// that a zero-copy-to-GPU pipeline would optimise is already negligible.
    /// **If this returned enough data for that to matter, it would be evidence
    /// the tolerance was wrong**, not that the transport needed work.
    ///
    /// `lin` is metres and `ang` is radians. Both default to the `docs/PHASE3.md`
    /// §4.2 values.
    #[pyo3(signature = (start_ns, end_ns, /, *, lin = 1e-3, ang = 1e-4))]
    fn adaptive<'py>(
        &self,
        py: Python<'py>,
        start_ns: i64,
        end_ns: i64,
        lin: f64,
        ang: f64,
    ) -> PyResult<Knots<'py>> {
        if !(lin.is_finite() && ang.is_finite()) || lin <= 0.0 || ang <= 0.0 {
            return Err(PyValueError::new_err(
                "lin and ang must be finite and positive",
            ));
        }
        // **`SystemDomain` here is storage, not the query.** `at_adaptive_tagged`
        // keeps a type parameter and it means something different there: `D`
        // fixes the element type of `scratch` and of the returned stamp slice
        // and is read by nothing in the fold, while `self.domain` is what is
        // checked. `0038` weighs this against the three alternatives — a second
        // public marker domain, a `repr(transparent)` slice cast, or deleting
        // `Stamp<D>` from the typed return — and carrying one documented phantom
        // is the smallest. The stamps become plain integers eight lines below,
        // so the phantom never reaches Python.
        let mut scratch = tf_tree::AdaptiveScratch::<SystemDomain>::new();
        let tol = tf_tree::ErrBound::new(ang, lin);
        let g = self.tree().guard();
        let (stamps, poses) = self
            .plan
            .at_adaptive_tagged(
                &g,
                (Stamp::from_nanos(start_ns), Stamp::from_nanos(end_ns)),
                self.domain,
                tol,
                &mut scratch,
            )
            .map_err(|e| lookup_err(self.tree(), e))?;

        let k = stamps.len();
        let out_s = PyArray1::<i64>::zeros(py, [k], false);
        let out_p = PyArray3::<f64>::zeros(py, [k, 4, 4], false);
        {
            // SAFETY: both arrays were just allocated here, so nothing else
            // holds a reference to them and both are contiguous by
            // construction.
            let (sd, pd) = unsafe { (out_s.as_slice_mut()?, out_p.as_slice_mut()?) };
            for (i, (st, iso)) in stamps.iter().zip(poses.iter()).enumerate() {
                sd[i] = st.nanos();
                tf_tree::write_mat4(iso, &mut pd[i * 16..(i + 1) * 16]);
            }
        }
        Ok((out_s, out_p))
    }

    /// Folded depth of this path, in edges.
    fn depth(&self) -> usize {
        self.plan.len()
    }

    /// The **dynamic** edges this plan samples, as `(parent, child)` pairs
    /// (§4.4).
    ///
    /// Shorter than [`depth`](Self::depth) whenever the path crosses a static
    /// edge: those are folded into a constant at compile time and their
    /// identities do not survive the fold. See
    /// [`plan_edges_impl`](crate::offline::plan_edges_impl) — the alternative
    /// to saying so is fabricating ids for edges the plan genuinely no longer
    /// knows about.
    fn edges(&self) -> PyResult<Vec<(String, String)>> {
        crate::offline::plan_edges_impl(self.tree(), &self.plan)
    }

    fn __repr__(&self) -> String {
        format!("<tf_tree.Plan depth={}>", self.plan.len())
    }
}

impl PyPlan {
    /// Shared batch path: validate, then fold straight into `out`.
    fn fill(
        &self,
        py: Python<'_>,
        stamps: &Bound<'_, PyArray1<i64>>,
        out: &Bound<'_, PyArray3<f64>>,
        n: usize,
    ) -> PyResult<()> {
        // Every check before a single store (§5.3). Non-contiguous is rejected
        // rather than silently copied: a silent copy would defeat the whole
        // purpose of this method while appearing to work, and the user would
        // ship it and wonder why their profile did not change.
        if !stamps.is_c_contiguous() || !out.is_c_contiguous() {
            return Err(BufferError::new_err(
                "stamps and out must be C-contiguous; pass np.ascontiguousarray(...) \
                 explicitly if you meant to copy",
            ));
        }
        let shape = out.shape();
        if shape != [n, 4, 4] {
            return Err(BufferError::new_err(format!(
                "out must have shape ({n}, 4, 4), got {shape:?}"
            )));
        }

        // **Writability, before a single store.** `as_slice_mut` checks neither
        // `NPY_ARRAY_WRITEABLE` nor aliasing; a read-only `np.memmap` is a
        // `PROT_READ` page and storing into it is `SIGSEGV`, not an error.
        // A single `flags` read — see the note in `at_into` on why not
        // `try_readwrite`.
        if !is_writeable(out.as_untyped()) {
            return Err(BufferError::new_err(
                "out is not writable (NumPy reports NPY_ARRAY_WRITEABLE clear); \
                 a read-only mapping cannot receive a transform",
            ));
        }

        // SAFETY: both arrays were just checked C-contiguous, `out` is writable,
        // and aliasing is the caller's to avoid exactly as `as_slice_mut`
        // documents. Taken *before* `allow_threads` and held across it: NumPy
        // refuses to resize an array while a buffer is exported, which is what
        // keeps these pointers valid (§6.2).
        let (src, dst) = unsafe { (stamps.as_slice()?, out.as_slice_mut()?) };

        let plan = *self.plan;
        let tree = self.tree();
        // Read out beside `plan` and `tree` above rather than through `self`
        // inside the closure: one load, on the near side of a `detach` whose
        // body must touch no Python object at all (§6.2).
        let domain = self.domain;

        let mut run = || {
            let g = tree.guard();
            // Raw nanoseconds: `at_many_into_tagged` takes `&[i64]` precisely so
            // this path does not have to allocate a `Vec<Stamp>` — which would
            // be the intermediate buffer `at_into` exists to avoid. The batch
            // forms never had a use for their type parameter beyond the domain
            // check (`0038` §1), so the tagged spelling loses nothing.
            plan.at_many_into_tagged(&g, src, domain, Layout::Mat4, dst)
        };
        let res = if release_the_gil(n, self.plan.len()) {
            // `detach` is PyO3 0.29's name for what was `allow_threads`.
            // Touch no Python object inside (§6.2): only the raw slices above.
            py.detach(run)
        } else {
            run()
        };
        res.map_err(|e| lookup_err(tree, e))
    }

    /// [`PyPlan::at_extrapolating`]'s array half: `(N, 4, 4)` poses and `(N,)`
    /// distances, allocated here and filled together.
    ///
    /// Both arrays are ours until this returns, so a failure part-way through
    /// drops them and the caller sees only the exception — the partial-write
    /// question `at_into` has to answer does not arise, because there is no
    /// caller buffer to half-fill.
    fn extrapolate_batch<'py>(
        &self,
        py: Python<'py>,
        stamps: &Bound<'py, PyArray1<i64>>,
        policy: ExtrapPolicy,
        layout: Layout,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
        if !stamps.is_c_contiguous() {
            return Err(BufferError::new_err(
                "stamps must be C-contiguous; pass np.ascontiguousarray(...) \
                 explicitly if you meant to copy",
            ));
        }
        // **Refuse the layout once, here, before anything is allocated.** The
        // fold below runs under `detach` where no `PyResult` can be built, so
        // `write_pose_unchecked` cannot refuse per element — this is the check
        // that makes "unchecked" true rather than hopeful.
        write_extrapolated(&tf_tree::Iso3::IDENTITY, layout, &mut [0.0; 16])?;

        let n = stamps.len();
        let e = layout.elems();
        // `(N, 4, 4)` for `mat4`, `(N, elems)` otherwise — the same pair of
        // shapes `at` returns for the same `layout`.
        let poses = if layout == Layout::Mat4 {
            PyArray3::<f64>::zeros(py, [n, 4, 4], false).into_any()
        } else {
            PyArray2::<f64>::zeros(py, [n, e], false).into_any()
        };
        let dist = PyArray1::<i64>::zeros(py, [n], false);
        {
            // SAFETY: `stamps` was checked C-contiguous above; `poses` and
            // `dist` were just allocated here, so nothing else holds a
            // reference to either and both are contiguous by construction. The
            // borrows are held across the `detach` below for §6.2's reason —
            // NumPy refuses to resize an array while a buffer is exported.
            let flat = poses.cast::<numpy::PyArrayDyn<f64>>()?;
            let (src, pd, dd) = unsafe {
                (
                    stamps.as_slice()?,
                    flat.as_slice_mut()?,
                    dist.as_slice_mut()?,
                )
            };
            let plan = *self.plan;
            let tree = self.tree();
            // Read out beside `plan` and `tree` rather than through `self`
            // inside the closure: `detach`'s body must touch no Python object
            // at all (§6.2).
            let domain = self.domain;
            let mut run = move || -> Result<(), tf_tree::LookupError> {
                let g = tree.guard();
                for (i, &t) in src.iter().enumerate() {
                    let x = plan.at_extrapolating_tagged(&g, t, domain, policy)?;
                    write_pose_unchecked(&x.pose, layout, &mut pd[i * e..(i + 1) * e]);
                    dd[i] = x.by_ns;
                }
                Ok(())
            };
            // The same threshold `at` uses. It under-counts this path — a
            // distance costs one `newest_stamp` load per dynamic edge per
            // stamp on top of the fold — so it errs towards releasing the GIL
            // for work that is longer than it estimated, which is the harmless
            // direction.
            let res = if release_the_gil(n, self.plan.len()) {
                py.detach(run)
            } else {
                run()
            };
            res.map_err(|e| lookup_err(tree, e))?;
        }
        Ok((poses.into_any(), dist.into_any()))
    }

    /// [`PyPlan::at`]'s `layout=` path: allocate the right shape and fill it.
    ///
    /// Off the `mat4` hot path by construction — [`PyPlan::at`] branches here
    /// before its scalar/array dispatch — so this is written for clarity. It
    /// still checks `PyInt` before attempting the array cast, for the reason
    /// [`PyPlan::at`]'s own comment measures: a failed downcast builds and
    /// throws away a `DowncastError`.
    fn at_layout<'py>(
        &self,
        py: Python<'py>,
        stamps: &Bound<'py, PyAny>,
        layout: Layout,
    ) -> PyResult<Bound<'py, PyAny>> {
        let e = layout.elems();
        if !stamps.is_instance_of::<pyo3::types::PyInt>() {
            if let Ok(arr) = stamps.cast::<PyArray1<i64>>() {
                return self.alloc_layout(py, arr, layout, e);
            }
        }
        {
            // **A one-element batch, not a scalar kernel.** `docs/PHASE3.md`
            // §11.1 requires `at(t)` to equal `at([t])[0]` *bit-exactly*, and
            // the cheapest way to guarantee that is for there to be one
            // implementation. The scalar `mat4` path above is the deliberate
            // exception — it predates this and is the measured hot path — and
            // what a second implementation *saves* on these three layouts was
            // not measured, because the reason not to have one is correctness
            // rather than cost.
            //
            // **Reached by falling through the array cast, not by an `else`**,
            // so a `float` still meets `stamp_from_any`'s `TypeError` with the
            // ULP measurement in it (§3) rather than a `BufferError` about
            // array dtypes. `at`'s `mat4` path has that shape for the same
            // reason and this must not diverge from it.
            let src = [stamp_from_any(stamps)?];
            if layout.is_f32() {
                let out = PyArray1::<f32>::zeros(py, [e], false);
                // SAFETY: freshly allocated here, contiguous by construction,
                // and no other reference to it exists.
                let dst = unsafe { out.as_slice_mut()? };
                self.eval_f32(py, &src, layout, dst)?;
                Ok(out.into_any())
            } else {
                let out = PyArray1::<f64>::zeros(py, [e], false);
                // SAFETY: as above.
                let dst = unsafe { out.as_slice_mut()? };
                self.eval_f64(py, &src, layout, dst)?;
                Ok(out.into_any())
            }
        }
    }

    /// [`Self::at_layout`]'s batch half: allocate `(n, elems)` and fill it.
    fn alloc_layout<'py>(
        &self,
        py: Python<'py>,
        stamps: &Bound<'py, PyArray1<i64>>,
        layout: Layout,
        e: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        if !stamps.is_c_contiguous() {
            return Err(BufferError::new_err(
                "stamps must be C-contiguous; pass np.ascontiguousarray(...) \
                 explicitly if you meant to copy",
            ));
        }
        let n = stamps.len();
        // SAFETY: checked C-contiguous above; the borrow is held across the
        // `detach` inside `eval_*` for the reason §6.2 gives — NumPy refuses to
        // resize an array while a buffer is exported.
        let src = unsafe { stamps.as_slice()? };
        if layout.is_f32() {
            let out = PyArray2::<f32>::zeros(py, [n, e], false);
            // SAFETY: freshly allocated here and contiguous by construction.
            let dst = unsafe { out.as_slice_mut()? };
            self.eval_f32(py, src, layout, dst)?;
            Ok(out.into_any())
        } else {
            let out = PyArray2::<f64>::zeros(py, [n, e], false);
            // SAFETY: as above.
            let dst = unsafe { out.as_slice_mut()? };
            self.eval_f64(py, src, layout, dst)?;
            Ok(out.into_any())
        }
    }

    /// [`PyPlan::at_into`]'s `layout=` path: validate `out`, then fill it.
    ///
    /// Everything is checked before a single element is written, exactly as the
    /// `mat4` path is (§5.3) — and the dispatch is on `stamps` first for the
    /// same reason it is there: probing `out` first blames the argument the
    /// caller got right.
    ///
    /// **The stamp dispatch is [`Self::at_layout`]'s, to the letter**, and that
    /// is a requirement rather than a tidiness: `docs/PHASE3.md` §3 is NORMATIVE
    /// that the accepted stamp types are `int`, an `np.int64` scalar and an
    /// `(N,)` `np.int64` array, and that a `float` meets the `TypeError`
    /// carrying the 238 ns ULP. An `if PyInt { .. } else { cast_or_BufferError }`
    /// gets **both** wrong — it refuses `np.int64` and it reports a `float` as a
    /// buffer problem — so the array cast is *fallen through* rather than
    /// `else`-d, and [`stamp_from_any`] is what has the last word.
    fn at_into_layout(
        &self,
        py: Python<'_>,
        stamps: &Bound<'_, PyAny>,
        out: &Bound<'_, PyAny>,
        layout: Layout,
        name: &str,
    ) -> PyResult<()> {
        let e = layout.elems();
        let src_arr = if stamps.is_instance_of::<pyo3::types::PyInt>() {
            None
        } else {
            stamps.cast::<PyArray1<i64>>().ok()
        };
        let scalar = src_arr.is_none();
        let src_owned = match &src_arr {
            Some(arr) => {
                if !arr.is_c_contiguous() {
                    return Err(BufferError::new_err(
                        "stamps must be C-contiguous; pass np.ascontiguousarray(...) \
                         explicitly if you meant to copy",
                    ));
                }
                [0i64]
            }
            None => [stamp_from_any(stamps)?],
        };
        // SAFETY: checked C-contiguous above; held across `eval_*`'s `detach`
        // per §6.2.
        let src: &[i64] = match &src_arr {
            Some(a) => unsafe { a.as_slice()? },
            None => &src_owned,
        };
        let n = src.len();
        let want: &[usize] = if scalar { &[e] } else { &[n, e] };

        if layout.is_f32() {
            let arr = cast_out::<f32>(out, layout, want, name)?;
            check_out(arr.as_untyped(), want)?;
            // SAFETY: `check_out` proved C-contiguous, correctly shaped and
            // writable; aliasing stays the caller's, exactly as `as_slice_mut`
            // documents. Nothing has been written yet.
            let dst = unsafe { arr.as_slice_mut()? };
            self.eval_f32(py, src, layout, dst)
        } else {
            let arr = cast_out::<f64>(out, layout, want, name)?;
            check_out(arr.as_untyped(), want)?;
            // SAFETY: as above.
            let dst = unsafe { arr.as_slice_mut()? };
            self.eval_f64(py, src, layout, dst)
        }
    }

    /// Fold `src` into `dst` in an `f64` layout, releasing the GIL if it pays.
    fn eval_f64(
        &self,
        py: Python<'_>,
        src: &[i64],
        layout: Layout,
        dst: &mut [f64],
    ) -> PyResult<()> {
        let plan = *self.plan;
        let tree = self.tree();
        let domain = self.domain;
        let mut run = || {
            let g = tree.guard();
            plan.at_many_into_tagged(&g, src, domain, layout, dst)
        };
        let res = if release_the_gil(src.len(), self.plan.len()) {
            py.detach(run)
        } else {
            run()
        };
        res.map_err(|e| lookup_err(tree, e))
    }

    /// [`Self::eval_f64`] for the one `f32` layout.
    fn eval_f32(
        &self,
        py: Python<'_>,
        src: &[i64],
        layout: Layout,
        dst: &mut [f32],
    ) -> PyResult<()> {
        let plan = *self.plan;
        let tree = self.tree();
        let domain = self.domain;
        let mut run = || {
            let g = tree.guard();
            plan.at_many_into_f32_tagged(&g, src, domain, layout, dst)
        };
        let res = if release_the_gil(src.len(), self.plan.len()) {
            py.detach(run)
        } else {
            run()
        };
        res.map_err(|e| lookup_err(tree, e))
    }
}

/// A claimed edge, and the only way to publish from Python.
///
/// # Why this is the shape it is
///
/// `tf_tree::Publisher` is **`Send + !Sync` by design** — Phase 1 makes
/// single-writer-per-edge a type-level property, so it cannot be a `#[pyclass]`
/// directly. `docs/PHASE3.md` §7.1 says to wrap it in a mutex, and the
/// semantics are exactly right: two Python threads pushing to one edge
/// serialize, which is the same guarantee the Rust type system gives, enforced
/// at a different level. An uncontended lock is ~15 ns against a `push` that
/// already costs more.
///
/// It also has to outlive the scope that created it, which is what
/// [`OwnedWriter`] is for: it carries its own `Arc<Tree>`, so the arena the
/// claim points into cannot go away underneath it and this crate holds no
/// lifetime of its own.
#[pyclass(name = "Publisher", module = "tf_tree")]
pub struct PyPublisher {
    /// How a failed `push` names this edge: `edge "map" -> "base"`.
    ///
    /// **The caller's own two strings, captured at claim time, rather than the
    /// `EdgeId` the writer carries.** Resolving that id back through the arena
    /// would reach the same pair by a longer route — and would reach the
    /// *stored* names, which `FrameRecord` truncates at 48 bytes, so a long
    /// name would come back cut in a message whose whole job is to be
    /// greppable against the caller's source.
    ///
    /// One `String`, allocated once per claim and never per push — a claim is a
    /// setup call, and `push` only *reads* this field, on the error path. So it
    /// is not the allocation `docs/API.md` R2 is about.
    edge: String,
    /// `None` after `__exit__` or `release()`, so a use-after-release is a
    /// clear Python error rather than a claim held past its scope.
    ///
    /// # Why an [`OwnedWriter`] and not a hand-rolled `EdgeWriter<'static>`
    ///
    /// Because there is exactly one lifetime extension in the workspace and it
    /// is not here (`docs/decisions/0017`). This field held an
    /// `EdgeWriter<'static>` produced by a local `extend_to_static`, and before
    /// that a type reinterpretation that was not a lifetime extension at all and
    /// dropped the claim lease and the fork guard on the floor. Both failures
    /// and the argument for centralising them are recorded in `0017` and on
    /// [`OwnedWriter`]; **this comment deliberately does not restate them**,
    /// because a second copy of a hazard's description drifts exactly the way a
    /// second copy of its code did — and because `0017` step 6's stated
    /// verification is a grep for the old spelling over this crate, which a
    /// comment quoting it would defeat.
    ///
    /// `OwnedWriter` reproduces every guard by *containing* the `EdgeWriter`
    /// whole, so the count cannot drift: `push` here is
    /// [`OwnedWriter::push`](tf_tree::OwnedWriter::push), which forwards to the
    /// fork-checked `EdgeWriter::push`.
    inner: Mutex<Option<OwnedWriter>>,
}

#[pymethods]
impl PyPublisher {
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Release the claim on scope exit.
    ///
    /// The context manager is the documented form (§4.3). `Drop` would release
    /// it too, but Python's finalization order is not guaranteed and an edge
    /// held until the interpreter feels like collecting is an edge no other
    /// process can claim.
    #[pyo3(signature = (*_args))]
    fn __exit__(&self, _args: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.release()?;
        Ok(false)
    }

    /// Drop the claim now.
    fn release(&self) -> PyResult<()> {
        let mut g = self.lock()?;
        *g = None;
        Ok(())
    }

    /// Publish `[qw, qx, qy, qz, tx, ty, tz]` at `stamp_ns`.
    #[pyo3(signature = (stamp_ns, quat7, /))]
    fn push(&self, stamp_ns: i64, quat7: Vec<f64>) -> PyResult<()> {
        let iso = iso_from_quat7(&quat7)?;
        let g = self.lock()?;
        let p = g.as_ref().ok_or_else(released)?;
        p.push(stamp_ns, &iso).map_err(|e| push_err(&self.edge, e))
    }

    /// Publish a whole batch: `(N,)` stamps and `(N, 7)` poses.
    ///
    /// The loop is in Rust rather than in Python. There is no batched write in
    /// the engine — `SampleRing::push` is one sample at a time by design, since
    /// each publication is an independent release-store that a reader may
    /// observe — so this does not avoid the per-sample work. What it avoids is
    /// N crossings of the FFI boundary, which at ~30 ns each is the entire
    /// difference for a replayed log.
    #[pyo3(signature = (stamps, poses, /))]
    fn push_many(
        &self,
        stamps: &Bound<'_, PyArray1<i64>>,
        poses: &Bound<'_, PyArray2<f64>>,
    ) -> PyResult<()> {
        let n = stamps.len();
        if !stamps.is_c_contiguous() || !poses.is_c_contiguous() {
            return Err(BufferError::new_err(
                "stamps and poses must be C-contiguous",
            ));
        }
        if poses.shape() != [n, 7] {
            return Err(BufferError::new_err(format!(
                "poses must have shape ({n}, 7) as [qw qx qy qz tx ty tz], got {:?}",
                poses.shape()
            )));
        }
        // SAFETY: both arrays were just checked contiguous and are borrowed
        // only for the duration of this call.
        let (st, po) = unsafe { (stamps.as_slice()?, poses.as_slice()?) };

        let g = self.lock()?;
        let p = g.as_ref().ok_or_else(released)?;
        for (i, stamp) in st.iter().enumerate() {
            let iso = iso_from_quat7(&po[i * 7..(i + 1) * 7])?;
            p.push(*stamp, &iso).map_err(|e| {
                // Name the index: a rejected sample partway through a batch is
                // otherwise indistinguishable from a rejected batch, and the
                // samples before it *were* published. Prefixed rather than
                // re-worded, so the sentence after the colon is the same one a
                // scalar `push` produces for the same failure.
                TfTreeError::new_err(format!(
                    "sample {i} (stamp {stamp}): {}",
                    push_msg(&self.edge, e)
                ))
            })?;
        }
        Ok(())
    }

    fn __repr__(&self) -> String {
        let held = self.inner.lock().map(|g| g.is_some()).unwrap_or(false);
        format!("<tf_tree.Publisher held={}>", py_bool(held))
    }
}

impl PyPublisher {
    fn lock(&self) -> PyResult<std::sync::MutexGuard<'_, Option<OwnedWriter>>> {
        self.inner
            .lock()
            .map_err(|_| TfTreeError::new_err("publisher mutex was poisoned by a panic"))
    }
}

/// DLPack device types that a CPU kernel may write to (`docs/PHASE3.md` §5.5).
///
/// From the DLPack ABI: 1 = `kDLCPU`, 3 = `kDLCUDAHost` (pinned), 11 =
/// `kDLROCMHost`, 13 = `kDLCUDAManaged`. Everything else is device memory, and
/// **a CPU store to a `cudaMalloc` pointer is undefined** — not slow, undefined.
const HOST_DEVICE_TYPES: [i32; 4] = [1, 3, 11, 13];

/// Refuse an `out` buffer that does not live where a CPU can write it.
///
/// # Why DLPack for this and the buffer protocol for the rest
///
/// Device placement is the one property only DLPack reports portably.
/// `__dlpack_device__()` returns `(device_type, device_id)` cheaply, without
/// consuming the object and without a CUDA runtime — which decision D8 forbids
/// us from taking as a dependency. `cudaPointerGetAttributes` would answer the
/// same question and cost exactly that dependency.
///
/// Mutability and contiguity come from the buffer protocol instead, because it
/// has conveyed writability since PEP 3118 and cannot get it wrong, whereas
/// older DLPack has no read-only bit at all.
///
/// # What this check is actually for
///
/// Almost every host-accessible buffer reports `kDLCPU` — PyTorch calls pinned
/// tensors `device='cpu'`, and `cupyx.empty_pinned` returns a NumPy array. So
/// this does not enable an exotic path. **Its job is to produce a good error
/// instead of a segfault**, and that is worth the twenty lines on its own.
fn reject_device_memory(obj: &Bound<'_, PyAny>) -> PyResult<()> {
    let Ok(f) = obj.getattr("__dlpack_device__") else {
        // No DLPack at all. The buffer protocol below still validates
        // writability and contiguity, and an object with neither is refused
        // there — naming both protocols.
        return Ok(());
    };
    let Ok(dev) = f.call0() else { return Ok(()) };
    let Ok((device_type, device_id)) = dev.extract::<(i32, i32)>() else {
        return Ok(());
    };
    if HOST_DEVICE_TYPES.contains(&device_type) {
        return Ok(());
    }
    Err(BufferError::new_err(format!(
        "out lives on DLPack device type {device_type} (id {device_id}), which a \
         CPU kernel cannot write to. Allocate pinned host memory instead — \
         torch.empty(..., pin_memory=True), cupyx.empty_pinned(...) or \
         numba.cuda.pinned_array(...) — and copy to the device yourself; the \
         adaptive knot array is about a kilobyte, so that transfer is ~6 us and \
         is not what limits you."
    )))
}

/// Downcast `out` for a `layout=` write, or say exactly what was wanted.
///
/// `PyArrayDyn` rather than a fixed rank because the scalar overload wants
/// `(elems,)` and the batch overload `(N, elems)`; the rank is then checked with
/// the shape by [`check_out`], in one message instead of two. The **dtype** is
/// still checked here, by the downcast itself, which is what keeps an
/// `affine32` write out of a `float64` buffer.
///
/// A failed downcast falls through to [`reject_device_memory`] first, for the
/// same reason `at_into`'s `mat4` path does: a CuPy or torch allocation is not
/// a numpy subclass, so it lands here, and a CPU store to a `cudaMalloc`
/// pointer is undefined rather than slow.
fn cast_out<'a, 'py, T: numpy::Element>(
    out: &'a Bound<'py, PyAny>,
    layout: Layout,
    want: &[usize],
    name: &str,
) -> PyResult<&'a Bound<'py, numpy::PyArrayDyn<T>>> {
    if let Ok(arr) = out.cast::<numpy::PyArrayDyn<T>>() {
        return Ok(arr);
    }
    reject_device_memory(out)?;
    let dtype = if layout.is_f32() {
        "float32"
    } else {
        "float64"
    };
    Err(BufferError::new_err(format!(
        "layout={name:?} needs out to be a writable, C-contiguous {want:?} {dtype} \
         numpy array. Other buffer-protocol objects are not accepted yet; \
         np.asarray(...) it first"
    )))
}

/// The three checks every `out` buffer passes before a single element is
/// written (§5.3): contiguous, the right shape, and writable.
///
/// Shared by both `layout=` overloads so the order — and therefore which
/// complaint a caller with two problems hears first — cannot drift between
/// them.
fn check_out(arr: &Bound<'_, numpy::PyUntypedArray>, want: &[usize]) -> PyResult<()> {
    if !arr.is_c_contiguous() {
        return Err(BufferError::new_err(
            "out must be C-contiguous; pass np.ascontiguousarray(...) explicitly \
             if you meant to copy",
        ));
    }
    let shape = arr.shape();
    if shape != want {
        return Err(BufferError::new_err(format!(
            "out must have shape {want:?}, got {shape:?}"
        )));
    }
    if !is_writeable(arr) {
        return Err(BufferError::new_err(
            "out is not writable (NumPy reports NPY_ARRAY_WRITEABLE clear); \
             a read-only mapping cannot receive a transform",
        ));
    }
    Ok(())
}

/// Whether NumPy marks this array writable.
///
/// **This is the check whose absence made a read-only `np.memmap` a `SIGSEGV`
/// rather than an error**, and made a `flags.writeable = False` array get
/// silently overwritten. `as_slice_mut` is `unsafe` precisely because it checks
/// neither this nor aliasing, and §5.5's rule — refuse rather than fault —
/// applies to host memory the caller cannot write just as much as to device
/// memory.
///
/// One field read, which is what `is_c_contiguous` also does.
fn is_writeable(arr: &Bound<'_, numpy::PyUntypedArray>) -> bool {
    // SAFETY: `as_array_ptr` returns this array's live `PyArrayObject` for the
    // lifetime of the borrow; `flags` is a plain `c_int` field and is only read.
    unsafe { (*arr.as_array_ptr()).flags & numpy::npyffi::NPY_ARRAY_WRITEABLE != 0 }
}

/// Render a bool the way Python spells it, for `__repr__`.
fn py_bool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

fn released() -> PyErr {
    TfTreeError::new_err("this publisher's claim was already released")
}

fn iso_from_quat7(q: &[f64]) -> PyResult<tf_tree::Iso3> {
    if q.len() != 7 {
        return Err(PyValueError::new_err(
            "expected [qw, qx, qy, qz, tx, ty, tz]",
        ));
    }
    Ok(tf_tree::Iso3::new(
        tf_tree::Quat {
            w: q[0],
            x: q[1],
            y: q[2],
            z: q[3],
        },
        tf_tree::Vec3::new(q[4], q[5], q[6]),
    ))
}

/// Parse the `interp=` keyword — `docs/PHASE3.md` §4.1's `interp="sclerp"`.
///
/// A builder-time keyword on a startup call, so R2 is not in tension. The two
/// spellings are the two `InterpPolicy` variants and there is deliberately no
/// third: a name this does not know is refused rather than silently defaulted,
/// because the whole difference between them is invisible in the output.
fn interp_from_str(name: &str) -> PyResult<InterpPolicy> {
    match name {
        "sclerp" => Ok(InterpPolicy::ScLerp),
        "lerpslerp" => Ok(InterpPolicy::LerpSlerp),
        other => Err(PyValueError::new_err(format!(
            "unknown interp {other:?}; expected 'sclerp' (SE(3) screw geodesic, \
             the engine default, and the only one with an exact derivative) or \
             'lerpslerp' (tf2-compatible)"
        ))),
    }
}

/// [`interp_from_str`] backwards, for the one error that has to name a policy.
///
/// Kept adjacent to its inverse rather than next to its caller, because the two
/// are a pair: a spelling added to one and not the other is how
/// `DerivativesUnavailableError` came to say `interpolation policy 1` — a
/// number the caller never typed and cannot pass back — while the constructor
/// two lines up knew the word for it all along.
///
/// `InterpPolicy` is deliberately not `#[non_exhaustive]` (see its doc comment
/// in `tf_tree_core::plan`), so a third policy is a compile error here. That is
/// the whole reason it can be exhaustive when [`crate::errors::lookup_err`]
/// cannot.
pub(crate) fn interp_name(policy: InterpPolicy) -> &'static str {
    match policy {
        InterpPolicy::ScLerp => "sclerp",
        InterpPolicy::LerpSlerp => "lerpslerp",
    }
}

/// Build an in-process tree from a simple edge list.
///
/// Topology is builder-time (decision `0004`), so there is no `declare_*` on a
/// live tree: the layout is a property of the arena, fixed when it is created.
///
/// # `interp=`, and why its default is the engine's
///
/// **`"sclerp"`**, which is `tf_tree::TreeBuilder`'s own default and what
/// `docs/PROJECT.md` §5 D5 requires — *do not* make `LerpSlerp` the default
/// without a measurement justifying it. This binding hard-coded `LerpSlerp`
/// from Phase 3 until now and no such measurement was ever recorded, so the
/// divergence was a mistake rather than a decision — and it is not cosmetic.
/// LERP+SLERP is left-invariant but **not** right-invariant, so a Python caller
/// on the old default got interpolation failing an invariance the Rust caller's
/// default satisfies, while `docs/API.md` §3 promised the Python surface
/// diverges only in the two places it lists.
///
/// **This changes the numbers a caller who passed no `interp=` was getting.**
/// It is a one-line break made before a published tag rather than a permanent
/// divergence after one. `interp="lerpslerp"` stays, and is the right answer for
/// bit-compatibility with `tf2` — which D5 says is what `LerpSlerp` is *for*.
///
/// The keyword also has an observable consequence beyond the numbers:
/// **`LerpSlerp` has no exact body twist**, so
/// `plan.at(stamps, layout="quat_twist")` over a `lerpslerp` tree raises
/// `DerivativesUnavailableError`. With `ScLerp` the default, that layout works
/// out of the box. `docs/PHASE3.md` §4.1 spells this keyword the same way in
/// its own layout sketch.
///
/// # `frame_headroom=`, and why zero was not a safe default to be stuck with
///
/// Spare **frame-name** slots, `TreeBuilder::frame_headroom` under the same
/// name. The frame table is sized from the declared topology and never grows
/// (invariant 3), and `frame_headroom` is the only way to leave room for a name
/// interned *after* the arena exists. With the 0 this had no way to change, an
/// arena created from Python could never accept one from any participant: a
/// Rust or C peer — or the ROS ingest bridge, which reserves eight for exactly
/// this — calling `Tree::frame()` on it gets `CapacityExceeded` forever.
///
/// It is also what makes the arena-mutation regression in
/// `tests/python/test_errors.py` observable at all: below the capacity
/// pre-check in `intern_core`, a failed intern writes nothing, so a binding that
/// interned on an error path looked innocent on every zero-headroom tree.
///
/// There is deliberately **no `edge_headroom`**, on `docs/PHASE5.md` §5.8's
/// amendment: nothing declares an edge at runtime, so the slots it reserves can
/// only ever be empty (the ROS bridge's config makes the same call).
/// # `edges` also takes a topology config, and that is the only way to declare
/// a real robot
///
/// A list of `(parent, child)` pairs makes every edge **dynamic** under one
/// capacity, which cannot express a static edge, a per-edge size, a declared
/// rate, or a per-edge domain. Passing the *text* of a topology config instead
/// — the same schema `ros/tf_tree_ros` starts from and `tf_tree topology
/// --discover` writes — expresses all four
/// ([`0041`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0041-python-declares-a-topology-the-way-everything-else-does.md)).
///
/// `capacity=` and `interp=` are refused beside a config, because the config
/// carries both and there would otherwise be no saying which won.
#[pyfunction]
#[pyo3(signature = (edges, *, capacity = None, interp = None, frame_headroom = 0))]
pub fn build(
    edges: &Bound<'_, PyAny>,
    capacity: Option<u32>,
    interp: Option<&str>,
    frame_headroom: u32,
) -> PyResult<PyTree> {
    // A `str` is never a valid edge list, so the dispatch is unambiguous and
    // needs no second keyword (`0041`).
    // `String`, not `&str`: `extract::<&str>` ties the borrow to the `Bound` in a
    // way PyO3 0.29 will not resolve under every feature set this crate is built
    // with — `just py-cross-check`'s Apple and Windows targets refuse it. One
    // allocation on a construction path is not a cost worth a `cfg`.
    if let Ok(text) = edges.extract::<String>() {
        if capacity.is_some() || interp.is_some() {
            return Err(PyValueError::new_err(
                "capacity= and interp= are not accepted with a topology config: the \
                 config carries both, per-edge, and there would be no saying which won. \
                 Set them in the config, or pass a list of (parent, child) pairs.",
            ));
        }
        return build_from_config(&text, frame_headroom);
    }
    let edges: Vec<(String, String)> = edges.extract().map_err(|_| {
        PyTypeError::new_err(
            "edges must be a list of (parent, child) string pairs, or the text of a \
             topology config",
        )
    })?;
    let capacity = capacity.unwrap_or(1024);
    let mut b = tf_tree::TreeBuilder::new()
        .default_interp(interp_from_str(interp.unwrap_or("sclerp"))?)
        .frame_headroom(frame_headroom);
    for (parent, child) in &edges {
        b = b.dynamic_edge(parent, child, EdgeCfg::new(Capacity::slots(capacity)));
    }
    let inner = b.build().map_err(|e| build_err(&edges, capacity, e))?;
    Ok(PyTree {
        inner: Arc::new(inner),
    })
}

/// Build from topology-config text — `0041`.
///
/// **The error is rendered here, while `text` is still alive.** `ConfigError`
/// borrows from the config source so it can name the offending frame without
/// allocating (`config.rs`'s own argument), which means it cannot outlive this
/// function. Formatting it into the Python exception is what converts a borrowed
/// diagnostic into an owned one.
fn config_builder(text: &str, frame_headroom: u32) -> PyResult<tf_tree::TreeBuilder> {
    let cfg = tf_tree_bridge::TopologyConfig::parse(text)
        .map_err(|e| PyValueError::new_err(format!("topology config: {e}")))?;

    // **Ask the config before asking the builder**, exactly as
    // `tf_tree_cli::topology` and `tf_tree_c::bridge` do against this same
    // schema. The parser rejects a self-edge and a duplicate child but not a
    // multi-hop cycle, and `build()` finds that one and reports it as
    // `WouldCreateCycle { child: FrameId(1) }` — an index into an arena that was
    // never constructed, which is the one thing a caller holding a *text file*
    // cannot resolve. `cycle_child` exists for this and its own doc says so;
    // Python was the third consumer of the schema and the only one that had
    // regressed the diagnostic.
    if let Some(child) = cfg.cycle_child() {
        return Err(PyValueError::new_err(format!(
            "topology config: the declared topology has a cycle through frame \
             {child:?} — following its parent links returns to it"
        )));
    }

    let mut b = cfg.builder();
    // The config has its own `frame_headroom`; a non-zero argument overrides it,
    // and zero — the default — leaves whatever the config asked for.
    if frame_headroom != 0 {
        b = b.frame_headroom(frame_headroom);
    }
    Ok(b)
}

/// Build a heap tree from topology-config text — `0041`.
fn build_from_config(text: &str, frame_headroom: u32) -> PyResult<PyTree> {
    let inner = config_builder(text, frame_headroom)?
        .build()
        // `config_build_err`, not `build_err`: the latter's prose is written
        // about an *edge list* the caller passed, and a config caller passed
        // none — see its own doc for the three ways that goes wrong.
        .map_err(config_build_err)?;
    Ok(PyTree {
        inner: Arc::new(inner),
    })
}

/// A `BuildError` from a config, phrased for somebody holding a text file.
///
/// **Not `build_err`.** That function's prose is written about the `edges=` list
/// and the `capacity=` keyword: given the empty list and placeholder capacity a
/// config path would have to hand it, it says "0 pairs", advises "lower
/// capacity=1024" — a keyword this path *refuses* — and, for a topology error it
/// cannot find a cycle in, concludes *"that is a bug in tf_tree rather than in
/// your call"* about a cycle the caller wrote themselves.
///
/// `TfTreeError` and not `PyValueError`, so `except tf_tree.TfTreeError` catches
/// a build failure from either construction form. The list path has always
/// raised it, and a config path that raised something else would be caught by
/// nobody's existing handler.
fn config_build_err(e: tf_tree::BuildError) -> PyErr {
    crate::errors::TfTreeError::new_err(match e {
        tf_tree::BuildError::Topology(inner) => format!(
            "the declared topology does not wire up: {inner}. Every frame but the \
             root needs exactly one parent, and the parent links must reach it."
        ),
        tf_tree::BuildError::TooManyFrames | tf_tree::BuildError::TooManyEdges => {
            "the declared topology is too large for the u32 id space".to_string()
        }
        other => format!(
            "the declared topology does not build: {other:?}. Its sizing comes \
             from the config's own rate_hz/history_secs or capacity, per edge."
        ),
    })
}

/// Publish one sample onto an edge, for tests and simple producers.
///
/// Takes `[qw qx qy qz tx ty tz]` rather than a 4x4. A matrix would have to be
/// converted back to a quaternion, and a *nearly* rigid matrix — which is what
/// arrives after any floating-point round trip — has no exact conversion, only
/// a projection. Taking the engine's own representation makes the input
/// unambiguous instead of silently re-orthonormalising the caller's data.
#[pyfunction]
#[pyo3(signature = (tree, child, parent, stamp_ns, quat7, /))]
pub fn push(
    tree: &PyTree,
    child: &str,
    parent: &str,
    stamp_ns: i64,
    quat7: Vec<f64>,
) -> PyResult<()> {
    if quat7.len() != 7 {
        return Err(PyValueError::new_err(
            "expected [qw, qx, qy, qz, tx, ty, tz]",
        ));
    }
    // See `Tree.publisher`: builder-time topology means interning a name that
    // has no record cannot produce an edge to publish on.
    let c = resolve_frame(&tree.inner, child)?;
    let p = resolve_frame(&tree.inner, parent)?;
    let iso = tf_tree::Iso3::new(
        tf_tree::Quat {
            w: quat7[0],
            x: quat7[1],
            y: quat7[2],
            z: quat7[3],
        },
        tf_tree::Vec3::new(quat7[4], quat7[5], quat7[6]),
    );
    let publisher = tree
        .inner
        .claim(c, p)
        .map_err(|e| claim_err(&tree.inner, parent, child, e))?;
    publisher
        .push(stamp_ns, &iso)
        .map_err(|e| push_err(&edge_label_of(parent, child), e))
}

/// Attach to a running arena (`docs/PHASE3.md` §4.1).
///
/// **`mode="ro"`, and creation off** — both differ from the Rust in-process
/// defaults on purpose (D18). Most Python consumers are notebooks, analysis
/// scripts and visualisers; they must be *incapable* of corrupting a robot's
/// transform tree, which a `PROT_READ` mapping enforces with the MMU. And a
/// notebook started before the robot must fail loudly rather than create an
/// empty arena the real publisher then refuses to join.
///
/// **`domain=` here is the rendezvous domain**, a `u32` naming which arena to
/// attach to (`$ROS_DOMAIN_ID`'s analogue). It is *not* `tf_tree.Tree.plan`'s
/// `domain=`, which is the `u8` time-domain tag of the edges inside that arena
/// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`). Two unrelated
/// numbers that share a word, and the pair is spelled out in both places
/// because a caller who conflates them gets an empty arena rather than an error.
///
/// # Creating
///
/// Pass `create=[(parent, child), ...]` — the same edge list [`build`] takes —
/// to create the arena when it is absent. Decision `0004` sizes an arena from
/// its declared edges, so there is no way to create one without saying what is
/// in it; that is why this is an edge list and not a boolean.
///
/// `capacity`, `interp` and `frame_headroom` describe the edges being created —
/// they are [`build`]'s, with the same `"sclerp"` default and the same
/// `layout="quat_twist"` consequence. Without `create` they describe nothing,
/// but `interp` is still **parsed**: accepting `open(interp="screw")` silently
/// while `build(interp="screw")` refuses it would make the same typo a startup
/// error in one call and a no-op in the other.
///
/// `frame_headroom` matters more here than it does on [`build`], and this is the
/// call it was missing from: a *shared* arena is the one other processes attach
/// to, and `Tree::frame()` from any of them needs a spare slot to intern a name
/// into. Zero — which is what this used to be, with no way to say otherwise —
/// means a Python-created arena refuses every runtime frame name for its whole
/// life.
///
/// **Creating requires `mode="rw"`**, and is refused otherwise rather than
/// quietly ignored. Both of §4.1's reasons for the read-only default survive
/// that: a `ro` consumer still cannot bring an arena into existence, and an
/// `rw` publisher — which has already opted into being able to corrupt the tree
/// — still has to ask.
// **Linux-only, and it refuses rather than vanishing.** The whole shared-arena
// surface — `Open`, `CreatePolicy`, `AttachMode` — is
// `#[cfg(all(feature = "shm", target_os = "linux"))]` in the facade, and this
// crate always enables `shm`, so the *target* is what decides. The paired
// `#[cfg(not(...))]` arm below keeps the attribute present on every platform
// for the reason `offline.rs` already records: a missing attribute makes a
// portable script fail with `AttributeError` at a line that has nothing to do
// with the reason.
#[cfg(target_os = "linux")]
#[pyfunction]
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = None, interp = None, frame_headroom = 0))]
pub fn open_arena(
    name: Option<&str>,
    domain: Option<u32>,
    mode: &str,
    create: Option<&Bound<'_, PyAny>>,
    capacity: Option<u32>,
    interp: Option<&str>,
    frame_headroom: u32,
) -> PyResult<PyTree> {
    let attach = match mode {
        "ro" => AttachMode::ReadOnly,
        "rw" => AttachMode::ReadWrite,
        other => {
            return Err(PyValueError::new_err(format!(
                "mode must be 'ro' or 'rw', got {other:?}"
            )))
        }
    };
    if create.is_some() && attach == AttachMode::ReadOnly {
        return Err(PyValueError::new_err(
            "create= requires mode='rw': a read-only participant cannot write \
             the arena it would have created",
        ));
    }
    // **Validated whether or not a layout is being created.** A misspelled
    // policy is a startup error under `build` and must be one here too; folding
    // this into the `if let` below made `open(interp="screw")` a silent no-op,
    // which is the one shape of a keyword nobody notices they got wrong.
    //
    // This read "before `create` is consulted" and sat directly above the
    // `interp_from_str` call. `0041` put the `create=` type dispatch between the
    // two, so the wording stopped being true of the lines under it and the
    // ordering changed with it: `open(create=42, interp="screw")` now reports
    // the `create=` type error rather than the misspelled policy. Both are
    // startup errors naming a keyword the caller got wrong, so which comes first
    // is not worth reordering the dispatch for — but the comment claimed an
    // order it no longer had, which is worse than either.
    // `create=` takes the same two forms `build`'s `edges` does (`0041`): a list
    // of pairs, or the text of a topology config. A `str` is never a valid edge
    // list, so the dispatch needs no second keyword.
    // Owned, for the same reason `build` takes an owned one.
    let config: Option<String> = create.and_then(|c| c.extract::<String>().ok());
    if config.is_some() && (capacity.is_some() || interp.is_some()) {
        return Err(PyValueError::new_err(
            "capacity= and interp= are not accepted with a topology config: the config \
             carries both, per-edge. Set them in the config, or pass a list of \
             (parent, child) pairs.",
        ));
    }
    let pairs: Option<Vec<(String, String)>> = match (create, &config) {
        (Some(c), None) => Some(c.extract().map_err(|_| {
            PyTypeError::new_err(
                "create= must be a list of (parent, child) string pairs, or the text of \
                 a topology config",
            )
        })?),
        _ => None,
    };
    let capacity = capacity.unwrap_or(1024);
    let policy = interp_from_str(interp.unwrap_or("sclerp"))?;
    let mut o = tf_tree::Open::new().mode(attach).create(match &create {
        None => tf_tree::CreatePolicy::Never,
        Some(_) => tf_tree::CreatePolicy::IfAbsent,
    });
    if let Some(text) = &config {
        // The same preflight and the same builder `build` uses, so the two
        // construction forms cannot diverge on a diagnostic.
        o = o.layout_if_creating(config_builder(text, frame_headroom)?);
    } else if let Some(edges) = &pairs {
        let mut b = tf_tree::TreeBuilder::new()
            .default_interp(policy)
            .frame_headroom(frame_headroom);
        for (parent, child) in edges {
            b = b.dynamic_edge(parent, child, EdgeCfg::new(Capacity::slots(capacity)));
        }
        o = o.layout_if_creating(b);
    }
    if let Some(d) = domain {
        o = o.domain(d);
    }
    // Both failures route through the same mapper, because `Open::name` returns
    // an `OpenError` too — a rejected arena name is `Rendezvous(IpcError)`, and
    // that arm was already prose. What was not is `Build`, which carries every
    // one of `BuildError`'s Debug dumps into the call a consumer makes first.
    // **A config path must not reach `open_err`'s build prose.** That prose is
    // written about the `create=` edge list and the `capacity=` keyword, and a
    // config caller passed neither — so it would report "0 pairs", advise
    // lowering a keyword this path refuses, and, for a topology error, blame
    // `tf_tree` for a cycle the caller wrote. Rendezvous failures are shared:
    // they are about the arena, not about how the layout was declared.
    let created: &[(String, String)] = pairs.as_deref().unwrap_or(&[]);
    let map_err = |e: tf_tree::OpenError| match (&config, e) {
        (Some(_), tf_tree::OpenError::Build(inner)) => config_build_err(inner),
        (_, e) => open_err(created, capacity, e),
    };
    if let Some(n) = name {
        o = o.name(n).map_err(&map_err)?;
    }
    let inner = o.open().map_err(&map_err)?;
    Ok(PyTree {
        inner: Arc::new(inner),
    })
}

/// See [`open_arena`]. The shared arena is Linux-only, like the `memfd` it maps.
///
/// Present on every platform on purpose — `offline.rs` records the argument: an
/// absent attribute makes a portable script fail with `AttributeError` at a line
/// that has nothing to do with the reason, and this one has a real reason to
/// give.
#[cfg(not(target_os = "linux"))]
#[pyfunction]
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = None, interp = None, frame_headroom = 0))]
#[allow(clippy::needless_pass_by_value)]
pub fn open_arena(
    name: Option<&str>,
    domain: Option<u32>,
    mode: &str,
    // **These must track the Linux signature, or the stub defeats itself.** It
    // exists so a macOS or Windows caller meets the message below rather than a
    // PyO3 conversion error at a line that has nothing to do with the reason —
    // and a `str` config (a sequence of one-character strings) cannot extract to
    // `Vec<(String, String)>`, so a stale signature would have given exactly the
    // generic `TypeError` this stub is here to prevent.
    create: Option<&Bound<'_, PyAny>>,
    capacity: Option<u32>,
    interp: Option<&str>,
    frame_headroom: u32,
) -> PyResult<PyTree> {
    let _ = (name, domain, mode, create, capacity, interp, frame_headroom);
    Err(crate::errors::TfTreeError::new_err(
        "a shared tf_tree arena needs the mmap-backed backend, which is \
         Linux-only in this build; tf_tree.build(...) works everywhere",
    ))
}
