#![no_std]
//! `no_std + alloc` single-process transform tree engine.
//!
//! This is the source of truth: frame interning, topology, edge records, the
//! seqlock sample buffers, plan compilation, and typed errors. It is the
//! shared-memory design (Phase 1) backed by a heap allocation — every layout
//! decision exists so Phase 2's `MappedArena` is a one-line swap.
//!
//! # Load-bearing invariants
//!
//! 1. **Append-only identity.** `FrameId`/`EdgeId` are never reused; removal is
//!    tombstoning. A stale `Plan` may index a valid record but never go out of
//!    bounds.
//! 2. **No pointers in the arena.** Every intra-arena reference is a `u32`.
//! 3. **Fixed capacity.** Set at construction; ring capacities are powers of two.
//! 4. **Single writer per edge.** Enforced by the claim table.
//! 5. **Monotone head.** Never masked in storage, only at access.
//! 6. **Stamps non-decreasing per edge.**
//! 7. **Little-endian arena fields** (asserted at construction).
//! 8. **Every heap allocation happens at construction.**
//!
//! # Unsafe
//!
//! `unsafe` is confined to two modules (`buffer`, `arena_view`), each carrying a
//! module-level `// SAFETY:` block. Everything else is safe.

extern crate alloc;

// Modules are added by the Phase 1 `tf_tree_core` implementation PRs:
//   frame, topology, edge, buffer, arena_view, plan, sample, error.
