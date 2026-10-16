---
paths: ["crates/tf_tree_core/**", "crates/tf_tree_arena/**", "crates/tf_tree/src/**"]
---

- `just miri` runs every test in `tf_tree_core` and `tf_tree_arena`, plus `tf_tree`'s lib and `owned_writer`. A test with a spinner thread, an unbounded loop or thousands of iterations needs a `cfg(miri)` bound, or the job hangs (a 200k-round test ran 28 min against a 5-min job).
- `just loom` catches a wrong liveness predicate only at `LOOM_MAX_PREEMPTIONS>=3`; unbounded is slower and misses it. The bound is pinned in `loom_tests.rs`; keep it.
- A model that idealises the exclusion primitive proves the property by construction. Run a disabled-mechanism control: it must go red.
- A stress test that is green may never have raced. The writer runs until stopped, the reader owns the iteration count, and the baseline must not be the unguarded read. Run the inverse mutation.
- A soak over a live arena asserts a write-side quantity (live writers, freshest-sample age). A perfect read score on a frozen arena is the tell.
