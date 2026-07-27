//! A minimal CBOR (RFC 8949) **writer** for the `.tft` manifest.
//!
//! # Why hand-written, and why here
//!
//! `docs/PHASE5.md` §2.3 makes the manifest CBOR "because it is cold,
//! variable-length, and worth being able to inspect with a generic tool.
//! Everything hot is in the arena." The value of that choice is entirely
//! *external* — `cbor2`, `cbor-diag`, `jq` after a conversion — and none of it
//! requires a serialization framework on this side to encode nine fixed keys.
//!
//! It lives in the facade rather than in `tf_tree_arena` for two reasons. The
//! arena crate's dependency and *scope* budget is bytes and mappings; it takes
//! the manifest as an opaque `&[u8]` and never looks inside. And the manifest's
//! content — frame names, edge spans — comes from `tf_tree_core`'s read surface,
//! which is above the arena, so a codec down there could not build one anyway.
//!
//! # Writer only
//!
//! There is deliberately no decoder. Nothing in the read path parses a manifest
//! (§2.1: opening a `.tft` is an `mmap` and no parsing), and the one consumer
//! that will want to — an inspection tool — is better served by the generic CBOR
//! tooling that motivated the format choice than by a second hand-rolled parser
//! nobody stresses. The encoding is pinned against RFC 8949 Appendix A's own
//! test vectors, which is ground truth this file did not produce.

/// Encoder for the definite-length CBOR subset the manifest uses.
///
/// Structural calls ([`Writer::array`], [`Writer::map`]) announce a count and
/// the caller then writes exactly that many items (twice that, for a map's
/// key/value pairs). Nothing checks that — a checked builder would need to carry
/// a stack, and the single producer of these bytes is one screen away in
/// [`crate::frozen`].
#[derive(Default)]
pub(crate) struct Writer {
    out: Vec<u8>,
}

impl Writer {
    /// A fresh, empty encoder.
    pub(crate) fn new() -> Writer {
        Writer { out: Vec::new() }
    }

    /// The encoded bytes.
    pub(crate) fn finish(self) -> Vec<u8> {
        self.out
    }

    /// Write a major type and its argument in the shortest legal form.
    ///
    /// Shortest-form is not decoration: RFC 8949 §4.2's "preferred
    /// serialization" is what makes two encodings of the same manifest compare
    /// equal byte for byte, which is what lets a test assert that freezing the
    /// same arena twice produces the same file.
    fn head(&mut self, major: u8, arg: u64) {
        let m = major << 5;
        if arg < 24 {
            self.out.push(m | arg as u8);
        } else if arg <= u64::from(u8::MAX) {
            self.out.push(m | 24);
            self.out.push(arg as u8);
        } else if arg <= u64::from(u16::MAX) {
            self.out.push(m | 25);
            self.out.extend_from_slice(&(arg as u16).to_be_bytes());
        } else if arg <= u64::from(u32::MAX) {
            self.out.push(m | 26);
            self.out.extend_from_slice(&(arg as u32).to_be_bytes());
        } else {
            self.out.push(m | 27);
            self.out.extend_from_slice(&arg.to_be_bytes());
        }
    }

    /// An unsigned integer (major type 0).
    pub(crate) fn u64(&mut self, v: u64) {
        self.head(0, v);
    }

    /// A signed integer: major type 0 when non-negative, 1 otherwise.
    ///
    /// The negative encoding stores `-1 - v` as an unsigned argument, so
    /// `i64::MIN` still fits — `!(v as u64)` computes it via the two's-complement
    /// identity rather than negating in `i64`, where `-(i64::MIN)` would panic in
    /// a debug build and wrap in a release one.
    pub(crate) fn i64(&mut self, v: i64) {
        if v >= 0 {
            self.head(0, v as u64);
        } else {
            self.head(1, !(v as u64));
        }
    }

    /// A UTF-8 text string (major type 3).
    pub(crate) fn text(&mut self, s: &str) {
        self.head(3, s.len() as u64);
        self.out.extend_from_slice(s.as_bytes());
    }

    /// `null` (major type 7, simple value 22) — the one simple value the
    /// manifest needs, for an edge that has never published and therefore has no
    /// time span.
    ///
    /// Encoding `0` instead would be worse than absent: a reader cannot tell a
    /// stamp of zero from "no samples", and epoch-zero stamps are real.
    pub(crate) fn null(&mut self) {
        self.out.push(0xF6);
    }

