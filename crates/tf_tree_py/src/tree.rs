//! `Tree` and `Plan` (`docs/PHASE3.md` §4).

use numpy::{PyArray1, PyArray2, PyArray3, PyArrayMethods, PyUntypedArrayMethods};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;

use tf_tree::{AttachMode, Capacity, EdgeCfg, InterpPolicy, Layout, Stamp, SystemDomain, Tree};

use crate::errors::{lookup_err, BufferError, FrameNotDeclaredError, TfTreeError};

/// Releasing the GIL costs a measured 40 ns; a depth-3 lookup costs ~150 ns
/// (`docs/PHASE3.md` §2). So releasing for a scalar would add 27% for
/// parallelism nobody can use inside a 150 ns window, and *not* releasing for a
/// large batch would stall other threads.
///
/// The rule is expressed in estimated work rather than element count, because
/// depth varies. Below the threshold the worst case is under 1 µs of GIL
/// retention — far below CPython's 5 ms switch interval, so no other thread
/// notices. Above it the worst case is a 4% overhead. **Both sides are cheap,
/// which is why the exact constant does not need tuning**; what matters is that
/// neither branch is ever badly wrong.
const GIL_RELEASE_THRESHOLD_NS: u64 = 1_000;
/// Rough per-step cost used only to place the threshold above.
const NS_PER_STEP_ESTIMATE: u64 = 55;

/// A transform tree.
#[pyclass(name = "Tree", module = "tf_tree", frozen)]
pub struct PyTree {
    inner: Tree,
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
    fn plan(&self, target: &str, source: &str) -> PyResult<PyPlan> {
        let t = self
            .inner
            .frame(target)
            .map_err(|_| FrameNotDeclaredError::new_err(format!("no frame named {target:?}")))?;
        let s = self
            .inner
            .frame(source)
            .map_err(|_| FrameNotDeclaredError::new_err(format!("no frame named {source:?}")))?;
        let plan = self.inner.plan(t, s).map_err(lookup_err)?;
        Ok(PyPlan {
            plan: Box::new(plan),
            tree: self.inner_ptr(),
        })
    }

    /// Whether this tree's arena is shared with other processes.
    fn is_shared(&self) -> bool {
        self.inner.is_shared()
    }

    /// Whether this process may publish into this tree.
    fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }

    fn __repr__(&self) -> String {
        format!(
            "<tf_tree.Tree shared={} writable={}>",
            self.inner.is_shared(),
            self.inner.is_writable()
        )
    }
}

