//! `Tree` and `Plan` (`docs/PHASE3.md` §4).

use numpy::{PyArray1, PyArray2, PyArray3, PyArrayMethods, PyUntypedArrayMethods};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Layout, OwnedWriter, Stamp, SystemDomain, Tree};

#[cfg(target_os = "linux")]
use crate::errors::open_err;
#[cfg(target_os = "linux")]
use tf_tree::AttachMode;

use crate::errors::{
    build_err, claim_err, edge_label_of, lookup_err, push_err, push_msg, resolve_frame,
    unknown_frame_err, BufferError, TfTreeError,
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
    #[pyo3(signature = (target, source, /))]
    fn plan(slf: &Bound<'_, PyTree>, target: &str, source: &str) -> PyResult<PyPlan> {
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
        Ok(PyPlan {
            plan: Box::new(plan),
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
    #[pyo3(signature = (target, source, stamp_ns, /))]
    fn lookup<'py>(
        &self,
        py: Python<'py>,
        target: &str,
        source: &str,
        stamp_ns: i64,
    ) -> PyResult<Bound<'py, PyArray2<f64>>> {
        let iso = self
            .inner
            .lookup(target, source, Stamp::<SystemDomain>::from_nanos(stamp_ns))
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
/// `Plan` is `align(64), size 2112` — it holds `[Step; MAX_DEPTH]` and `Step`
/// carries an `Iso3`, which is one cacheline by design. **CPython's object
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
            .at(&g, Stamp::<SystemDomain>::from_nanos(stamp))
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
                .at(&g, Stamp::<SystemDomain>::from_nanos(stamp))
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
        let mut scratch = tf_tree::AdaptiveScratch::<SystemDomain>::new();
        let tol = tf_tree::ErrBound::new(ang, lin);
        let g = self.tree().guard();
        let (stamps, poses) = self
            .plan
            .at_adaptive(
                &g,
                (Stamp::from_nanos(start_ns), Stamp::from_nanos(end_ns)),
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

        let mut run = || {
            let g = tree.guard();
            // Raw nanoseconds: `at_many_into` takes `&[i64]` precisely so this
            // path does not have to allocate a `Vec<Stamp>` — which would be
            // the intermediate buffer `at_into` exists to avoid.
            plan.at_many_into::<SystemDomain>(&g, src, Layout::Mat4, dst)
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
        let mut run = || {
            let g = tree.guard();
            plan.at_many_into::<SystemDomain>(&g, src, layout, dst)
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
        let mut run = || {
            let g = tree.guard();
            plan.at_many_into_f32::<SystemDomain>(&g, src, layout, dst)
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
#[pyfunction]
#[pyo3(signature = (edges, *, capacity = 1024, interp = "sclerp", frame_headroom = 0))]
pub fn build(
    edges: Vec<(String, String)>,
    capacity: u32,
    interp: &str,
    frame_headroom: u32,
) -> PyResult<PyTree> {
    let mut b = tf_tree::TreeBuilder::new()
        .default_interp(interp_from_str(interp)?)
        .frame_headroom(frame_headroom);
    for (parent, child) in &edges {
        b = b.dynamic_edge(parent, child, EdgeCfg::new(Capacity::slots(capacity)));
    }
    let inner = b.build().map_err(|e| build_err(&edges, capacity, e))?;
    Ok(PyTree {
        inner: Arc::new(inner),
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
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = 1024, interp = "sclerp", frame_headroom = 0))]
pub fn open_arena(
    name: Option<&str>,
    domain: Option<u32>,
    mode: &str,
    create: Option<Vec<(String, String)>>,
    capacity: u32,
    interp: &str,
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
    // **Parsed unconditionally, before `create` is consulted.** A misspelled
    // policy is a startup error under `build` and must be one here too; folding
    // this into the `if let` below made `open(interp="screw")` a silent no-op,
    // which is the one shape of a keyword nobody notices they got wrong.
    let policy = interp_from_str(interp)?;
    let mut o = tf_tree::Open::new().mode(attach).create(match &create {
        None => tf_tree::CreatePolicy::Never,
        Some(_) => tf_tree::CreatePolicy::IfAbsent,
    });
    if let Some(edges) = &create {
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
    let created = create.as_deref().unwrap_or(&[]);
    if let Some(n) = name {
        o = o.name(n).map_err(|e| open_err(created, capacity, e))?;
    }
    let inner = o.open().map_err(|e| open_err(created, capacity, e))?;
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
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = 1024, interp = "sclerp", frame_headroom = 0))]
#[allow(clippy::needless_pass_by_value)]
pub fn open_arena(
    name: Option<&str>,
    domain: Option<u32>,
    mode: &str,
    create: Option<Vec<(String, String)>>,
    capacity: u32,
    interp: &str,
    frame_headroom: u32,
) -> PyResult<PyTree> {
    let _ = (name, domain, mode, create, capacity, interp, frame_headroom);
    Err(crate::errors::TfTreeError::new_err(
        "a shared tf_tree arena needs the mmap-backed backend, which is \
         Linux-only in this build; tf_tree.build(...) works everywhere",
    ))
}
