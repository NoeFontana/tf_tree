//! Split the C ABI's measured +52% into the two things that changed at once.
//!
//! `docs/benchmarks/tf2.md` reports a C++ caller against `libtf_tree_c.so` at
//! **306.7 ns** where native Rust measures **201.5 ns**, and records that the
//! run changed two variables together: the call crosses a shared-library
//! boundary, *and* the arena is a `MAP_SHARED` `memfd` rather than a private
//! heap allocation. It calls separating them owed.
//!
//! [`tf_tree_bench::backing`] is the missing middle arm — the same native Rust
//! API and the same loop, on the same `memfd` backing the C++ side reads — so
//! the difference it reports is the mapping alone, and whatever the C++ arm
//! costs beyond it is the boundary.
//!
//! Run it with `just abi-split`, which prints both halves.

#![allow(clippy::print_stdout)]

use anyhow::Result;

/// The C++ arm from `docker/tf2/native_ratio.sh`, for the subtraction.
///
/// A literal, and that is a real weakness rather than a shortcut: it is a
/// figure from a different run on a different day, so the arithmetic below is a
/// decomposition of *recorded* numbers and not a paired measurement of the
/// boundary. The mapping half — the half this binary actually measures — is
/// paired. The line printed under the split says so, because a reader who
/// takes the second row for a measured one would be over-trusting it.
const CPP_ABI_NS: f64 = 306.7;

fn main() -> Result<()> {
    let run = tf_tree_bench::backing::measure()?;
    println!("{}", run.verdict_line());

    // **The bound, not the point estimate, is what the split rests on.** The
    // sign of a ~2 ns effect measured on this host resolves in some runs and not
    // in others; the band's upper edge barely moves. Since the residue being
    // attributed is ~100 ns, "the mapping accounts for at most this" settles the
    // question either way, and it settles it without needing a quiet host.
    let bound = run.backing_ns_bound();
    let total = CPP_ABI_NS - run.heap_ns;
    let boundary_min = total - bound;
    println!();
    println!("splitting the C ABI's +{total:.1} ns over native Rust:");
    match run.backing_ns() {
        Some(b) => println!(
            "  arena backing (heap -> memfd)   {b:+7.1} ns   measured, paired over {} rounds; \
             at most {bound:.1} ns across the band",
            run.rounds
        ),
        None => println!(
            "  arena backing (heap -> memfd)   <={bound:6.1} ns   sign unresolved over {} rounds \
             (the band contains 1.0), so this is the bound and not a point estimate",
            run.rounds
        ),
    }
    println!(
        "  library boundary (this -> C++)  >={boundary_min:6.1} ns   the residue, against \
         {CPP_ABI_NS:.1} ns recorded in docs/benchmarks/tf2.md"
    );
    println!();
    println!(
        "  {:.0}% of the gap is the boundary at minimum. The backing row is paired; the \
         boundary row is not — it subtracts a figure from another run on another day, so it \
         is an attribution rather than a measurement.",
        100.0 * boundary_min / total
    );
    Ok(())
}
