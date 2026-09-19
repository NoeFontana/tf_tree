# 0034: the depth bound priced two slots the same

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** landed in one change (#251). Both bounds moved, `fold` takes
the two `u32` slices, variant C, and the twenty prose/test sites.

## Context

`MAX_DEPTH` was documented as a bound on the *compiled* plan and enforced as a
bound on the *raw* walk, so two slots (the raw walk buffers and the compiled
`[Step; MAX_DEPTH]` array) were priced by one number. A path of 16 static edges
folds to one step yet was refused.

## Decision

**Separate the two bounds, and move both.**

1. **`MAX_PATH_EDGES = 64`** bounds the raw walk. It is checked on **`nt + ns`,
   inside the walk loop**, so that "64" means 64 edges walked and not 128.
2. **`MAX_DEPTH` goes 16 -> 32** and is the length of the compiled
   `[Step; MAX_DEPTH]` array, counted *after* folding.
3. **`fold` takes the two `u32` slices; the intermediate `[Step; MAX_DEPTH]`
   array is deleted.** It is bit-equal to the previous `fold`, not
   tolerance-equal, over shapes that include interleaved static/dynamic paths.
4. **The compiled bound is checked after the fold loop, and the fold loop does
   not return early** (variant C below).
5. **No new `LookupError` variant.**

### Sizing

**`MAX_PATH_EDGES = 64`**: raw corpus diameter 30 plus a `/tf` prefix (~33). The
bound sets the worst accepted compile (1.09 us at 64, 3.97 us at 256), and a failed
`Tree::plan` is never cached. **`MAX_DEPTH = 32`**: the corpus needs 28 folded
steps, plus two dynamic prefix edges.

### `TreeTooDeep` keeps its shape, and its message changes

`depth` stays a `u16`; a new variant would need a new `tft_status` in a frozen C
ABI. `depth` is the true raw edge count when the *raw* bound refuses and the true
folded step count when the *compiled* bound refuses, so the message must not name
a maximum. Per `API.md` R5 the core carries the identifier and **each binding's
prose layer carries the remedy its own callers can act on** (Python cannot declare
a static edge at all); the core's own rendering offers the one remedy every
binding has: re-parent so the two frames share a nearer ancestor.

## Rationale

### (A) Raise `MAX_DEPTH` — REVERSED, by measurement

Raising the constant costs only a `Tree::plan` cache miss, which D3 places off
the hot path, and stops queries `tf2` answers from being refused.

### (D) Fold during the walk — unnecessary, and would have been wrong

`compile` emits the source half in reverse of walk order, so an accumulator can
only produce the right-nested association. `Iso3` composition is not associative
under rounding and every existing test is tolerance-based (`TOL = 1e-12`), so
nothing in the suite would catch it.

### Where the compiled bound is checked — three variants, and taking step 3 literally panics

`fold` writes `out[n]` inside its loop, so checking "after fold" literally is out
of bounds when nothing folds.

| variant | shape | verdict |
|---|---|---|
| **A** | check inside the fold loop, return `TreeTooDeep` early | **loses.** A defect past the compiled bound returns `TreeTooDeep` instead of `UnknownEdge` / `MixedTimeDomains` |
| **B** | fold into a `[Step; MAX_PATH_EDGES]` scratch, check after | **loses on cost** (32 KiB of stack at a raw bound of 256) |
| **C** | output array stays `[Step; MAX_DEPTH]`; the loop never returns early | **chosen** |

**Variant C:** when `n >= MAX_DEPTH` the write is skipped, `n` keeps incrementing,
and every remaining edge is still resolved through `edge_meta`. The collapse
decision reads a tracked `last_static: bool` instead of `out[n - 1]`. After the
loop, `if n > MAX_DEPTH { return TreeTooDeep { depth: n } }`. The cost is only on
refused paths.

## Consequences

* `MAX_DEPTH` is `pub const`: a semver-relevant change; `docs/PHASE1.md` §7.1
  states the two bounds.
* **Memory:** `Plan` 2112 -> 4160 B, `Guard` 208 -> 336 B, plan cache 66.0 KiB per
  thread.
* **Error precedence moves:** `fold` runs before the compiled check, so a long
  path with an unknown edge or a domain clash returns `UnknownEdge` /
  `MixedTimeDomains`. **`MissingEdge` wins over `TreeTooDeep` by position:**
  `push_edge!` raises it inside the walk. The precedence table test in
  `crates/tf_tree_core/src/tests.rs` pins it, including the row where A and C
  disagree.

## Implementation plan

1. **`MAX_PATH_EDGES = 64` added, `MAX_DEPTH` raised to 32, and both documented**,
   including a new entry in `docs/API.md` §6's delta table.
2. **`fold` takes the two `u32` slices; the intermediate array is deleted.**
3. **The depth checks move**, variant C.
4. **Pin the precedence** with a table of `(shape, defect, expected error)` rows.
5. **The spec half:** `docs/PHASE1.md` §7.1, `docs/benchmarks/tf2.md`, `docs/API.md`.