impl PyTree {
    fn inner_ptr(&self) -> *const Tree {
        &self.inner
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
    /// The tree the plan reads through.
    ///
    /// # Safety
    ///
    /// A raw pointer because `Plan` must not borrow the `Tree` across the
    /// Python boundary — `#[pyclass]` cannot carry a lifetime. It is kept valid
    /// by the `_tree` attribute Python holds on every `Plan`, which keeps the
    /// owning `Tree` object alive for at least as long as the plan.
    tree: *const Tree,
}

// SAFETY: `Tree` is `Send + Sync` (its arena is accessed only through
// `tf_tree_core`'s atomic protocols), and this pointer is never used to create a
// `&mut`. The pointee outlives every use because Python holds a reference to the
// owning `Tree` for the life of the `Plan`.
unsafe impl Send for PyPlan {}
unsafe impl Sync for PyPlan {}

impl PyPlan {
    /// # Safety
    ///
    /// Relies on the module invariant above: the owning `Tree` is kept alive by
    /// a Python reference for at least as long as this `Plan`.
    fn tree(&self) -> &Tree {
        // SAFETY: see the field's docs and the `unsafe impl`s above.
        unsafe { &*self.tree }
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
    /// **Positional-only.** `METH_FASTCALL` is a measured 29 ns cheaper than
    /// `PyArg_ParseTuple` for one argument — 20% of a depth-3 budget (§4.2).
    #[pyo3(signature = (stamps, /))]
    fn at<'py>(&self, py: Python<'py>, stamps: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        if let Ok(arr) = stamps.cast::<PyArray1<i64>>() {
            let n = arr.len();
            let out = PyArray3::<f64>::zeros(py, [n, 4, 4], false);
            self.fill(py, arr, &out, n)?;
            return Ok(out.into_any());
        }
        let stamp = stamp_from_any(stamps)?;
        let g = self.tree().guard();
        let iso = self
            .plan
            .at(&g, Stamp::<SystemDomain>::from_nanos(stamp))
            .map_err(lookup_err)?;
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
    #[pyo3(signature = (stamps, out, /))]
    fn at_into(
        &self,
        py: Python<'_>,
        stamps: &Bound<'_, PyArray1<i64>>,
        out: &Bound<'_, PyArray3<f64>>,
    ) -> PyResult<()> {
        let n = stamps.len();
        self.fill(py, stamps, out, n)
    }

    /// The most recent transform on this path.
    fn latest<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyArray2<f64>>> {
        let g = self.tree().guard();
        let iso = self.plan.latest(&g).map_err(lookup_err)?;
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
        let tol = tf_tree::ErrBound {
            rot_rad: ang,
            trans: lin,
        };
        let g = self.tree().guard();
        let (stamps, poses) = self
            .plan
            .at_adaptive(
                &g,
                (Stamp::from_nanos(start_ns), Stamp::from_nanos(end_ns)),
                tol,
                &mut scratch,
            )
            .map_err(lookup_err)?;

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

        // SAFETY: both arrays were just checked C-contiguous, and `out` is
        // exclusively borrowed for this call. Taken *before* `allow_threads` and
        // held across it: NumPy refuses to resize an array while a buffer is
        // exported, which is what keeps these pointers valid (§6.2).
        let (src, dst) = unsafe { (stamps.as_slice()?, out.as_slice_mut()?) };

        let est = (n as u64)
            .saturating_mul(self.plan.len() as u64)
            .saturating_mul(NS_PER_STEP_ESTIMATE);
        let plan = *self.plan;
        let tree = self.tree();

        let mut run = || {
            let g = tree.guard();
            // Raw nanoseconds: `at_many_into` takes `&[i64]` precisely so this
            // path does not have to allocate a `Vec<Stamp>` — which would be
            // the intermediate buffer `at_into` exists to avoid.
            plan.at_many_into::<SystemDomain>(&g, src, Layout::Mat4, dst)
        };
        let res = if est >= GIL_RELEASE_THRESHOLD_NS {
            // `detach` is PyO3 0.29's name for what was `allow_threads`.
            // Touch no Python object inside (§6.2): only the raw slices above.
            py.detach(run)
        } else {
            run()
        };
        res.map_err(lookup_err)
    }
}

/// Build an in-process tree from a simple edge list.
///
/// Topology is builder-time (decision `0004`), so there is no `declare_*` on a
/// live tree: the layout is a property of the arena, fixed when it is created.
#[pyfunction]
#[pyo3(signature = (edges, *, capacity = 1024))]
pub fn build(edges: Vec<(String, String)>, capacity: u32) -> PyResult<PyTree> {
    let mut b = tf_tree::TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for (parent, child) in &edges {
        b = b.dynamic_edge(parent, child, EdgeCfg::new(Capacity::slots(capacity)));
    }
    let inner = b
        .build()
        .map_err(|e| TfTreeError::new_err(format!("{e}")))?;
    Ok(PyTree { inner })
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
    let c = tree
        .inner
        .frame(child)
        .map_err(|_| FrameNotDeclaredError::new_err(format!("no frame named {child:?}")))?;
    let p = tree
        .inner
        .frame(parent)
        .map_err(|_| FrameNotDeclaredError::new_err(format!("no frame named {parent:?}")))?;
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
        .map_err(|e| TfTreeError::new_err(format!("{e}")))?;
    publisher
        .push(stamp_ns, &iso)
        .map_err(|e| TfTreeError::new_err(format!("{e:?}")))
}

/// Attach to a running arena (`docs/PHASE3.md` §4.1).
///
/// **`mode="ro"`, and creation is refused outright** — both differ from the
/// Rust in-process defaults on purpose (D18). Most Python consumers are notebooks,
/// analysis scripts and visualisers; they must be *incapable* of corrupting a
/// robot's transform tree, which a `PROT_READ` mapping enforces with the MMU.
/// And a notebook started before the robot must fail loudly rather than create
/// an empty arena the real publisher then refuses to join.
#[pyfunction]
#[pyo3(signature = (*, name = None, domain = None, mode = "ro"))]
pub fn open_arena(name: Option<&str>, domain: Option<u32>, mode: &str) -> PyResult<PyTree> {
    let attach = match mode {
        "ro" => AttachMode::ReadOnly,
        "rw" => AttachMode::ReadWrite,
        other => {
            return Err(PyValueError::new_err(format!(
                "mode must be 'ro' or 'rw', got {other:?}"
            )))
        }
    };
    // `create` is deliberately not a parameter yet: creating an arena needs a
    // layout (decision `0004` sizes it from the declared edges), and that is
    // not wired through from Python. A consumer-only default is also what §4.1
    // asks for — a notebook started before the robot must fail loudly.
    let mut o = tf_tree::Open::new()
        .mode(attach)
        .create(tf_tree::CreatePolicy::Never);
    if let Some(d) = domain {
        o = o.domain(d);
    }
    if let Some(n) = name {
        o = o
            .name(n)
            .map_err(|e| TfTreeError::new_err(format!("{e}")))?;
    }
    let inner = o.open().map_err(|e| TfTreeError::new_err(format!("{e}")))?;
    Ok(PyTree { inner })
}
