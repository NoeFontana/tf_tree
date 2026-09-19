//! A minimal CBOR (RFC 8949) **writer** for the `.tft` manifest
//! (`docs/PHASE5.md` §2.3).
//!
//! Hand-written because the format's value is external (`cbor2`, `jq`); it lives
//! here because the arena crate takes the manifest as opaque bytes. There is no
//! decoder: opening a `.tft` parses nothing (§2.1). Encoding is pinned against
//! RFC 8949 Appendix A.

/// Encoder for the definite-length CBOR subset the manifest uses.
///
/// [`Writer::array`] / [`Writer::map`] announce a count and the caller writes
/// exactly that many items (twice for a map). Nothing checks it.
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

    /// Write a major type and its argument in the shortest legal form (RFC 8949
    /// §4.2), so freezing the same arena twice yields the same bytes.
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
    /// `!(v as u64)` is `-1 - v` without negating `i64::MIN`.
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

    /// `null` (major type 7, simple value 22): an edge that never published has
    /// no span, and `0` would be indistinguishable from a real epoch-zero stamp.
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

    /// RFC 8949 Appendix A's vectors; each boundary pair pins one `head` branch.
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

    /// The negative branch, including `i64::MIN`. Mutant: `head(1, (-v) as u64)`
    /// drops the `-1 -` offset and the first assertion fails.
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

    /// Strings and containers; `text` takes a *byte* length, hence `"ü"`.
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
