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

/// A scalar keeps the GIL (~193 ns lookup); a larger batch releases it (`docs/PHASE3.md` §6.1).
pub(crate) const GIL_RELEASE_THRESHOLD_NS: u64 = 1_000;
/// Rough per-step cost behind the threshold (`docs/PHASE3.md` §6.1).
const NS_PER_STEP_ESTIMATE: u64 = 64;

/// §6.1's rule, in one place so the two callers cannot drift.
#[inline]
const fn release_the_gil(n: usize, depth: usize) -> bool {
    let est = (n as u64)
        .saturating_mul(depth as u64)
        .saturating_mul(NS_PER_STEP_ESTIMATE);
    est >= GIL_RELEASE_THRESHOLD_NS
}

/// The depth-3 crossover (`n = 6` releases, `n = 5` does not; §6.1), asserted at compile time.
const _: () = assert!(
    release_the_gil(6, 3) && !release_the_gil(5, 3),
    "the depth-3 GIL crossover moved; docs/PHASE3.md §6.1 says n = 6"
);

/// Parse `layout=`; no default and no inference (`docs/API.md` R4).
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

/// Parse `policy` of [`PyPlan::at_extrapolating`]; no default.
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

/// Write one extrapolated pose in `layout`; `QuatTwist` is refused, as in the C ABI.
fn write_pose_unchecked(pose: &tf_tree::Iso3, layout: Layout, dst: &mut [f64]) {
    match layout {
        Layout::Quat => tf_tree::write_quat(pose, dst),
        // `write_extrapolated` already refused the rest; no `PyResult` under `detach`.
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
    pub(crate) inner: Arc<Tree>,
    /// Set only by [`crate::ingest::ingest_bag`] (`0046`); immutable, see [`Self::source_live`].
    pub(crate) source: Option<SourceInfo>,
    /// Whether [`Self::source`] still describes this tree; cleared by the first `publisher()`.
    pub(crate) source_live: std::sync::atomic::AtomicBool,
}

/// The recording a [`PyTree`] was ingested from — [`PyTree::source`]'s contents.
pub(crate) struct SourceInfo {
    /// The recording's path as given to `ingest_bag`.
    pub(crate) path: String,
    /// BLAKE3 of the recording's **bytes**.
    pub(crate) digest: [u8; 32],
    /// `Survey::transforms_read`.
    pub(crate) transforms: u64,
    /// How many declared edges the recording carried no sample for.
    pub(crate) edges_without_samples: usize,
    /// The interval the **recording** covers, at most the queryable window (`Tree.span()`).
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

    /// [`Self::source`], if it still describes this tree (`Acquire` against `publisher()`'s `Release`).
    pub(crate) fn provenance(&self) -> Option<&SourceInfo> {
        if self.source_live.load(std::sync::atomic::Ordering::Acquire) {
            self.source.as_ref()
        } else {
            None
        }
    }
}

/// Whether this plan samples anything at evaluation time; mirrors `Plan::check_domain_tag`.
fn samples_anything(plan: &tf_tree::Plan) -> bool {
    plan.steps()
        .iter()
        .any(|s| matches!(s, tf_tree::Step::Dyn { .. }))
}

/// Extract an integer-nanosecond stamp, refusing floats (§3); arrays keep numpy's own `TypeError`.
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

/// `isinstance(obj, numpy.floating)`; `false` if it cannot be asked (it only picks a message).
fn is_numpy_floating_scalar(obj: &Bound<'_, PyAny>) -> bool {
    obj.py()
        .import("numpy")
        .and_then(|np| np.getattr("floating"))
        .and_then(|floating| obj.is_instance(&floating))
        .unwrap_or(false)
}

#[pymethods]
impl PyTree {
    /// Compile a plan from `source` to `target`; compile once and reuse.
    ///
    /// # `domain=`
    ///
    /// The `u8` time-domain tag the queries on this plan will be in (`0038`);
    /// `tf_tree.SYSTEM_DOMAIN` and siblings name the built-ins, user domains start at `4`
    /// (`docs/API.md` §2.5). Default `0`. A mismatch is refused at plan time; the core
    /// re-checks every call.
    #[pyo3(signature = (target, source, /, *, domain = 0))]
    fn plan(slf: &Bound<'_, PyTree>, target: &str, source: &str, domain: u8) -> PyResult<PyPlan> {
        let py = slf.py();
        let this = slf.get();
        let t = resolve_frame(py, &this.inner, target)?;
        let s = resolve_frame(py, &this.inner, source)?;
        let plan = this
            .inner
            .plan(t, s)
            .map_err(|e| lookup_err(py, &this.inner, domain, e))?;
        if samples_anything(&plan) && plan.domain() != domain {
            return Err(plan_domain_err(py, target, source, plan.domain(), domain));
        }
        Ok(PyPlan {
            plan: Box::new(plan),
            domain,
            tree: slf.clone().unbind(),
        })
    }

    /// Claim `child`'s edge and return a publisher for it (§4.3).
    ///
    /// Argument order is **(child, parent)**.
    ///
    /// Use it as a context manager
    #[pyo3(signature = (child, parent, /))]
    fn publisher(slf: &Bound<'_, PyTree>, child: &str, parent: &str) -> PyResult<PyPublisher> {
        let py = slf.py();
        let this = slf.get();
        // The tree stops being the recording once it can be written to. `Release` pairs with `provenance()`'s `Acquire`.
        this.source_live
            .store(false, std::sync::atomic::Ordering::Release);
        let c = resolve_frame(py, &this.inner, child)?;
        let p = resolve_frame(py, &this.inner, parent)?;
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
    /// The replacement is atomic. `source` labels the recording (`null` in the manifest when
    /// absent) and overrides the label only: `source_digest` comes from `Tree.source`, else
    /// all-zero. The GIL is released for the copy.
    #[pyo3(signature = (path, /, *, source = None))]
    fn freeze(&self, py: Python<'_>, path: PathBuf, source: Option<&str>) -> PyResult<()> {
        let prov = self.provenance();
        let digest = prov.map_or([0u8; 32], |s| s.digest);
        let label = source.or_else(|| prov.map(|s| s.path.as_str()));
        crate::offline::freeze_impl(py, &self.inner, &path, label, digest)
    }

    /// The recording this tree was ingested from, or `None` (also `None` for a tree built in
    /// Python, opened with `open_file`, or once `publisher()` has been called).
    ///
    /// Keys: `path`, `digest` (hex), `transforms`, `edges_without_samples`,
    /// `recording_start_ns`, `recording_end_ns` (the recording's, not the rings'; see `Tree.span`).
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
    /// `(t0, t1)` in nanoseconds, or `None` when every step is static. Otherwise the
    /// **intersection** of the dynamic edges' retained windows; an empty one is returned, not
    /// raised. An outer bound, not a coverage guarantee.
    #[pyo3(signature = (target, source, /))]
    fn span(&self, py: Python<'_>, target: &str, source: &str) -> PyResult<Option<(i64, i64)>> {
        crate::offline::span_impl(py, &self.inner, target, source)
    }

    /// The frame names on this tree, in declaration order (§4.4).
    ///
    /// A snapshot; a tree inherited across a `fork()` raises rather than answering `[]`.
    fn frames(&self) -> PyResult<Vec<String>> {
        crate::offline::frames_impl(&self.inner)
    }

    /// The edges on this tree as `(parent, child)` name pairs (§4.4).
    ///
    /// Names only (`docs/PHASE5.md` §4.2). Rebuilding from them yields the *graph* only: every
    /// edge comes back dynamic. A snapshot.
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
    /// owner" (`0043`, `0057`). **Nothing calls it for you** (`0019`).
    ///
    /// ```python
    /// if tree.owner_lost():
    ///     tree.inherit_ownership()
    /// ```
    #[cfg(target_os = "linux")]
    fn owner_lost(&self) -> bool {
        self.inner.owner_lost()
    }

    /// Inherit the owner role from a departed owner and begin serving (§3.5, `0044`).
    ///
    /// Returns `"Inherited"`, `"OwnerAlive"`, `"Contended"`, `"ReadOnly"` or `"NotApplicable"`.
    /// Anything but `"Inherited"` means this process is not the owner; `"OwnerAlive"` and
    /// `"Contended"` are retryable while `owner_lost()` stays `True`. `"ReadOnly"`: open with
    /// `mode="rw"` (D18).
    ///
    /// # Errors
    ///
    /// `TfTreeError` if the `fcntl` fails or the rendezvous socket cannot be bound.
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
    /// Sums claim leases no live process holds and participant records the kernel has released;
    /// the only collector for a dead **owner**.
    ///
    /// **Dangerous** where a Rust component served an arena from `TreeBuilder::build_shared` and
    /// published by hand (out of contract, `0031`): it frees running processes' records.
    /// `open_arena` is safe. See `docs/RUNBOOK.md`, *ParticipantTableFull*.
    ///
    /// `0` for a read-only, in-process or rendezvous-less tree.
    #[cfg(target_os = "linux")]
    fn reap_dead(&self) -> usize {
        self.inner.reap_dead() + self.inner.reap_participants()
    }

    // Off Linux a tree is in-process: these arms exist and do not raise.

    /// Always `False` off Linux.
    #[cfg(not(target_os = "linux"))]
    fn owner_lost(&self) -> bool {
        false
    }

    /// Always `"NotApplicable"` off Linux.
    #[cfg(not(target_os = "linux"))]
    fn inherit_ownership(&self) -> PyResult<&'static str> {
        Ok("NotApplicable")
    }

    /// Always `0` off Linux.
    #[cfg(not(target_os = "linux"))]
    fn reap_dead(&self) -> usize {
        0
    }

    /// Which arena instance this tree is attached to, as 32 hex characters.
    ///
    /// All-zero for an in-process tree; tells apart same-named segments after the owner was
    /// replaced.
    ///
    /// # Errors
    ///
    /// `ChildProcessDetachedError` on a tree inherited across a `fork()`.
    fn instance_uuid(&self) -> PyResult<String> {
        if self.inner.detached() {
            return Err(crate::errors::detached_err());
        }
        Ok(self.uuid_hex())
    }

    fn __repr__(&self) -> String {
        // Describes a detached tree: a raising `__repr__` breaks `print`.
        let instance = if self.inner.detached() {
            " detached-by-fork".to_string()
        } else {
            let uuid = self.uuid_hex();
            if uuid.chars().all(|c| c == '0') {
                String::new()
            } else {
                format!(" instance={}", &uuid[..8])
            }
        };
        format!(
            "<tf_tree.Tree shared={} writable={}{instance}>",
            py_bool(self.inner.is_shared()),
            py_bool(self.inner.is_writable())
        )
    }

    /// One transform, without compiling a plan first (§4.2).
    ///
    /// The plan is cached per **thread** (§7.2); prefer `tree.plan(...)` in a loop.
    ///
    /// # `domain=`
    ///
    /// As [`PyTree::plan`]'s (`0038`); checked per call, so the refusal names two tags rather
    /// than a route.
    #[pyo3(signature = (target, source, stamp_ns, /, *, domain = 0))]
    fn lookup<'py>(
        &self,
        py: Python<'py>,
        target: &str,
        source: &str,
        stamp_ns: &Bound<'py, PyAny>,
        domain: u8,
    ) -> PyResult<Bound<'py, PyArray2<f64>>> {
        let stamp_ns = stamp_from_any(stamp_ns)?;
        let iso = self
            .inner
            .lookup_tagged(target, source, stamp_ns, domain)
            // Name the missing frame here, where both names are in scope ([`unknown_frame_err`]).
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
    /// The instance uuid as 32 lowercase hex characters; **callers check `detached()` first**.
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
/// `frozen` and `Sync`: a `Plan` is `Copy`, so threads may evaluate one concurrently. Fields
/// added here must respect pyclass alignment (`0042`).
#[pyclass(name = "Plan", module = "tf_tree", frozen)]
pub struct PyPlan {
    plan: Box<tf_tree::Plan>,
    /// The time-domain tag every query through this handle carries (`0038`).
    domain: u8,
    /// A reference-counted handle to the tree, so the arena cannot outlive its readers.
    tree: Py<PyTree>,
}

impl PyPlan {
    /// The tree this plan reads through; [`PyTree`] is `frozen`, so `get` needs no GIL token.
    fn tree(&self) -> &Tree {
        &self.tree.get().inner
    }
}

#[pymethods]
impl PyPlan {
    /// Evaluate at one stamp, or at an array of stamps.
    ///
    /// Scalar in, `(4, 4)` out; `(N,)` in, `(N, 4, 4)` out. `stamps` is positional-only (§4.2).
    ///
    /// # `layout=`
    ///
    /// Keyword-only, explicit, no default (`docs/API.md` R4):
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
    /// The methods stay vectorcall, pinned by
    /// `tests/python/test_api.py::test_the_hot_methods_are_emitted_as_meth_fastcall`.
    #[pyo3(signature = (stamps, /, *, layout = None))]
    fn at<'py>(
        &self,
        py: Python<'py>,
        stamps: &Bound<'py, PyAny>,
        layout: Option<&str>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if let Some(name) = layout {
            let layout = layout_from_str(name)?;
            if layout != Layout::Mat4 {
                return self.at_layout(py, stamps, layout);
            }
        }
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
        // SAFETY: freshly allocated: unaliased, exactly 16 contiguous f64.
        let slice = unsafe { out.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(out.into_any())
    }

    /// Evaluate a batch into a caller-provided `(N, 4, 4)` float64 array.
    ///
    /// The tier that allocates nothing (§5.2); `out` is validated **completely before any element
    /// is written**. `layout=` is [`at`](Self::at)'s and `out` follows it (R2); stamp dispatch is
    /// `at`'s (`docs/PHASE3.md` §3).
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
        // `reject_device_memory` is skipped on the numpy path (~120 ns); a successful `cast` proves
        // host memory.
        //
        // Dispatch on `stamps`, then validate `out`, so the error blames the right argument. An
        // `int` leads; the array cast is fallen through so `stamp_from_any` has the last word
        // (`docs/PHASE3.md` §3).
        if !stamps.is_instance_of::<pyo3::types::PyInt>() {
            if let Ok(stamps) = stamps.cast::<PyArray1<i64>>() {
                let arr = match out.cast::<PyArray3<f64>>() {
                    Ok(a) => a,
                    Err(_) => {
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
        // SAFETY: checked C-contiguous, (4, 4) and writable; aliasing is the caller's.
        let slice = unsafe { arr.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(())
    }

    /// Evaluate past the newest sample under an explicit policy, and get back how far past it
    /// that was (`0039`).
    ///
    /// Returns `(poses, by_ns)`. `policy` is required: `"error"` (refuse, as `at` does),
    /// `"hold"` (the newest pose) or `"constant_twist"` (extend the screw the two newest
    /// samples imply).
    ///
    /// | `stamps` | `poses` | `by_ns` |
    /// | --- | --- | --- |
    /// | `int` | `(4, 4)` float64 | `int` |
    /// | `(N,)` int64 | `(N, 4, 4)` float64 | `(N,)` **int64 array** |
    ///
    /// `by_ns` is per stamp (`max(0, stamp - newest_common)`). `layout=` accepts `mat4` and `quat`
    /// only. Cost: a loop over the scalar form under one `Guard` (`0039` §4).
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
        let out = if layout == Layout::Mat4 {
            let a = PyArray2::<f64>::zeros(py, [4, 4], false);
            // SAFETY: freshly allocated: unaliased, exactly 16 contiguous f64.
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
    /// `poses` and `by_ns` take the shapes `at_extrapolating` returns. A `LookupError` on
    /// element *k* leaves `0..k` written, as in `at_into`.
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
        write_extrapolated(&tf_tree::Iso3::IDENTITY, layout, &mut [0.0; 16])?;
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
            // SAFETY: `as_slice` checks contiguity and alignment; writers to the array while `src`
            // lives are the caller's to rule out (`by_ns` overlap is refused below).
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

        // Refuse a `by_ns` aliasing `stamps` (byte ranges, so views too): both are `int64`.
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

        // SAFETY: `check_out` proved both writable and shaped; the range check rules out aliasing.
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
    /// stamp (§5.6). `lin` is metres and `ang` radians, defaulting to `docs/PHASE3.md` §4.2's
    /// values.
    #[pyo3(signature = (start_ns, end_ns, /, *, lin = 1e-3, ang = 1e-4))]
    fn adaptive<'py>(
        &self,
        py: Python<'py>,
        start_ns: &Bound<'py, PyAny>,
        end_ns: &Bound<'py, PyAny>,
        lin: f64,
        ang: f64,
    ) -> PyResult<Knots<'py>> {
        let start_ns = stamp_from_any(start_ns)?;
        let end_ns = stamp_from_any(end_ns)?;
        if !(lin.is_finite() && ang.is_finite()) || lin <= 0.0 || ang <= 0.0 {
            return Err(PyValueError::new_err(
                "lin and ang must be finite and positive",
            ));
        }
        // `SystemDomain` only types `scratch` and the stamp slice (`0038`); `self.domain` is what is checked.
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
            // SAFETY: both arrays were just allocated here: unaliased and contiguous.
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

    /// The **dynamic** edges this plan samples, as `(parent, child)` pairs (§4.4); shorter than
    /// [`depth`](Self::depth) across a static edge.
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

        if !is_writeable(out.as_untyped()) {
            return Err(BufferError::new_err(
                "out is not writable (NumPy reports NPY_ARRAY_WRITEABLE clear); \
                 a read-only mapping cannot receive a transform",
            ));
        }

        // SAFETY: no alias may write `stamps` or touch `out` while the slices live, across the
        // `detach`: the caller's to uphold. `out` was checked writable above.
        let (src, dst) = unsafe { (stamps.as_slice()?, out.as_slice_mut()?) };

        let plan = *self.plan;
        let tree = self.tree();
        let domain = self.domain;

        let mut run = || {
            let g = tree.guard();
            plan.at_many_into_tagged(&g, src, domain, Layout::Mat4, dst)
        };
        let res = if release_the_gil(n, self.plan.len()) {
            py.detach(run)
        } else {
            run()
        };
        res.map_err(|e| lookup_err(py, tree, domain, e))
    }

    /// [`PyPlan::at_extrapolating`]'s array half: allocates `(N, 4, 4)` poses and `(N,)` distances.
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
        write_extrapolated(&tf_tree::Iso3::IDENTITY, layout, &mut [0.0; 16])?;

        let n = stamps.len();
        let e = layout.elems();
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
            res.map_err(|e| lookup_err(py, tree, domain, e))?;
        }
        Ok((poses.into_any(), dist.into_any()))
    }

    /// [`PyPlan::at`]'s `layout=` path: allocate the right shape and fill it.
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
            // A one-element batch: `docs/PHASE3.md` §11.1 requires `at(t)` == `at([t])[0]` bit-exactly.
            let src = [stamp_from_any(stamps)?];
            if layout.is_f32() {
                let out = PyArray1::<f32>::zeros(py, [e], false);
                // SAFETY: freshly allocated and contiguous.
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

    /// [`PyPlan::at_into`]'s `layout=` path: validate `out`, then fill it (§5.3). Stamp dispatch is
    /// [`Self::at_layout`]'s (`docs/PHASE3.md` §3, NORMATIVE).
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
            // SAFETY: `check_out` proved C-contiguous, shaped and writable; aliasing is the caller's.
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
/// `tf_tree::Publisher` is `Send + !Sync`, so it sits in a mutex (`docs/PHASE3.md` §7.1); it
/// carries its own `Arc<Tree>` via [`OwnedWriter`].
#[pyclass(name = "Publisher", module = "tf_tree")]
pub struct PyPublisher {
    /// How a failed `push` names this edge: the caller's own two strings, captured at claim time.
    edge: String,
    /// `None` after `__exit__` or `release()`, so a use-after-release is a clear Python error.
    ///
    /// An [`OwnedWriter`] (`docs/decisions/0017`); `push` forwards fork-checked `EdgeWriter::push`.
    inner: Mutex<Option<OwnedWriter>>,
    /// The tree this claim was made on, so an **empty** `push_many` still raises
    /// `ChildProcessDetachedError` in a fork child (`docs/PHASE3.md` §8.1, NORMATIVE). `Weak`:
    /// it does not outlive `release()`.
    tree: std::sync::Weak<Tree>,
}

#[pymethods]
impl PyPublisher {
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Release the claim on scope exit (§4.3); `Drop` alone is unordered at finalization.
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
        let stamp_ns = stamp_from_any(stamp_ns)?;
        let iso = iso_from_quat7(&quat7)?;
        let g = self.lock()?;
        let p = g.as_ref().ok_or_else(released)?;
        p.push(stamp_ns, &iso)
            .map_err(|e| push_err(py, self.tree.upgrade().as_deref(), &self.edge, e))
    }

    /// Publish a whole batch: `(N,)` stamps and `(N, 7)` poses (one FFI crossing, not N).
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
        // An empty batch never reaches `push`'s fork check (`docs/PHASE3.md` §8.1).
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

/// DLPack device types a CPU kernel may write to (`docs/PHASE3.md` §5.5): `kDLCPU`,
/// `kDLCUDAHost`, `kDLROCMHost`, `kDLCUDAManaged`.
const HOST_DEVICE_TYPES: [i32; 4] = [1, 3, 11, 13];

/// Refuse an `out` buffer that does not live where a CPU can write it (DLPack's
/// `__dlpack_device__`, D8).
fn reject_device_memory(obj: &Bound<'_, PyAny>) -> PyResult<()> {
    let Ok(f) = obj.getattr("__dlpack_device__") else {
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

/// Downcast `out` for a `layout=` write, or say exactly what was wanted; the downcast checks
/// dtype, [`check_out`] rank and shape.
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

/// Whether NumPy marks this array writable (`as_slice_mut` does not check; §5.5).
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

/// Parse `interp=` (`docs/PHASE3.md` §4.1): exactly the two `InterpPolicy` variants.
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

/// [`interp_from_str`] backwards; `InterpPolicy` is exhaustive, so a third policy is a compile
/// error here.
pub(crate) fn interp_name(policy: InterpPolicy) -> &'static str {
    match policy {
        InterpPolicy::ScLerp => "sclerp",
        InterpPolicy::LerpSlerp => "lerpslerp",
    }
}

/// Build an in-process tree from a simple edge list.
///
/// Topology is builder-time (`0004`).
///
/// # `interp=`
///
/// Defaults to **`"sclerp"`** (`docs/PROJECT.md` §5 D5). `"lerpslerp"` has no exact body
/// twist, so `layout="quat_twist"` over it raises `DerivativesUnavailableError`.
///
/// # `frame_headroom=`
///
/// Spare **frame-name** slots (`TreeBuilder::frame_headroom`); the frame table never grows
/// (invariant 3), so with 0 `Tree::frame()` on the arena gets `CapacityExceeded`. There is no
/// `edge_headroom` (`docs/PHASE5.md` §5.8).
///
/// # `edges` also takes a topology config
///
/// A list of pairs makes every edge **dynamic**. The *text* of a topology config (`0041`)
/// also expresses static edges, sizes, rates and domains; `capacity=` and `interp=` are
/// refused beside it.
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

/// Build from topology-config text — `0041`. Rendered here because `ConfigError` borrows from `text`.
fn config_builder(text: &str, frame_headroom: u32) -> PyResult<tf_tree::TreeBuilder> {
    let cfg = tf_tree_bridge::TopologyConfig::parse(text)
        .map_err(|e| PyValueError::new_err(format!("topology config: {e}")))?;

    // Ask the config first, as `tf_tree_cli::topology` does: the parser misses a multi-hop cycle.
    if let Some(child) = cfg.cycle_child() {
        return Err(PyValueError::new_err(format!(
            "topology config: the declared topology has a cycle through frame \
             {child:?} — following its parent links returns to it"
        )));
    }

    let mut b = cfg.builder();
    if frame_headroom != 0 {
        b = b.frame_headroom(frame_headroom);
    }
    Ok(b)
}

/// Build a heap tree from topology-config text — `0041`.
fn build_from_config(text: &str, frame_headroom: u32) -> PyResult<PyTree> {
    let inner = config_builder(text, frame_headroom)?
        .build()
        .map_err(config_build_err)?;
    Ok(PyTree::wrap(Arc::new(inner)))
}

/// A `BuildError` from a config, phrased for somebody holding a text file; `TfTreeError`, like
/// `build_err`.
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
/// Takes `[qw qx qy qz tx ty tz]`, not a 4x4 (a nearly rigid matrix has no exact quaternion).
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
    let stamp_ns = stamp_from_any(stamp_ns)?;
    if quat7.len() != 7 {
        return Err(PyValueError::new_err(
            "expected [qw, qx, qy, qz, tx, ty, tz]",
        ));
    }
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
/// `mode="ro"` and creation off, on purpose (D18).
///
/// `domain=` is the `u32` *rendezvous* domain, **not** `Tree.plan`'s `u8` time-domain tag
/// (`0038`).
///
/// # Creating
///
/// `create=[(parent, child), ...]` (or topology-config text, as [`build`]'s `edges`) creates
/// the arena when absent (`0004`). `capacity`, `interp` and `frame_headroom` are [`build`]'s;
/// `interp` is parsed even without `create`. Creating requires `mode="rw"`.
#[cfg(target_os = "linux")]
#[pyfunction]
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = None, interp = None, frame_headroom = 0))]
#[allow(clippy::too_many_arguments)] // the eighth argument is PyO3's token (`0058` §5)
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
    // `interp` is validated even when nothing is created; `create=` takes `build`'s two forms (`0041`).
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
    // One mapper for both failures; a config path must not reach `open_err`'s build prose.
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

/// See [`open_arena`]. Linux-only; present on every platform on purpose (see `offline.rs`).
#[cfg(not(target_os = "linux"))]
#[pyfunction]
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = None, interp = None, frame_headroom = 0))]
#[allow(clippy::needless_pass_by_value)]
pub fn open_arena(
    name: Option<&str>,
    domain: Option<u32>,
    mode: &str,
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

/// Thirty-two bytes as 64 lowercase hex characters (a string, to compare with `tf_tree doctor`).
fn hex32(bytes: [u8; 32]) -> String {
    use core::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