    /// Open a definite-length array of `n` items.
    pub(crate) fn array(&mut self, n: usize) {
        self.head(4, n as u64);
    }

    /// Open a definite-length map of `n` key/value pairs.
    pub(crate) fn map(&mut self, n: usize) {
        self.head(5, n as u64);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        f(&mut w);
        w.finish()
    }

    /// RFC 8949 Appendix A's own vectors, which this file did not produce.
    ///
    /// The boundary cases are the point: 23/24, 255/256, 65535/65536 are exactly
    /// where `head` changes width, and each pair pins one branch. Mutant: change
    /// `arg < 24` to `arg <= 24` ⇒ `24` encodes as `0x1818`… no: as `0x18` with
    /// no payload, and the first `assert_eq!` for 24 fails.
    #[test]
    fn integers_match_the_rfc_vectors() {
        assert_eq!(enc(|w| w.u64(0)), [0x00]);
        assert_eq!(enc(|w| w.u64(23)), [0x17]);
        assert_eq!(enc(|w| w.u64(24)), [0x18, 0x18]);
        assert_eq!(enc(|w| w.u64(255)), [0x18, 0xff]);
        assert_eq!(enc(|w| w.u64(256)), [0x19, 0x01, 0x00]);
        assert_eq!(enc(|w| w.u64(65535)), [0x19, 0xff, 0xff]);
        assert_eq!(enc(|w| w.u64(65536)), [0x1a, 0x00, 0x01, 0x00, 0x00]);
        assert_eq!(
            enc(|w| w.u64(4_294_967_296)),
            [0x1b, 0, 0, 0, 1, 0, 0, 0, 0]
        );
        assert_eq!(
            enc(|w| w.u64(u64::MAX)),
            [0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
    }

    /// The negative branch, including the value that cannot be negated in `i64`.
    ///
    /// `i64::MIN` is not a corner nobody hits: `created_unix_ns` and every stamp
    /// in the manifest are `i64`, and a sentinel of `i64::MIN` is exactly what an
    /// uninitialised one looks like. Mutant: write `self.head(1, (-1 - v) as u64)`
    /// ⇒ the `i64::MIN` case panics on overflow in a debug build, which is what
    /// `cargo nextest` runs.
    #[test]
    fn negative_integers_match_the_rfc_vectors() {
        assert_eq!(enc(|w| w.i64(-1)), [0x20]);
        assert_eq!(enc(|w| w.i64(-24)), [0x37]);
        assert_eq!(enc(|w| w.i64(-25)), [0x38, 0x18]);
        assert_eq!(enc(|w| w.i64(-1000)), [0x39, 0x03, 0xe7]);
        assert_eq!(
            enc(|w| w.i64(i64::MIN)),
            [0x3b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        // And the non-negative branch of the same function, which must produce
        // major type 0 and not a positive-looking major type 1.
        assert_eq!(enc(|w| w.i64(0)), [0x00]);
        assert_eq!(enc(|w| w.i64(1)), [0x01]);
    }

    /// Strings and containers, again against the RFC's vectors.
    ///
    /// `text` takes a *byte* length, not a character count — the difference only
    /// shows up on non-ASCII, so `"ü"` is in here deliberately. Mutant: use
    /// `s.chars().count()` as the head argument ⇒ the last case emits a length of
    /// 1 for two bytes and the assertion fails.
    #[test]
    fn strings_and_containers_match_the_rfc_vectors() {
        assert_eq!(enc(|w| w.text("")), [0x60]);
        assert_eq!(enc(|w| w.text("a")), [0x61, 0x61]);
        assert_eq!(enc(|w| w.text("IETF")), [0x64, 0x49, 0x45, 0x54, 0x46]);
        assert_eq!(enc(|w| w.array(0)), [0x80]);
        assert_eq!(enc(|w| w.map(0)), [0xa0]);
        assert_eq!(enc(|w| w.null()), [0xf6]);
        // {"a": 1}
        assert_eq!(
            enc(|w| {
                w.map(1);
                w.text("a");
                w.u64(1);
            }),
            [0xa1, 0x61, 0x61, 0x01]
        );
        // "ü" is two bytes, one char.
        assert_eq!(enc(|w| w.text("ü")), [0x62, 0xc3, 0xbc]);
    }
}
