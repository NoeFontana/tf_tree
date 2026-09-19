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
    build_err, claim_err, detached_err, edge_label_of, lookup_err, plan_domain_err, push_class,
    push_err, push_msg, resolve_frame, unknown_frame_err, BufferError, TfTreeError,
};

/// Releasing the GIL costs ~40 ns against a ~193 ns depth-3 lookup (`docs/PHASE3.md` §6.1's
/// amendment), so a scalar keeps it and a large batch releases it. The rule is in estimated
/// work, because depth varies: below the threshold the GIL is held under ~1 µs, above it the
/// release overhead is <=4%. Both sides are cheap, so the constant needs no tuning.
pub(crate) const GIL_RELEASE_THRESHOLD_NS: u64 = 1_000;
/// Rough per-step cost placing the threshold above; `docs/PHASE3.md` §6.1's amendment is the
/// single account of its derivation.
const NS_PER_STEP_ESTIMATE: u64 = 64;

/// §6.1's rule, in one place so the two callers cannot drift apart.
///
/// `Layout::QuatTwist` gets no multiplier: `est` under-estimates a pose batch by ~1.7x and a
/// twist batch by ~1.9x (depth 3, n = 4000, `ScLerp`), so correcting only the twist fixes the
/// smaller error. The residual is in the safe direction (`est` too low releases later, never
/// sooner) and stays orders of magnitude under CPython's 5 ms switch interval.
#[inline]
const fn release_the_gil(n: usize, depth: usize) -> bool {
    let est = (n as u64)
        .saturating_mul(depth as u64)
        .saturating_mul(NS_PER_STEP_ESTIMATE);
    est >= GIL_RELEASE_THRESHOLD_NS
}

/// The depth-3 crossover, pinned at compile time: at 64 ns/step a depth-3 batch releases from
/// `n = 6` and not at `n = 5` (`docs/PHASE3.md` §6.1). An assertion, not a `#[test]`, because
/// this crate is outside the workspace and every build of it evaluates this.
const _: () = assert!(
    release_the_gil(6, 3) && !release_the_gil(5, 3),
    "the depth-3 GIL crossover moved; docs/PHASE3.md §6.1 says n = 6"
);

/// Parse the `layout=` keyword into the core's [`Layout`].
///
/// No default and no inference (`docs/API.md` R4): a wrong transpose or `wxyz`/`xyzw` order
/// yields a valid-looking transform pointing the wrong way.
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

