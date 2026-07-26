//! `Tree` and `Plan` (`docs/PHASE3.md` §4).

use numpy::{PyArray1, PyArray2, PyArray3, PyArrayMethods, PyUntypedArrayMethods};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;
use std::sync::Mutex;

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
    pub(crate) inner: Tree,
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
        let t = this
            .inner
            .frame(target)
            .map_err(|_| FrameNotDeclaredError::new_err(format!("no frame named {target:?}")))?;
        let s = this
            .inner
            .frame(source)
            .map_err(|_| FrameNotDeclaredError::new_err(format!("no frame named {source:?}")))?;
        let plan = this.inner.plan(t, s).map_err(lookup_err)?;
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
        let c = this
            .inner
            .frame(child)
            .map_err(|_| FrameNotDeclaredError::new_err(format!("no frame named {child:?}")))?;
        let p = this
            .inner
            .frame(parent)
            .map_err(|_| FrameNotDeclaredError::new_err(format!("no frame named {parent:?}")))?;
        let publisher = this
            .inner
            .claim(c, p)
            .map_err(|e| TfTreeError::new_err(format!("{e}")))?;

        // SAFETY: the only `unsafe` in this binding that is not a numpy slice.
        //
        // `EdgeWriter<'a>` borrows the `Tree`, and a `#[pyclass]` cannot carry a
        // lifetime, so the borrow is extended to `'static` and its validity
        // moved to a runtime guarantee: the `Py<PyTree>` stored alongside is a
        // strong reference, so the `Tree` — and the arena the claim points into
        // — outlives this writer for certain.
        //
        // That is the same guarantee `Plan` relies on, and it is spelled with a
        // refcount rather than a comment *because* the comment version was a
        // use-after-free (see `PyPlan::tree`). The writer is never handed out,
        // only borrowed under the mutex, so no caller can outlive it either.
        //
        // The transmute is behind `extend_to_static`, whose signature pins both
        // types so only the lifetime can differ. Inline, it read
        // `transmute::<EdgeWriter, Publisher>` and compiled for as long as the
        // two happened to be the same size — see `PyPublisher::inner`.
        let writer = unsafe { extend_to_static(publisher) };

        Ok(PyPublisher {
            inner: Mutex::new(Some(writer)),
            _tree: slf.clone().unbind(),
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

    /// Which arena instance this tree is attached to, as 32 hex characters.
    ///
    /// All-zero for an in-process tree. Two processes that resolved the same
    /// name can still hold *different* segments if the owner was replaced
    /// between their `open()` calls; this is what tells them apart, and
    /// comparing names cannot.
    fn instance_uuid(&self) -> String {
        self.inner
            .instance_uuid()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn __repr__(&self) -> String {
        let uuid = self.instance_uuid();
        // Show the instance only when there is one. An all-zero field on an
        // in-process tree is noise that reads like a bug.
        let instance = if uuid.chars().all(|c| c == '0') {
            String::new()
        } else {
            format!(" instance={}", &uuid[..8])
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
    /// `(target, source, topology generation)` — a shared cache behind a lock
    /// would turn the convenience API into a contention point on exactly the
    /// workload free-threading exists to serve (§7.2).
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
            .map_err(lookup_err)?;
        let out = PyArray2::<f64>::zeros(py, [4, 4], false);
        // SAFETY: freshly allocated here; no other reference exists.
        let slice = unsafe { out.as_slice_mut()? };
        tf_tree::write_mat4(&iso, slice);
        Ok(out)
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
    /// **Positional-only.** `METH_FASTCALL` is a measured 29 ns cheaper than
    /// `PyArg_ParseTuple` for one argument — 20% of a depth-3 budget (§4.2).
    #[pyo3(signature = (stamps, /))]
    fn at<'py>(&self, py: Python<'py>, stamps: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
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
        stamps: &Bound<'_, PyAny>,
        out: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
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
        if let Ok(arr) = out.cast::<PyArray2<f64>>() {
            let stamp = stamp_from_any(stamps)?;
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
                .map_err(lookup_err)?;
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

        let arr = match out.cast::<PyArray3<f64>>() {
            Ok(a) => a,
            Err(_) => {
                // Not a numpy array, so it may be device memory. Refuse rather
                // than fault (§5.5): a CPU store to a `cudaMalloc` pointer is
                // undefined, not slow.
                reject_device_memory(out)?;
                return Err(BufferError::new_err(
                    "out must be a writable, C-contiguous (N, 4, 4) float64 array — or \
                     (4, 4) for a scalar stamp — or an object exposing the buffer \
                     protocol with that layout",
                ));
            }
        };
        let stamps = stamps
            .cast::<PyArray1<i64>>()
            .map_err(|_| BufferError::new_err("stamps must be an (N,) int64 array, or an int"))?;
        let n = stamps.len();
        self.fill(py, stamps, arr, n)
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
/// It also borrows the `Tree`. The `Py<PyTree>` below is what makes that sound
/// — the same refcount `Plan` uses, for the same reason and after the same bug.
#[pyclass(name = "Publisher", module = "tf_tree")]
pub struct PyPublisher {
    /// `None` after `__exit__` or `release()`, so a use-after-release is a
    /// clear Python error rather than a claim held past its scope.
    ///
    /// # This is an `EdgeWriter`, not a `Publisher`, and the difference is two
    /// silent bugs
    ///
    /// It held a `Publisher` until a `transmute::<EdgeWriter, Publisher>`
    /// stopped compiling on a size change. That transmute was not a lifetime
    /// extension — it reinterpreted one type as another, and since `publisher`
    /// is `EdgeWriter`'s first field the bytes lined up and the rest were
    /// dropped on the floor:
    ///
    /// * the **claim lease** was never released. `ClaimLease`'s `Drop` is what
    ///   unlocks the edge's OFD byte, so every Python publisher leaked one for
    ///   the life of the process — and a leaked lease is indistinguishable from
    ///   a live writer, so no reaper would ever collect the edge either.
    /// * the **fork guard** was bypassed. `EdgeWriter::push` checks the fork
    ///   generation and `Publisher::push` does not, so a `push` from a
    ///   `multiprocessing` child would have written through a dangling pointer
    ///   into an unmapped page instead of returning `ChildDetached` — and
    ///   `multiprocessing` defaults to `fork` on Linux.
    ///
    /// Neither had a test, because nothing built this crate: it is excluded
    /// from the workspace, so `just test` and `just lint` never saw it. That is
    /// fixed in the `justfile` alongside this.
    inner: Mutex<Option<tf_tree::EdgeWriter<'static>>>,
    /// Keeps the arena alive for at least as long as the claim points into it.
    _tree: Py<PyTree>,
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
        p.push(stamp_ns, &iso)
            .map_err(|e| TfTreeError::new_err(format!("{e:?}")))
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
                // samples before it *were* published.
                TfTreeError::new_err(format!("sample {i} (stamp {stamp}): {e:?}"))
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
    fn lock(&self) -> PyResult<std::sync::MutexGuard<'_, Option<tf_tree::EdgeWriter<'static>>>> {
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

/// Extend an [`tf_tree::EdgeWriter`]'s borrow to `'static`.
///
/// # Safety
///
/// The caller must keep the `Tree` the writer borrows alive for at least as
/// long as the returned value. [`PyTree::publisher`] does that with a
/// `Py<PyTree>` — a refcount, not a promise.
///
/// # Why a function and not an inline `transmute`
///
/// **The signature is the point.** `transmute` will happily convert between two
/// *different* types whose sizes agree, and that is what this replaced: a
/// `transmute::<EdgeWriter, Publisher>` that compiled until a field was added,
/// and until then discarded the claim lease and the fork guard. Here the input
/// and output types are written out and only the lifetime is free, so the same
/// mistake does not compile.
unsafe fn extend_to_static(w: tf_tree::EdgeWriter<'_>) -> tf_tree::EdgeWriter<'static> {
    // SAFETY: same type, and a lifetime the caller has undertaken to honour.
    unsafe { core::mem::transmute(w) }
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
/// **Creating requires `mode="rw"`**, and is refused otherwise rather than
/// quietly ignored. Both of §4.1's reasons for the read-only default survive
/// that: a `ro` consumer still cannot bring an arena into existence, and an
/// `rw` publisher — which has already opted into being able to corrupt the tree
/// — still has to ask.
#[pyfunction]
#[pyo3(signature = (*, name = None, domain = None, mode = "ro", create = None, capacity = 1024))]
pub fn open_arena(
    name: Option<&str>,
    domain: Option<u32>,
    mode: &str,
    create: Option<Vec<(String, String)>>,
    capacity: u32,
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
    let mut o = tf_tree::Open::new().mode(attach).create(match &create {
        None => tf_tree::CreatePolicy::Never,
        Some(_) => tf_tree::CreatePolicy::IfAbsent,
    });
    if let Some(edges) = &create {
        let mut b = tf_tree::TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
        for (parent, child) in edges {
            b = b.dynamic_edge(parent, child, EdgeCfg::new(Capacity::slots(capacity)));
        }
        o = o.layout_if_creating(b);
    }
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
