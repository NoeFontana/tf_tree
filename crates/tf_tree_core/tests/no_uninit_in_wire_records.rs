//! **Every byte of an arena record that reaches disk must be initialised.**
//!
//! `EdgeRecord` is `#[repr(C, align(64))]`, and `nominal_rate_mhz` ends at
//! offset 28 while `head: AtomicU64` must start at a multiple of 8 — so the
//! compiler inserts a 4-byte hole at `[28..32)`. A struct literal initialises
//! *fields*; it does not initialise padding. `TopologyBuilder::declare_edge`
//! then publishes the record with a **typed** write
//! (`ptr::write(base.add(off).cast::<EdgeRecord>(), record)`), which copies the
//! hole along with everything else, and `write_frozen` memcpys the whole arena
//! to a file.
//!
//! So four uninitialised bytes of producer memory landed in every `.tft`, once
//! per declared edge. Three things were wrong with that and only the first is
//! about bytes:
//!
//! 1. It is **Undefined Behaviour** — reading uninitialised memory as `u8`.
//!    Miri says so, and this test is the one that asks it.
//! 2. It is an **information leak** into a shipped artifact.
//! 3. It makes a `.tft` **not content-addressable**, because those bytes differ
//!    run to run. That is what the question "is freeze byte-reproducible?"
//!    turned out to be about.
//!
//! The fix names the bytes. It moves nothing: the hole was already reserved by
//! the compiler, so `size_of`, every field offset and `layout_hash` are
//! unchanged, and it is **not** an arena field in the sense `CLAUDE.md`
//! forbids adding.
//!
//! **This test fails under Miri without the fix**, which is the point of it —
//! `cargo +nightly miri test -p tf_tree_core --test no_uninit_in_wire_records`
//! reported
//! *"Undefined Behavior: reading memory at `alloc[0x1c..0x1d]`, but memory is
//! uninitialized"*, `0x1c` being byte 28.

use std::mem::{align_of, offset_of, size_of};
use tf_tree_core::edge::EdgeRecord;

/// Read a record as bytes exactly as `write_frozen` does.
fn as_wire_bytes(rec: &EdgeRecord) -> &[u8] {
    // SAFETY: `EdgeRecord` is `#[repr(C)]` and, with every hole named, has no
    // uninitialised bytes — which is the property under test. This mirrors
    // `write_frozen`'s `&[u8]` view of the arena.
    unsafe {
        core::slice::from_raw_parts(
            (rec as *const EdgeRecord).cast::<u8>(),
            size_of::<EdgeRecord>(),
        )
    }
}

#[inline(never)]
fn dirty_the_stack(fill: u8) -> [u8; 4096] {
    let mut buf = [fill; 4096];
    std::hint::black_box(&mut buf);
    buf
}

#[test]
fn the_hole_before_head_is_named_and_zero() {
    // The hole is where it always was; naming it must not have moved anything.
    assert_eq!(size_of::<EdgeRecord>(), 128, "EdgeRecord stride changed");
    assert_eq!(align_of::<EdgeRecord>(), 64);
    assert_eq!(offset_of!(EdgeRecord, nominal_rate_mhz), 24);
    assert_eq!(
        offset_of!(EdgeRecord, head),
        32,
        "head moved, so the padding this test is about is somewhere else now"
    );

    for fill in [0xAAu8, 0x55, 0xEE, 0x00] {
        let _dirt = std::hint::black_box(dirty_the_stack(fill));
        let dynamic = EdgeRecord::dynamic(1, 2, 64, 0, 0, 0, 0);
        let statik = EdgeRecord::static_edge(1, 2, [0; 7], 0);
        for (what, rec) in [("dynamic", &dynamic), ("static_edge", &statik)] {
            assert_eq!(
                &as_wire_bytes(rec)[28..32],
                &[0u8; 4],
                "{what}: the hole before `head` carries producer memory into \
                 every .tft (stack was filled 0x{fill:02X})"
            );
        }
    }
}

#[test]
fn every_wire_record_is_fully_initialised() {
    // The general property, asserted the way `write_frozen` would observe it:
    // a record built on a dirtied stack must be byte-identical to one built on
    // a clean one. Under Miri this additionally proves no byte is uninit.
    let _dirt = std::hint::black_box(dirty_the_stack(0xC3));
    let a = EdgeRecord::dynamic(7, 9, 128, 16, 32, 1, 2);
    let _clean = std::hint::black_box([0u8; 4096]);
    let b = EdgeRecord::dynamic(7, 9, 128, 16, 32, 1, 2);
    assert_eq!(
        as_wire_bytes(&a),
        as_wire_bytes(&b),
        "two identically-built EdgeRecords differ byte for byte, so the arena \
         is carrying producer state and a .tft cannot be content-addressed"
    );
}