/// Parse the `policy` argument of [`PyPlan::at_extrapolating`] into [`ExtrapPolicy`].
///
/// A string, like `layout=` and `interp=`; no default, because the policies differ in what the
/// answer *is*.
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
/// `QuatTwist` is refused, as in the C ABI: there is no extrapolating `at_with_derivatives`,
/// so one row would mix two policies.
fn write_pose_unchecked(pose: &tf_tree::Iso3, layout: Layout, dst: &mut [f64]) {
    match layout {
        Layout::Quat => tf_tree::write_quat(pose, dst),
        // `write_extrapolated` has already refused everything else; a `PyResult` cannot be built under `detach`.
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
    /// The engine, behind the `Arc` [`tf_tree::Tree::claim_owned`] requires (`docs/API.md` §2.2).
    /// The `Arc` keeps the *arena* alive; [`PyPlan`]'s `Py<PyTree>` keeps the Python object.
    pub(crate) inner: Arc<Tree>,
    /// Where this tree came from, when it came from a recording: `None` unless produced by
    /// [`crate::ingest::ingest_bag`] (the zero `source_digest` of `Tree::freeze_to`, [`0046`]).
    /// Immutable; what changes is [`Self::source_live`].
    pub(crate) source: Option<SourceInfo>,
    /// Whether [`Self::source`] still describes this tree's contents. Cleared the first time the
    /// tree can be written to (`publisher()`): a wrong digest is worse than an absent one
    /// (`docs/PHASE5.md` §2.3). `AtomicBool` because the pyclass is `frozen` and 3.14t runs it
    /// concurrently.
    pub(crate) source_live: std::sync::atomic::AtomicBool,
}

/// The recording a [`PyTree`] was ingested from — [`PyTree::source`]'s contents. A plain struct
/// rendered to a `dict` at the boundary, so a new field is additive.
pub(crate) struct SourceInfo {
    /// The recording's path as given to `ingest_bag`.
    pub(crate) path: String,
    /// BLAKE3 of the recording's **bytes** — the file, not the transforms — so
    /// it answers "was this index built from *that* file" without a re-ingest.
    pub(crate) digest: [u8; 32],
    /// `Survey::transforms_read`.
    pub(crate) transforms: u64,
    /// How many declared edges the recording carried no sample for.
    pub(crate) edges_without_samples: usize,
    /// The interval the **recording** covers; the queryable window is at most this and usually
    /// narrower (`Tree.span()` is the one to plan against).
    pub(crate) recording_ns: Option<(i64, i64)>,
}

impl PyTree {
    /// Wrap an engine with no recording behind it.
    pub(crate) fn wrap(inner: Arc<Tree>) -> Self {
        Self {
            inner,
            source: None,
            source_live: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Wrap an engine that was just read out of a recording.
    pub(crate) fn from_recording(inner: Arc<Tree>, source: SourceInfo) -> Self {
        Self {
            inner,
            source: Some(source),
            source_live: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// [`Self::source`], if it still describes this tree. `Acquire` against `publisher()`'s
    /// `Release`; a freeze racing a concurrent `publisher()` is the caller's race.
    pub(crate) fn provenance(&self) -> Option<&SourceInfo> {
        if self.source_live.load(std::sync::atomic::Ordering::Acquire) {
            self.source.as_ref()
        } else {
            None
        }
    }
}

/// Whether this plan samples anything at evaluation time.
///
/// Reproduces the predicate `Plan::check_domain_tag` runs on (`docs/decisions/0038` §4) from
/// `steps()`: an all-static path has no domain of its own, so refusing a domain over one would
/// be a new refusal.
fn samples_anything(plan: &tf_tree::Plan) -> bool {
    plan.steps()
        .iter()
        .any(|s| matches!(s, tf_tree::Step::Dyn { .. }))
}

/// Extract an integer-nanosecond stamp, refusing floats with the measurement that justifies it (§3).
///
/// Numpy float scalars are recognised only after the integer conversion fails, so an accepted
/// stamp pays nothing. Scalars only: an array keeps numpy's own `TypeError`
/// (`test_the_layout_path_reports_a_bad_stamps_array_exactly_as_at_does`).
fn stamp_from_any(obj: &Bound<'_, PyAny>) -> PyResult<i64> {
    if obj.is_instance_of::<pyo3::types::PyFloat>() {
        return Err(float_stamp_err());
    }
    obj.extract::<i64>().map_err(|e| {
        if is_numpy_floating_scalar(obj) {
            float_stamp_err()
        } else {
            e
        }
    })
}

/// [`stamp_from_any`]'s refusal, and its only spelling.
fn float_stamp_err() -> PyErr {
    PyTypeError::new_err(
        "stamps are integer nanoseconds, not float seconds. At a 2026 epoch \
         the ULP of float64 seconds is 238 ns, so every interval in a 1 kHz \
         stream is wrong after a round trip. Use tf_tree.from_sec(x) if you \
         genuinely have float seconds and accept the loss.",
    )
}

/// `isinstance(obj, numpy.floating)`, and `false` if that cannot be asked: this only chooses
/// the message of a refusal already happening.
fn is_numpy_floating_scalar(obj: &Bound<'_, PyAny>) -> bool {
    obj.py()
        .import("numpy")
        .and_then(|np| np.getattr("floating"))
        .and_then(|floating| obj.is_instance(&floating))
        .unwrap_or(false)
}

#[pymethods]
impl PyTree {
    /// Compile a plan from `source` to `target`.
    ///
    /// Compile once and reuse: the path walk and per-edge metadata lookup happen here.
    ///
    /// # `domain=`
    ///
    /// The `u8` time-domain tag the *queries* on this plan will be in
    /// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`); `tf_tree.SYSTEM_DOMAIN` and
    /// its siblings name the built-ins, user domains start at `4` (`docs/API.md` §2.5). The
    /// default stays `0`, not the plan's own domain, so a mistaken caller on a sim or sensor
    /// arena fails loudly. Unrelated to [`open_arena`]'s `domain=`, the `u32` rendezvous
    /// namespace.
    ///
    /// # Checked here, not per query
    ///
    /// A mismatch is refused at plan time with both frame names in hand; the core still
    /// re-checks every call (`0038` §4).
    #[pyo3(signature = (target, source, /, *, domain = 0))]
    fn plan(slf: &Bound<'_, PyTree>, target: &str, source: &str, domain: u8) -> PyResult<PyPlan> {
        let py = slf.py();
        let this = slf.get();
        // `resolve_frame`, not the interning `Tree::frame`: compiling a plan is a read.
        let t = resolve_frame(py, &this.inner, target)?;
        let s = resolve_frame(py, &this.inner, source)?;
        let plan = this
            .inner
            .plan(t, s)
            .map_err(|e| lookup_err(py, &this.inner, domain, e))?;
        // Guarded on [`samples_anything`] so this fires exactly where the core's check does.
        if samples_anything(&plan) && plan.domain() != domain {
            return Err(plan_domain_err(py, target, source, plan.domain(), domain));
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
    /// Argument order is **(child, parent)**, matching `Tree::claim`.
    ///
    /// Use it as a context manager
    #[pyo3(signature = (child, parent, /))]
    fn publisher(slf: &Bound<'_, PyTree>, child: &str, parent: &str) -> PyResult<PyPublisher> {
        let py = slf.py();
        let this = slf.get();
        // The tree stops being the recording once it can be written to. `Release` pairs with `provenance()`'s `Acquire`.
        this.source_live
            .store(false, std::sync::atomic::Ordering::Release);
        // Read-only resolution: topology is builder-time (`0004`), so interning a name yields no edge.
        let c = resolve_frame(py, &this.inner, child)?;
        let p = resolve_frame(py, &this.inner, parent)?;
        // `claim_owned`: the writer owns its `Arc<Tree>` (`docs/decisions/0017` step 6).
        let writer = this
            .inner
            .claim_owned(c, p)
            .map_err(|e| claim_err(py, &this.inner, parent, child, e))?;

        Ok(PyPublisher {
            edge: edge_label_of(parent, child),
            inner: Mutex::new(Some(writer)),
            tree: Arc::downgrade(&this.inner),
        })
    }

    /// Write this tree to `path` as a frozen `.tft` (`docs/PHASE5.md` §2.3).
    ///
    /// The replacement is atomic (sibling temporary, then rename). `source` labels the
    /// recording these poses came from (`null` in the manifest when absent). `path` is any
    /// `os.PathLike`, and the GIL is released for the copy.
    ///
    /// The container's `source_digest` comes from `Tree.source` when this tree was ingested from
    /// a recording, else all-zero. An explicit `source=` overrides the *label* only.
    #[pyo3(signature = (path, /, *, source = None))]
    fn freeze(&self, py: Python<'_>, path: PathBuf, source: Option<&str>) -> PyResult<()> {
        let prov = self.provenance();
        let digest = prov.map_or([0u8; 32], |s| s.digest);
        let label = source.or_else(|| prov.map(|s| s.path.as_str()));
        crate::offline::freeze_impl(py, &self.inner, &path, label, digest)
    }

    /// The recording this tree was ingested from, or `None`.
    ///
    /// `None` for a tree built in Python or opened with `open_file`, and again once
    /// `publisher()` has been called (the digest would assert something false).
    ///
    /// Keys: `path`, `digest` (hex), `transforms`, `edges_without_samples`,
    /// `recording_start_ns`, `recording_end_ns`. The `recording_*` bounds are the recording's,
    /// not what the rings retain; use `Tree.span(target, source)` to plan queries.
    #[getter]
    fn source<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, pyo3::types::PyDict>>> {
        let Some(src) = self.provenance() else {
            return Ok(None);
        };
        let d = pyo3::types::PyDict::new(py);
        d.set_item("path", &src.path)?;
        d.set_item("digest", hex32(src.digest))?;
        d.set_item("transforms", src.transforms)?;
        d.set_item("edges_without_samples", src.edges_without_samples)?;
        match src.recording_ns {
            // `None`: no dated transform, which is not a zero-length interval.
            Some((lo, hi)) => {
                d.set_item("recording_start_ns", lo)?;
                d.set_item("recording_end_ns", hi)?;
            }
            None => {
                d.set_item("recording_start_ns", py.None())?;
                d.set_item("recording_end_ns", py.None())?;
            }
        }
        Ok(Some(d))
    }

    /// The interval over which `tree.plan(target, source)` is answerable.
    ///
    /// `(t0, t1)` in nanoseconds, or `None` when every step is static (any stamp). Otherwise
    /// the **intersection** of the dynamic edges' retained windows; an empty intersection is
    /// *returned* as an empty interval, not raised. It is an outer bound, not a coverage
    /// guarantee: a publisher that died mid-window moves neither end, so a span can be
    /// almost entirely gap.
    #[pyo3(signature = (target, source, /))]
    fn span(&self, py: Python<'_>, target: &str, source: &str) -> PyResult<Option<(i64, i64)>> {
        crate::offline::span_impl(py, &self.inner, target, source)
    }

    /// The frame names on this tree, in declaration order (§4.4).
    ///
    /// A snapshot on a live arena: a frame interned later is absent, and a slot caught
    /// mid-intern is skipped. A tree inherited across a `fork()` raises rather than
    /// answering `[]`.
    fn frames(&self) -> PyResult<Vec<String>> {
        crate::offline::frames_impl(&self.inner)
    }

    /// The edges on this tree as `(parent, child)` name pairs (§4.4).
    ///
    /// Names only: rate, jitter, gaps and counts are `docs/PHASE5.md` §4.2's `ds.edges()`,
    /// held back until §3's counting pass exists. `(parent, child)` is `tf_tree.build`'s order,
    /// but that rebuilds the *graph* only: the kind is not reported, so a static edge comes back
    /// dynamic and empty. A snapshot; `plan()` resolves against the live topology.
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

    /// Has the process that owns this arena gone away (`docs/PHASE2.md` §3.5)?
    ///
    /// `False` for anything that is not a joined shared attachment. Answers "the arena has no
    /// owner", not "my socket is dead" (`0043`, `0057`). **Nothing calls it for you**: no
    /// background thread (`0019`), so an arena whose survivors never ask stays ownerless.
    ///
    /// ```python
    /// if tree.owner_lost():
    ///     tree.inherit_ownership()
    /// ```
    #[cfg(target_os = "linux")]
    fn owner_lost(&self) -> bool {
        self.inner.owner_lost()
    }

    /// Inherit the owner role from a departed owner and begin serving (§3.5;
    /// `docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md`).
    ///
    /// Returns the outcome's name: `"Inherited"`, `"OwnerAlive"`, `"Contended"`, `"ReadOnly"`,
    /// `"NotApplicable"`. Anything but `"Inherited"` means this process is not the owner, and
    /// lookups are unaffected either way. `"OwnerAlive"` and `"Contended"` are not final while
    /// `owner_lost()` stays `True`: call again on the next pass. `"ReadOnly"` means a read-only
    /// consumer cannot rescue itself (D18); open with `mode="rw"` to inherit.
    ///
    /// # Errors
    ///
    /// `TfTreeError` if the `fcntl` fails or the rendezvous socket cannot be bound; the process
    /// then remains a plain participant.
    #[cfg(target_os = "linux")]
    fn inherit_ownership(&self) -> PyResult<&'static str> {
        match self.inner.inherit_ownership() {
            Ok(o) => Ok(match o {
                tf_tree::Inheritance::Inherited => "Inherited",
                tf_tree::Inheritance::OwnerAlive => "OwnerAlive",
                tf_tree::Inheritance::Contended => "Contended",
                tf_tree::Inheritance::ReadOnly => "ReadOnly",
                // `#[non_exhaustive]`: "NotApplicable" (behave as a plain participant) is the safe default.
                _ => "NotApplicable",
            }),
            Err(e) => Err(crate::errors::inherit_err(&e)),
        }
    }

    /// Collect what dead participants left behind; returns how many records were freed.
    ///
    /// Both sweeps, summed: claim leases no live process holds, and participant records whose
    /// lock bytes the kernel has released. Usually there is nothing to do: the owner's hangup
    /// callback already revokes a dead participant's claims. This is the only collector for a
    /// dead **owner** (and for participants that die after it), which is ordinary.
    ///
    /// **Dangerous** in a process tree where a Rust component served an arena built with
    /// `TreeBuilder::build_shared` and published by hand (out of contract, `0031`): such a
    /// participant holds no lock byte and is indistinguishable from a dead one, so this frees
    /// the records of *running* processes. `open_arena` always joins through the rendezvous, so
    /// Python alone is safe. See `docs/RUNBOOK.md`, *ParticipantTableFull*.
    ///
    /// `0` for a read-only tree, an in-process tree, or one with no rendezvous.
    #[cfg(target_os = "linux")]
    fn reap_dead(&self) -> usize {
        self.inner.reap_dead() + self.inner.reap_participants()
    }

    // The non-Linux arms are present, not absent (see `open_arena`), and do not raise: off
    // Linux a tree is in-process, so there is no owner to lose and nothing left behind.

    /// Always `False` off Linux: there is no owner that can go away.
    #[cfg(not(target_os = "linux"))]
    fn owner_lost(&self) -> bool {
        false
    }

    /// Always `"NotApplicable"` off Linux: there is no owner role to inherit.
    #[cfg(not(target_os = "linux"))]
    fn inherit_ownership(&self) -> PyResult<&'static str> {
        Ok("NotApplicable")
    }

    /// Always `0` off Linux: no other process can have left anything behind.
    #[cfg(not(target_os = "linux"))]
    fn reap_dead(&self) -> usize {
        0
    }

    /// Which arena instance this tree is attached to, as 32 hex characters.
    ///
    /// All-zero for an in-process tree. Two processes that resolved the same name can hold
    /// *different* segments if the owner was replaced between their `open()` calls; this tells
    /// them apart.
    ///
    /// # Errors
    ///
    /// `ChildProcessDetachedError` on a tree inherited across a `fork()`: the poison arena's
    /// all-zero identity would read as "in-process" and hide a split brain.
    fn instance_uuid(&self) -> PyResult<String> {
        if self.inner.detached() {
            return Err(crate::errors::detached_err());
        }
        Ok(self.uuid_hex())
    }

    fn __repr__(&self) -> String {
        // Describes a detached tree instead of refusing: a raising `__repr__` breaks `print` and debuggers.
        let instance = if self.inner.detached() {
            " detached-by-fork".to_string()
        } else {
            let uuid = self.uuid_hex();
            // Show the instance only when there is one.
            if uuid.chars().all(|c| c == '0') {
                String::new()
            } else {
                format!(" instance={}", &uuid[..8])
            }
        };
        // `True`/`False`, not Rust's lowercase.
        format!(
            "<tf_tree.Tree shared={} writable={}{instance}>",
            py_bool(self.inner.is_shared()),
            py_bool(self.inner.is_writable())
        )
    }

    /// One transform, without compiling a plan first (§4.2).
    ///
    /// The plan is cached per **thread**, keyed on `(arena, target, source, topology
    /// generation)`; a shared cache would be a contention point under free-threading (§7.2).
    /// Prefer `tree.plan(...)` in a loop: this pays a cache probe per call.
    ///
    /// # `domain=`
    ///
    /// As [`PyTree::plan`]'s (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`). The
    /// check cannot move to plan time here (the plan is cached, not returned), so it stays
    /// per call and the refusal names two tags rather than a route.
    #[pyo3(signature = (target, source, stamp_ns, /, *, domain = 0))]
    fn lookup<'py>(
        &self,
        py: Python<'py>,
        target: &str,
        source: &str,
        stamp_ns: &Bound<'py, PyAny>,
        domain: u8,
    ) -> PyResult<Bound<'py, PyArray2<f64>>> {
        // `&PyAny`, not `i64`, so a `float` gets §3's refusal rather than PyO3's conversion error.
        let stamp_ns = stamp_from_any(stamp_ns)?;
        let iso = self
            .inner
            .lookup_tagged(target, source, stamp_ns, domain)
            // `UnknownFrame` carries only a BLAKE3 prefix, so name the missing frame here, where
            // both names are in scope. [`unknown_frame_err`] is a pure read (`Tree::frame` interns).
            .map_err(|e| match e {
                tf_tree::LookupError::UnknownFrame { .. } => {
                    unknown_frame_err(py, &self.inner, [target, source], e)
                }
                other => lookup_err(py, &self.inner, domain, other),
            })?;
        let out = PyArray2::<f64>::zeros(py, [4, 4], false);
        // SAFETY: freshly allocated here; no other reference exists.
        let slice = unsafe { out.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(out)
    }
}

impl PyTree {
    /// The instance uuid as 32 lowercase hex characters. Outside `#[pymethods]` so it is not a
    /// Python method; **callers check `detached()` first**.
    fn uuid_hex(&self) -> String {
        self.inner
            .instance_uuid()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// What [`PyPlan::adaptive`] returns: `(K,)` stamps and `(K, 4, 4)` poses.
type Knots<'py> = (Bound<'py, PyArray1<i64>>, Bound<'py, PyArray3<f64>>);

/// A compiled lookup path.
///
/// `frozen` and `Sync`: a `Plan` is `Copy` with no interior mutability, so free-threaded
/// threads may evaluate one concurrently.
///
/// The `Box` dates from `Plan` being over-aligned (`align(64)`, against CPython's 16-byte
/// guarantee); `Plan` is now `align(8)` (`0042`), so it is kept only as an unmeasured
/// allocation-per-plan cost. Fields added here must respect pyclass alignment.
#[pyclass(name = "Plan", module = "tf_tree", frozen)]
pub struct PyPlan {
    plan: Box<tf_tree::Plan>,
    /// The time-domain tag every query through this handle carries
    /// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`). Validated at
    /// [`PyTree::plan`], so the core's per-query check is a predictable no-op. Costs the pyclass
    /// eight bytes (`align(8)` rounds 17 up to 24), affordable because it is per plan.
    domain: u8,
    /// A **reference-counted handle** to the tree this plan reads through, so the arena cannot
    /// outlive its readers (a raw pointer here was a use-after-free after `del tree`).
    tree: Py<PyTree>,
}

impl PyPlan {
    /// The tree this plan reads through. `get`, not `borrow`: [`PyTree`] is `frozen`, so no
    /// borrow check or GIL token is needed.
    fn tree(&self) -> &Tree {
        &self.tree.get().inner
    }
}

#[pymethods]
impl PyPlan {
    /// Evaluate at one stamp, or at an array of stamps.
    ///
    /// Scalar in, `(4, 4)` out; `(N,)` in, `(N, 4, 4)` out. `stamps` is positional-only
    /// (`METH_FASTCALL`, §4.2).
    ///
    /// # `layout=`
    ///
    /// Keyword-only, explicit, no default that could be silently wrong (`docs/API.md` R4):
    ///
    /// | `layout=` | scalar | batch | dtype |
    /// | --- | --- | --- | --- |
    /// | `"mat4"` (default) | `(4, 4)` | `(N, 4, 4)` | `float64` |
    /// | `"quat"` | `(7,)` | `(N, 7)` | `float64` |
    /// | `"affine32"` | `(12,)` | `(N, 12)` | **`float32`** |
    /// | `"quat_twist"` | `(13,)` | `(N, 13)` | `float64` |
    ///
    /// `"quat_twist"` is `at_with_derivatives` as a layout (`docs/PHASE5.md` §4.4 item 1):
    /// `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]`, the body twist in the plan's **source**
    /// frame, angular first. A `LerpSlerp` edge has no exact body twist and raises
    /// `DerivativesUnavailableError`.
    ///
    /// The keyword's cost to a caller who omits it is unmeasured (§4.2's A/B was inconclusive on
    /// a noisy host). The methods stay vectorcall, pinned by
    /// `tests/python/test_api.py::test_the_hot_methods_are_emitted_as_meth_fastcall`.
    #[pyo3(signature = (stamps, /, *, layout = None))]
    fn at<'py>(
        &self,
        py: Python<'py>,
        stamps: &Bound<'py, PyAny>,
        layout: Option<&str>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // A non-default layout leaves the hot path before the dispatch below.
        if let Some(name) = layout {
            let layout = layout_from_str(name)?;
            if layout != Layout::Mat4 {
                return self.at_layout(py, stamps, layout);
            }
        }
        // Scalar first: a failed `cast::<PyArray1<i64>>` on an `int` builds and discards a
        // `DowncastError` (~150 ns of a ~313 ns call), and a control loop is all scalar ticks.
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
            .map_err(|e| lookup_err(py, self.tree(), self.domain, e))?;
        let out = PyArray2::<f64>::zeros(py, [4, 4], false);
        // SAFETY: freshly allocated by us, so nothing else holds a reference and
        // the slice is exactly 16 contiguous f64.
        let slice = unsafe { out.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(out.into_any())
    }

    /// Evaluate a batch into a caller-provided `(N, 4, 4)` float64 array.
    ///
    /// The tier that allocates nothing (§5.2). The array is validated **completely before any
    /// element is written**.
    ///
    /// `layout=` is [`at`](Self::at)'s, and `out`'s shape and dtype follow it: `(N,
    /// layout_elems)`, or `(layout_elems,)` for a scalar stamp; `float32` for `"affine32"`,
    /// `float64` otherwise (R2: every batch entry point has an `_into` form).
    ///
    /// Stamp dispatch is `at`'s: probe `int`, fall through the `(N,) int64` array cast, and let
    /// [`stamp_from_any`] have the last word (`docs/PHASE3.md` §3: an `np.int64` scalar is
    /// accepted, a `float` meets the ULP `TypeError`). So a `list` or a mis-typed stamps array
    /// raises numpy's or PyO3's own `TypeError`, not a `BufferError`.
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
        // `reject_device_memory` is skipped on the numpy path: it costs ~120 ns per call, and a
        // successful `cast` proves host memory (CuPy and torch are not numpy subclasses).
        //
        // Scalar form: a control loop does one lookup per tick, so `at`'s allocation dominates
        // (`np.empty((4, 4))` ~177 ns of `at`'s ~330; `at_into` ~146 ns).
        // Dispatch on `stamps`, then validate `out` against it, so the error blames the right
        // argument. An `int` leads (a pointer compare). The array cast is fallen through, not
        // `else`-d: `docs/PHASE3.md` §3 accepts an `np.int64` scalar and gives a `float` the ULP
        // message, with `stamp_from_any` having the last word.
        if !stamps.is_instance_of::<pyo3::types::PyInt>() {
            if let Ok(stamps) = stamps.cast::<PyArray1<i64>>() {
                let arr = match out.cast::<PyArray3<f64>>() {
                    Ok(a) => a,
                    Err(_) => {
                        // Not a numpy array, so it may be device memory: refuse rather than
                        // fault (§5.5). Only `numpy.ndarray` (subclasses included) is accepted.
                        reject_device_memory(out)?;
                        return Err(BufferError::new_err(
                            "out must be a writable, C-contiguous (N, 4, 4) float64 numpy array \
                             — or (4, 4) for a scalar stamp. Other buffer-protocol objects are \
                             not accepted yet; np.asarray(...) it first",
                        ));
                    }
                };
                let n = stamps.len();
                return self.fill(py, stamps, arr, n);
            }
        }

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
        // Writability, before anything is evaluated: `as_slice_mut` does not check it, and a
        // read-only `np.memmap` would `SIGSEGV` (§5.5: refuse rather than fault).
        // `try_readwrite` was rejected: its borrow registry cost +50 ns on a 173 ns call.
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
            .map_err(|e| lookup_err(py, self.tree(), self.domain, e))?;
        // SAFETY: checked C-contiguous, (4, 4) and writable above, so the slice is exactly 16
        // writable f64. Aliasing remains the caller's to avoid.
        let slice = unsafe { arr.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(())
    }

    /// Evaluate past the newest sample under an explicit policy, and get back how far past it
    /// that was (`docs/decisions/0039`).
    ///
    /// Returns `(poses, by_ns)`; there is no spelling returning the pose alone, so the distance
    /// travels with it. `policy` is required: `"error"` (refuse, as `at` does, with a distance
    /// on success), `"hold"` (the newest pose) or `"constant_twist"` (extend the screw the two
    /// newest samples imply). `at` still refuses; this is a second entry point.
    ///
    /// | `stamps` | `poses` | `by_ns` |
    /// | --- | --- | --- |
    /// | `int` | `(4, 4)` float64 | `int` |
    /// | `(N,)` int64 | `(N, 4, 4)` float64 | `(N,)` **int64 array** |
    ///
    /// `by_ns` is per stamp (`max(0, stamp - newest_common)`): a scalar for a batch would mark
    /// fresh elements stale or stale ones fresh.
    ///
    /// The Rust `Extrapolated`'s edge is not carried (`Plan.edges()` and `tf_tree doctor` give
    /// the breakdown). `layout=` accepts `mat4` and `quat` only.
    ///
    /// Cost: a loop over the scalar form under one `Guard` (`0039` §4), so an `O(log n)`
    /// bracket search per stamp per step.
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
        // `at`'s dispatch (`PyInt` first; the array cast is fallen through so a `float` meets §3's message).
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
            .map_err(|e| lookup_err(py, self.tree(), self.domain, e))?;
        // `mat4` keeps `at`'s `(4, 4)` shape; other layouts are the flat `(elems,)` row.
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

    /// [`PyPlan::at_extrapolating`] writing into caller memory (`docs/API.md` R2, NORMATIVE).
    ///
    /// `poses` takes the shape `at_extrapolating` would return (`(4, 4)` or `(elems,)` for a
    /// scalar stamp, `(N, 4, 4)` or `(N, elems)` for an array); `by_ns` is `()`-shaped or
    /// `(N,)` `int64`.
    ///
    /// A `LookupError` on element *k* leaves `0..k` written and the rest as they were, as in
    /// `at_into`; use the allocating form for all-or-nothing.
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
        // Refuse the layout before touching either buffer: the fold cannot refuse per element.
        write_extrapolated(&tf_tree::Iso3::IDENTITY, layout, &mut [0.0; 16])?;
        let e = layout.elems();

        // `at`'s stamp dispatch; see `at_extrapolating`.
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
        let src: &[i64] = match &src_arr {
            // SAFETY: `as_slice` refuses a non-contiguous or misaligned array; its precondition is
            // that no alias writes the array while `src` lives. `by_ns` is refused below if it
            // overlaps; every other writer (the caller's views, another thread) is the caller's
            // to rule out: raw `as_slice` registers no numpy borrow.
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

        // Refuse a `by_ns` that aliases `stamps` before either mutable slice exists: this is the
        // only `_into` whose input and an output are both `int64`, so `by_ns=stamps` would give
        // `&[i64]` and `&mut [i64]` over one allocation (UB from safe Python). Comparing byte
        // ranges also catches a *view* of the same memory.
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

        // SAFETY: `check_out` proved both C-contiguous, shaped and writable, and the range check
        // rules out `by_ns` aliasing `stamps`.
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
        res.map_err(|err| lookup_err(py, self.tree(), self.domain, err))
    }

    /// The most recent transform on this path.
    fn latest<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyArray2<f64>>> {
        let g = self.tree().guard();
        let iso = self
            .plan
            .latest(&g)
            .map_err(|e| lookup_err(py, self.tree(), self.domain, e))?;
        let out = PyArray2::<f64>::zeros(py, [4, 4], false);
        // SAFETY: freshly allocated here; no other reference exists.
        let slice = unsafe { out.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(out)
    }

    /// The minimum set of knots whose linear interpolation stays within `tol`.
    ///
    /// Returns `(stamps, poses)`: `(K,)` int64 and `(K, 4, 4)` float64, strictly increasing in
    /// stamp; the consumer LERPs between knots, with error bounded by construction. A 100 ms
    /// sweep at 1 cm / 1e-4 rad is tens of knots (§5.6); a large result means the tolerance is
    /// wrong. `lin` is metres and `ang` radians, defaulting to `docs/PHASE3.md` §4.2's values.
    #[pyo3(signature = (start_ns, end_ns, /, *, lin = 1e-3, ang = 1e-4))]
    fn adaptive<'py>(
        &self,
        py: Python<'py>,
        start_ns: &Bound<'py, PyAny>,
        end_ns: &Bound<'py, PyAny>,
        lin: f64,
        ang: f64,
    ) -> PyResult<Knots<'py>> {
        // Both stamps before the tolerances, so a float stamp meets §3's message first.
        let start_ns = stamp_from_any(start_ns)?;
        let end_ns = stamp_from_any(end_ns)?;
        if !(lin.is_finite() && ang.is_finite()) || lin <= 0.0 || ang <= 0.0 {
            return Err(PyValueError::new_err(
                "lin and ang must be finite and positive",
            ));
        }
        // `SystemDomain` is storage here, not the query: `D` only types `scratch` and the stamp slice (`0038`); `self.domain` is what is checked.
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
            .map_err(|e| lookup_err(py, self.tree(), self.domain, e))?;

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

    /// The **dynamic** edges this plan samples, as `(parent, child)` pairs (§4.4).
    ///
    /// Shorter than [`depth`](Self::depth) across a static edge: folded edges lose their
    /// identities at compile time.
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
        // Every check before a single store (§5.3). Non-contiguous is rejected, not silently copied.
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

        // Writability before a single store: a read-only `np.memmap` would `SIGSEGV` (see `at_into`).
        if !is_writeable(out.as_untyped()) {
            return Err(BufferError::new_err(
                "out is not writable (NumPy reports NPY_ARRAY_WRITEABLE clear); \
                 a read-only mapping cannot receive a transform",
            ));
        }

        // SAFETY: no other alias may write `stamps` or touch `out` while the slices live, across
        // the `detach` included (a dtype view can share memory). That is the caller's to uphold;
        // raw `as_slice*` registers no numpy borrow. `out` was checked writable above.
        let (src, dst) = unsafe { (stamps.as_slice()?, out.as_slice_mut()?) };

        let plan = *self.plan;
        let tree = self.tree();
        // Read out here: the `detach` body must touch no Python object (§6.2).
        let domain = self.domain;

        let mut run = || {
            let g = tree.guard();
            // Raw nanoseconds, so no `Vec<Stamp>` is allocated (`0038` §1).
            plan.at_many_into_tagged(&g, src, domain, Layout::Mat4, dst)
        };
        let res = if release_the_gil(n, self.plan.len()) {
            // Touch no Python object inside (§6.2).
            py.detach(run)
        } else {
            run()
        };
        res.map_err(|e| lookup_err(py, tree, domain, e))
    }

    /// [`PyPlan::at_extrapolating`]'s array half: `(N, 4, 4)` poses and `(N,)` distances,
    /// allocated here (a failure drops them, so no partial write).
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
        // Refuse the layout once, before allocating: `write_pose_unchecked` cannot refuse under `detach`.
        write_extrapolated(&tf_tree::Iso3::IDENTITY, layout, &mut [0.0; 16])?;

        let n = stamps.len();
        let e = layout.elems();
        // `(N, 4, 4)` for `mat4`, `(N, elems)` otherwise, as `at` returns.
        let poses = if layout == Layout::Mat4 {
            PyArray3::<f64>::zeros(py, [n, 4, 4], false).into_any()
        } else {
            PyArray2::<f64>::zeros(py, [n, e], false).into_any()
        };
        let dist = PyArray1::<i64>::zeros(py, [n], false);
        {
            let flat = poses.cast::<numpy::PyArrayDyn<f64>>()?;
            // SAFETY: `poses` and `dist` are freshly allocated. No other alias may write `stamps`
            // while `src` lives, across the `detach` included: the caller's to uphold.
            let (src, pd, dd) = unsafe {
                (
                    stamps.as_slice()?,
                    flat.as_slice_mut()?,
                    dist.as_slice_mut()?,
                )
            };
            let plan = *self.plan;
            let tree = self.tree();
            // Read out here: the `detach` body must touch no Python object (§6.2).
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
            // `at`'s threshold; it under-counts this path, which errs towards releasing the GIL.
            let res = if release_the_gil(n, self.plan.len()) {
                py.detach(run)
            } else {
                run()
            };
            res.map_err(|e| lookup_err(py, tree, domain, e))?;
        }
        Ok((poses.into_any(), dist.into_any()))
    }

    /// [`PyPlan::at`]'s `layout=` path: allocate the right shape and fill it. Off the `mat4`
    /// hot path, so written for clarity.
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
            // A one-element batch, not a scalar kernel: `docs/PHASE3.md` §11.1 requires `at(t)` ==
            // `at([t])[0]` bit-exactly. Reached by falling through the array cast, so a `float`
            // meets `stamp_from_any`'s message (§3), as in `at`.
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
        // SAFETY: no other alias may write `stamps` while `src` lives, across the `detach` in `eval_*`: the caller's to uphold.
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
    /// Everything is checked before a single element is written (§5.3). The stamp dispatch is
    /// [`Self::at_layout`]'s, as `docs/PHASE3.md` §3 (NORMATIVE) requires: the array cast is
    /// fallen through, not `else`-d, and [`stamp_from_any`] has the last word.
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
        let src: &[i64] = match &src_arr {
            // SAFETY: as in `at_extrapolating_into`; no other alias may write the array across `eval_*`'s `detach`.
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
        res.map_err(|e| lookup_err(py, tree, domain, e))
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
        res.map_err(|e| lookup_err(py, tree, domain, e))
    }
}

/// A claimed edge, and the only way to publish from Python.
///
/// `tf_tree::Publisher` is `Send + !Sync` by design (single writer per edge), so it sits in a
/// mutex (`docs/PHASE3.md` §7.1): two Python threads pushing to one edge serialize, at ~15 ns
/// uncontended. It carries its own `Arc<Tree>` via [`OwnedWriter`], so the arena outlives it.
#[pyclass(name = "Publisher", module = "tf_tree")]
pub struct PyPublisher {
    /// How a failed `push` names this edge: `edge "map" -> "base"`.
    ///
    /// The caller's own two strings, captured at claim time, so a failed `push` names the edge
    /// greppably (the stored names truncate at 48 bytes). Allocated once per claim; `push`
    /// reads it only on the error path.
    edge: String,
    /// `None` after `__exit__` or `release()`, so a use-after-release is a clear Python error.
    ///
    /// An [`OwnedWriter`], not a hand-rolled `EdgeWriter<'static>`: the workspace has exactly one
    /// lifetime extension (`docs/decisions/0017`), and it reproduces every guard by containing
    /// the `EdgeWriter` whole. Do not restate `0017`'s hazards here (its step 6 verifies by grep).
    /// `push` is [`OwnedWriter::push`](tf_tree::OwnedWriter::push), which forwards
    /// fork-checked `EdgeWriter::push`.
    inner: Mutex<Option<OwnedWriter>>,
    /// The tree this claim was made on, for the one refusal no `push` reaches: an **empty**
    /// `push_many` must still raise `ChildProcessDetachedError` in a fork child
    /// (`docs/PHASE3.md` §8.1, NORMATIVE). `Weak`, so it does not keep the tree alive past
    /// `release()`.
    tree: std::sync::Weak<Tree>,
}

#[pymethods]
impl PyPublisher {
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Release the claim on scope exit (§4.3). `Drop` would too, but Python's finalization
    /// order is not guaranteed and a held edge blocks every other claimant.
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
    fn push(&self, py: Python<'_>, stamp_ns: &Bound<'_, PyAny>, quat7: Vec<f64>) -> PyResult<()> {
        // The stamp first (§3's refusal carries a measurement); see `Tree.lookup`.
        let stamp_ns = stamp_from_any(stamp_ns)?;
        let iso = iso_from_quat7(&quat7)?;
        let g = self.lock()?;
        let p = g.as_ref().ok_or_else(released)?;
        p.push(stamp_ns, &iso)
            .map_err(|e| push_err(py, self.tree.upgrade().as_deref(), &self.edge, e))
    }

    /// Publish a whole batch: `(N,)` stamps and `(N, 7)` poses.
    ///
    /// The loop is in Rust: the engine has no batched write (each publication is an independent
    /// release-store), but this avoids N FFI crossings (~30 ns each).
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
        // SAFETY: both arrays were just checked contiguous and are borrowed only for this call.
        let (st, po) = unsafe { (stamps.as_slice()?, poses.as_slice()?) };

        let g = self.lock()?;
        let p = g.as_ref().ok_or_else(released)?;
        // An empty batch never reaches `push`'s fork check, so refuse here (`docs/PHASE3.md` §8.1);
        // any other batch meets the refusal on sample 0.
        if st.is_empty() && self.tree.upgrade().is_some_and(|t| t.detached()) {
            return Err(detached_err());
        }
        let py = stamps.py();
        for (i, stamp) in st.iter().enumerate() {
            let iso = iso_from_quat7(&po[i * 7..(i + 1) * 7])?;
            p.push(*stamp, &iso).map_err(|e| {
                // Name the index: earlier samples *were* published. The message after the colon is a scalar `push`'s.
                push_class(py, self.tree.upgrade().as_deref(), e)(format!(
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
/// DLPack ABI: 1 = `kDLCPU`, 3 = `kDLCUDAHost` (pinned), 11 = `kDLROCMHost`, 13 =
/// `kDLCUDAManaged`. Anything else is device memory, which a CPU store leaves undefined.
const HOST_DEVICE_TYPES: [i32; 4] = [1, 3, 11, 13];

/// Refuse an `out` buffer that does not live where a CPU can write it.
///
/// Placement comes from DLPack's `__dlpack_device__` (cheap, no CUDA runtime, D8); mutability
/// and contiguity from the buffer protocol. Nearly every host buffer reports `kDLCPU`, so this
/// only turns a segfault into a good error.
fn reject_device_memory(obj: &Bound<'_, PyAny>) -> PyResult<()> {
    let Ok(f) = obj.getattr("__dlpack_device__") else {
        // No DLPack: the buffer protocol below validates the rest.
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
/// `PyArrayDyn` because the scalar overload wants `(elems,)` and the batch `(N, elems)`; the
/// downcast checks dtype (keeping `affine32` out of a `float64` buffer), [`check_out`] rank and
/// shape. A failed downcast tries [`reject_device_memory`] first.
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

/// The three checks every `out` buffer passes before a write (§5.3): contiguous, right shape,
/// writable, in a fixed order shared by both `layout=` overloads.
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
/// Whether NumPy marks this array writable. `as_slice_mut` checks neither this nor aliasing,
/// and a read-only `np.memmap` would `SIGSEGV` (§5.5: refuse rather than fault). One field read.
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

/// Parse `interp=` (`docs/PHASE3.md` §4.1). Exactly the two `InterpPolicy` variants; an unknown
/// name is refused, since the difference is invisible in the output.
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

/// [`interp_from_str`] backwards, kept adjacent as a pair (a missing spelling once let
/// `DerivativesUnavailableError` say `interpolation policy 1`). `InterpPolicy` is deliberately
/// exhaustive, so a third policy is a compile error here.
pub(crate) fn interp_name(policy: InterpPolicy) -> &'static str {
    match policy {
        InterpPolicy::ScLerp => "sclerp",
        InterpPolicy::LerpSlerp => "lerpslerp",
    }
}

/// Build an in-process tree from a simple edge list.
///
/// Topology is builder-time (decision `0004`): there is no `declare_*` on a live tree.
///
/// # `interp=`
///
/// Defaults to **`"sclerp"`**, `tf_tree::TreeBuilder`'s own default (`docs/PROJECT.md` §5 D5).
/// `"lerpslerp"` is tf2-compatible but not right-invariant, and has no exact body twist, so
/// `layout="quat_twist"` over it raises `DerivativesUnavailableError`.
///
/// # `frame_headroom=`
///
/// Spare **frame-name** slots (`TreeBuilder::frame_headroom`). The frame table never grows
/// (invariant 3), so with 0 a Rust or C peer, or the ROS ingest bridge, calling `Tree::frame()`
/// on the arena gets `CapacityExceeded` forever. There is deliberately no `edge_headroom`
/// (`docs/PHASE5.md` §5.8): nothing declares an edge at runtime.
///
/// # `edges` also takes a topology config
///
/// A list of pairs makes every edge **dynamic** under one capacity. Passing the *text* of a
/// topology config instead (the schema `ros/tf_tree_ros` starts from and `tf_tree topology
/// --discover` writes) also expresses static edges, per-edge sizes, rates and domains
/// ([`0041`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0041-python-declares-a-topology-the-way-everything-else-does.md)).
/// `capacity=` and `interp=` are refused beside a config, since the config carries both.
#[pyfunction]
#[pyo3(signature = (edges, *, capacity = None, interp = None, frame_headroom = 0))]
pub fn build(
    edges: &Bound<'_, PyAny>,
    capacity: Option<u32>,
    interp: Option<&str>,
    frame_headroom: u32,
) -> PyResult<PyTree> {
    // A `str` is never a valid edge list (`0041`). `String`, not `&str`, which `just py-cross-check`'s Apple and Windows targets refuse.
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
    Ok(PyTree::wrap(Arc::new(inner)))
}

/// Build from topology-config text — `0041`.
///
/// The error is rendered here because `ConfigError` borrows from `text`; formatting it
/// converts a borrowed diagnostic into an owned one.
fn config_builder(text: &str, frame_headroom: u32) -> PyResult<tf_tree::TreeBuilder> {
    let cfg = tf_tree_bridge::TopologyConfig::parse(text)
        .map_err(|e| PyValueError::new_err(format!("topology config: {e}")))?;

    // Ask the config before the builder, as `tf_tree_cli::topology` and `tf_tree_c::bridge` do:
    // the parser misses a multi-hop cycle, and `build()` would report it as an unresolvable
    // `FrameId` for an arena never constructed.
    if let Some(child) = cfg.cycle_child() {
        return Err(PyValueError::new_err(format!(
            "topology config: the declared topology has a cycle through frame \
             {child:?} — following its parent links returns to it"
        )));
    }

    let mut b = cfg.builder();
    // A non-zero argument overrides the config's own `frame_headroom`; zero leaves it.
    if frame_headroom != 0 {
        b = b.frame_headroom(frame_headroom);
    }
    Ok(b)
}

/// Build a heap tree from topology-config text — `0041`.
fn build_from_config(text: &str, frame_headroom: u32) -> PyResult<PyTree> {
    let inner = config_builder(text, frame_headroom)?
        .build()
        // `config_build_err`, not `build_err`, whose prose is about an *edge list*.
        .map_err(config_build_err)?;
    Ok(PyTree::wrap(Arc::new(inner)))
}

/// A `BuildError` from a config, phrased for somebody holding a text file.
///
/// Not `build_err`, whose prose is about the `edges=` list and `capacity=` keyword (it would say
/// "0 pairs" and blame `tf_tree` for a cycle the caller wrote). `TfTreeError`, so one
/// `except` catches a build failure from either construction form.
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
/// Takes `[qw qx qy qz tx ty tz]` rather than a 4x4: a nearly rigid matrix has no exact
/// quaternion, only a projection.
#[pyfunction]
#[pyo3(signature = (tree, child, parent, stamp_ns, quat7, /))]
pub fn push(
    py: Python<'_>,
    tree: &PyTree,
    child: &str,
    parent: &str,
    stamp_ns: &Bound<'_, PyAny>,
    quat7: Vec<f64>,
) -> PyResult<()> {
    // The stamp first, so a refused stamp costs no resolution or claim (see `Tree.lookup`).
    let stamp_ns = stamp_from_any(stamp_ns)?;
    if quat7.len() != 7 {
        return Err(PyValueError::new_err(
            "expected [qw, qx, qy, qz, tx, ty, tz]",
        ));
    }
    // Read-only resolution, as in `Tree.publisher`.
    let c = resolve_frame(py, &tree.inner, child)?;
    let p = resolve_frame(py, &tree.inner, parent)?;
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
        .map_err(|e| claim_err(py, &tree.inner, parent, child, e))?;
    publisher
        .push(stamp_ns, &iso)
        .map_err(|e| push_err(py, Some(&tree.inner), &edge_label_of(parent, child), e))
}

/// Attach to a running arena (`docs/PHASE3.md` §4.1).
///
/// `mode="ro"` and creation off, on purpose (D18): a `PROT_READ` mapping makes notebooks
/// incapable of corrupting a robot's tree, and a notebook started early must fail loudly
/// rather than create an empty arena the publisher then refuses to join.
///
/// `domain=` is the `u32` *rendezvous* domain (which arena to attach to, `$ROS_DOMAIN_ID`'s
/// analogue), **not** `Tree.plan`'s `u8` time-domain tag
/// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`).
///
/// # Creating
///
/// `create=[(parent, child), ...]` (or topology-config text, as [`build`]'s `edges`) creates the
/// arena when absent; `0004` sizes an arena from its declared edges. `capacity`, `interp` and
/// `frame_headroom` are [`build`]'s; `interp` is parsed even without `create`, so a typo fails
/// in both calls. `frame_headroom` matters most here: with 0, a Python-created arena refuses
/// every runtime frame name for its life. Creating requires `mode="rw"` and is refused otherwise.
// Linux-only (the shared-arena surface is `#[cfg(target_os = "linux")]` in the facade). The
// `#[cfg(not(...))]` arm below keeps the attribute present; see `offline.rs`.
#[cfg(target_os = "linux")]
#[pyfunction]
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = None, interp = None, frame_headroom = 0))]
// The eighth argument is PyO3's token, which `open_err` needs (`docs/decisions/0058` §5).
#[allow(clippy::too_many_arguments)]
pub fn open_arena(
    py: Python<'_>,
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
    // `interp` is validated even when nothing is created, so a typo is a startup error as in
    // `build`. `create=` takes `build`'s two forms (`0041`); owned, as there.
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
        // The same preflight and builder as `build`, so the diagnostics cannot diverge.
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
    // One mapper for both failures (`Open::name` also returns an `OpenError`). A config path must
    // not reach `open_err`'s build prose, which is about the `create=` edge list and `capacity=`.
    let created: &[(String, String)] = pairs.as_deref().unwrap_or(&[]);
    let map_err = |e: tf_tree::OpenError| match (&config, e) {
        (Some(_), tf_tree::OpenError::Build(inner)) => config_build_err(inner),
        (_, e) => open_err(py, created, capacity, e),
    };
    if let Some(n) = name {
        o = o.name(n).map_err(&map_err)?;
    }
    let inner = o.open().map_err(&map_err)?;
    Ok(PyTree::wrap(Arc::new(inner)))
}

/// See [`open_arena`]. The shared arena is Linux-only, like the `memfd` it maps.
///
/// Present on every platform on purpose: see `offline.rs`.
#[cfg(not(target_os = "linux"))]
#[pyfunction]
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = None, interp = None, frame_headroom = 0))]
#[allow(clippy::needless_pass_by_value)]
pub fn open_arena(
    name: Option<&str>,
    domain: Option<u32>,
    mode: &str,
    // These must track the Linux signature, or a `str` config would give the generic `TypeError` this stub prevents.
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

/// Thirty-two bytes as 64 lowercase hex characters.
///
/// A string, not `bytes`, so it compares with what `tf_tree doctor` printed.
fn hex32(bytes: [u8; 32]) -> String {
    use core::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
