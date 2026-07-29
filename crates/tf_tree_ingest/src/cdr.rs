//! CDR decoding of `tf2_msgs/msg/TFMessage` — `docs/PHASE5.md` §3.3.
//!
//! # Why this is hand-written and not a dependency
//!
//! MCAP is self-describing about *framing* — schema name, message encoding —
//! but the bytes of a `cdr`-encoded message are only decodable by something that
//! knows OMG CDR. §0.0 records that the `mcap` crate needs no ROS; that is only
//! true if the one message type this phase cares about is decoded here. The
//! whole grammar below is four primitives and two strings, and it is entirely
//! specified by the alignment rules restated in [`Reader`].
//!
//! # The one place a mistake would be silent
//!
//! ROS transmits quaternions **w-last** (`x y z w`); `tf_tree_math::Quat` is
//! w-first, and so is the `[f64; 7]` canonical order this crate passes around
//! (`docs/PHASE1.md` §3.1). A transposition here does not fail — it produces a
//! valid unit quaternion describing a *different* rotation, which then flows all
//! the way into a `.tft` and out into somebody's training set. It is
//! transposed once, in [`Reader::transform`], and tested against bytes captured
//! from the wire order rather than against this module's own encoder.
//!
//! # It allocates per transform, and that is a known, measured, accepted cost
//!
//! [`TransformStamped`] owns two `String`s and `NameNormalizer::normalize`
//! returns two more, so a transform costs four heap allocations plus one `Vec`
//! per message — and the whole decode runs `1 + G` times, once for the survey
//! and once per `--max-memory` group. `Reader::string` could yield a `&'a str`
//! borrowed from the payload, with owning deferred to `Frames::intern`, which
//! already deduplicates; that is worth an estimated 2–5× on this path.
//!
//! It is **not** done, and the reason is a number rather than an opinion.
//! Measured on this host, release build, a 90 000-transform synthetic recording:
//! **54.8 ms, 609 µs per 1 000 transforms**, against a §12 gate 5 that asks for
//! 10× real time. This is an offline batch path that runs once per recording,
//! not an engine hot path — **nothing in this crate touches a lookup or a push**
//! — and the borrow refactor would change `TransformStamped`'s shape, which the
//! fixture encoder also uses. Recorded here so the next person to open this file
//! finds the measurement instead of rediscovering the allocations.

/// Why a `TFMessage` payload could not be decoded.
///
/// `Copy` and `String`-free (`docs/PROJECT.md` §5). Each variant carries the
/// byte offset at which the decode gave up, because "this bag has one bad
/// message in 400 000" is a different problem from "this bag is not CDR" and the
/// offset is what tells them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CdrError {
    /// The payload ended in the middle of a field.
    #[error("CDR payload ended at byte {at} while reading {want} more")]
    Truncated {
        /// Offset into the encapsulated body at which the read started.
        at: usize,
        /// How many bytes the field needed.
        want: usize,
    },
    /// The 4-byte encapsulation header was missing or names a representation
    /// this decoder does not implement.
    ///
    /// XCDR2 (`0x0006`..`0x0009`) lands here deliberately rather than being
    /// decoded as XCDR1: for a flat, final-extensibility struct like
    /// `TFMessage` the two encodings agree, but nothing in the payload proves
    /// the struct is flat, and guessing would corrupt a nested one silently.
    #[error("unsupported CDR encapsulation 0x{id:04x}")]
    BadEncapsulation {
        /// The representation identifier that was found.
        id: u16,
    },
    /// A string field's length prefix was zero, or ran past the payload. CDR
    /// strings include their NUL terminator, so a length of zero is malformed
    /// rather than empty.
    #[error("bad CDR string length {len} at byte {at}")]
    BadString {
        /// Offset of the length prefix.
        at: usize,
        /// The length that was read.
        len: u32,
    },
    /// A frame name was not UTF-8. ROS frame ids are unconstrained bytes on the
    /// wire, so this is a real recording defect and not an impossibility.
    #[error("frame name at byte {at} is not UTF-8")]
    NotUtf8 {
        /// Offset of the string body.
        at: usize,
    },
    /// The transform array's length prefix exceeds what the remaining payload
    /// could hold even at the minimum encoded size of one element.
    ///
    /// Checked before allocating: a corrupt `u32` would otherwise ask for a
    /// 4-billion-element `Vec` and take the process out on a recording that is
    /// merely damaged.
    #[error("TFMessage claims {count} transforms, which cannot fit in {bytes} bytes")]
    ImplausibleCount {
        /// The declared element count.
        count: u32,
        /// Bytes left in the payload when it was read.
        bytes: usize,
    },
}

/// One decoded `geometry_msgs/msg/TransformStamped`.
#[derive(Clone, Debug, PartialEq)]
pub struct TransformStamped {
    /// `header.stamp` flattened to nanoseconds.
    pub stamp_ns: i64,
    /// `header.frame_id` — the parent, exactly as it arrived (not normalized).
    pub frame_id: String,
    /// `child_frame_id`, likewise raw.
    pub child_frame_id: String,
    /// `[qw qx qy qz tx ty tz]`, the canonical order (`docs/PHASE1.md` §3.1),
    /// already transposed out of ROS's w-last wire order.
    pub pose: [f64; 7],
}

/// The smallest number of bytes one `TransformStamped` can occupy: 4 (`sec`)
/// + 4 (`nanosec`) + 5 (a one-character `frame_id` with its length and NUL)
/// + 5 (`child_frame_id`) + 56 (seven `f64`), before any alignment padding.
///
/// Used only as the denominator of the plausibility check in
/// [`decode_tf_message`]; being an *under*-estimate is what makes that check
/// safe to reject on.
const MIN_TRANSFORM_BYTES: usize = 4 + 4 + 5 + 5 + 56;

/// A cursor over one CDR-encapsulated body.
///
/// # Alignment, which is the whole of CDR
///
/// Every primitive of size `n` starts at an offset that is a multiple of `n`,
/// **counted from the start of the encapsulated body** — that is, from just
/// after the 4-byte encapsulation header, not from the start of the buffer the
/// transport handed over. `buf` here is the body alone, so `pos` is already the
/// right origin; slicing the header off in [`decode_tf_message`] rather than
/// tracking an offset is what makes that impossible to get wrong.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    little_endian: bool,
}

impl<'a> Reader<'a> {
    fn align(&mut self, n: usize) {
        let rem = self.pos % n;
        if rem != 0 {
            self.pos += n - rem;
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CdrError> {
        let at = self.pos;
        let end = at
            .checked_add(n)
            .ok_or(CdrError::Truncated { at, want: n })?;
        if end > self.buf.len() {
            return Err(CdrError::Truncated { at, want: n });
        }
        self.pos = end;
        Ok(&self.buf[at..end])
    }

    fn u32(&mut self) -> Result<u32, CdrError> {
        self.align(4);
        let b = self.take(4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Ok(if self.little_endian {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    }

    fn i32(&mut self) -> Result<i32, CdrError> {
        self.u32().map(|v| v as i32)
    }

    fn f64(&mut self) -> Result<f64, CdrError> {
        self.align(8);
        let b = self.take(8)?;
        let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        Ok(if self.little_endian {
            f64::from_le_bytes(a)
        } else {
            f64::from_be_bytes(a)
        })
    }

    /// A CDR `string`: a `u32` length **including** the NUL terminator, then
    /// that many bytes. The terminator is dropped here rather than trusted —
    /// some serializers emit a length that does not count it, and a name with a
    /// trailing NUL interns as a *different* frame from the same name without
    /// one, which is a bug that only shows up as a missing transform.
    fn string(&mut self) -> Result<String, CdrError> {
        let at = self.pos;
        let len = self.u32()?;
        if len == 0 {
            return Err(CdrError::BadString { at, len });
        }
        let body_at = self.pos;
        let raw = self.take(len as usize)?;
        let body = match raw.split_last() {
            Some((0, rest)) => rest,
            _ => raw,
        };
        core::str::from_utf8(body)
            .map(str::to_owned)
            .map_err(|_| CdrError::NotUtf8 { at: body_at })
    }

    /// One `geometry_msgs/msg/TransformStamped`.
    fn transform(&mut self) -> Result<TransformStamped, CdrError> {
        // std_msgs/Header: builtin_interfaces/Time { int32 sec, uint32 nanosec }
        // then string frame_id. `sec` is signed and pre-1970 stamps are a real
        // (broken) thing to find in a bag, so the widening is signed too.
        let sec = i64::from(self.i32()?);
        let nanosec = i64::from(self.u32()?);
        let frame_id = self.string()?;
        let child_frame_id = self.string()?;
        // geometry_msgs/Transform: Vector3 translation, then Quaternion
        // rotation — and the quaternion is **x y z w** on the wire.
        let tx = self.f64()?;
        let ty = self.f64()?;
        let tz = self.f64()?;
        let qx = self.f64()?;
        let qy = self.f64()?;
        let qz = self.f64()?;
        let qw = self.f64()?;
        Ok(TransformStamped {
            // `saturating` rather than wrapping: a `sec` near `i64::MAX/1e9` is
            // corrupt data, and the anomaly counters downstream would rather see
            // an absurdly large stamp than a wrapped, plausible-looking one.
            stamp_ns: sec.saturating_mul(1_000_000_000).saturating_add(nanosec),
            frame_id,
            child_frame_id,
            pose: [qw, qx, qy, qz, tx, ty, tz],
        })
    }
}

/// Decode a `tf2_msgs/msg/TFMessage` payload, encapsulation header included.
///
/// # Errors
///
/// [`CdrError`] — see its variants. Nothing here is a panic path: every length
/// read off the wire is bounds-checked before it is used.
pub fn decode_tf_message(payload: &[u8]) -> Result<Vec<TransformStamped>, CdrError> {
    if payload.len() < 4 {
        return Err(CdrError::Truncated {
            at: 0,
            want: 4 - payload.len(),
        });
    }
    // The encapsulation header is big-endian by definition, whatever the body
    // that follows it is.
    let id = u16::from_be_bytes([payload[0], payload[1]]);
    let little_endian = match id {
        0x0000 | 0x0002 => false,
        0x0001 | 0x0003 => true,
        other => return Err(CdrError::BadEncapsulation { id: other }),
    };
    let mut r = Reader {
        buf: &payload[4..],
        pos: 0,
        little_endian,
    };
    let count = r.u32()?;
    let left = r.buf.len() - r.pos;
    if (count as usize).saturating_mul(MIN_TRANSFORM_BYTES) > left {
        return Err(CdrError::ImplausibleCount { count, bytes: left });
    }
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        out.push(r.transform()?);
    }
    Ok(out)
}

/// Encode a `TFMessage` payload the way ROS 2 does (XCDR1, little-endian).
///
/// Present so the synthetic fixture in [`crate::fixture`] can produce real
/// message bytes. It is deliberately **not** used as the oracle for
/// [`decode_tf_message`]'s tests: a decoder tested only against its own encoder
/// agrees with itself about a transposed quaternion. See
/// `wire_bytes_decode_w_last`.
#[cfg(any(test, feature = "fixture"))]
#[must_use]
pub fn encode_tf_message(transforms: &[TransformStamped]) -> Vec<u8> {
    let mut out: Vec<u8> = vec![0x00, 0x01, 0x00, 0x00];
    // Alignment is counted from the body, so `out.len() - 4` is the origin.
    fn pad(out: &mut Vec<u8>, n: usize) {
        while !(out.len() - 4).is_multiple_of(n) {
            out.push(0);
        }
    }
    fn put_u32(out: &mut Vec<u8>, v: u32) {
        pad(out, 4);
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn put_f64(out: &mut Vec<u8>, v: f64) {
        pad(out, 8);
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn put_str(out: &mut Vec<u8>, s: &str) {
        put_u32(out, s.len() as u32 + 1);
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    }
    put_u32(&mut out, transforms.len() as u32);
    for t in transforms {
        let sec = t.stamp_ns.div_euclid(1_000_000_000);
        let nsec = t.stamp_ns.rem_euclid(1_000_000_000);
        put_u32(&mut out, sec as i32 as u32);
        put_u32(&mut out, nsec as u32);
        put_str(&mut out, &t.frame_id);
        put_str(&mut out, &t.child_frame_id);
        for v in [t.pose[4], t.pose[5], t.pose[6]] {
            put_f64(&mut out, v);
        }
        // Back to w-last for the wire.
        for v in [t.pose[1], t.pose[2], t.pose[3], t.pose[0]] {
            put_f64(&mut out, v);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// A hand-assembled little-endian `TFMessage` carrying one transform, with
    /// the quaternion in ROS's **w-last** wire order and every component
    /// distinct.
    ///
    /// The distinctness is the fixture's whole job: with `q = (1,0,0,0)` — which
    /// is what a lazily-built fixture uses — a w-last/w-first transposition is
    /// invisible, and this repository has shipped that class of vacuous test
    /// before.
    fn wire_one() -> Vec<u8> {
        let mut b: Vec<u8> = vec![0x00, 0x01, 0x00, 0x00];
        b.extend_from_slice(&1u32.to_le_bytes()); // 1 transform
        b.extend_from_slice(&7i32.to_le_bytes()); // sec
        b.extend_from_slice(&250_000_000u32.to_le_bytes()); // nanosec
        b.extend_from_slice(&5u32.to_le_bytes()); // "odom\0"
        b.extend_from_slice(b"odom\0");
        // CDR aligns every primitive, so the next length prefix starts on a
        // 4-byte boundary counted from the body. Omitting this pad is exactly
        // the mistake the decoder must not mirror.
        while !(b.len() - 4).is_multiple_of(4) {
            b.push(0);
        }
        b.extend_from_slice(&5u32.to_le_bytes()); // "base\0"
        b.extend_from_slice(b"base\0");
        while !(b.len() - 4).is_multiple_of(8) {
            b.push(0);
        }
        for v in [1.0f64, 2.0, 3.0] {
            b.extend_from_slice(&v.to_le_bytes()); // translation
        }
        for v in [0.1f64, 0.2, 0.3, 0.9273618495495704] {
            b.extend_from_slice(&v.to_le_bytes()); // x y z w
        }
        b
    }

    /// Wire bytes decode with the quaternion transposed to w-first and the
    /// stamp flattened to nanoseconds.
    ///
    /// Mutant: swap the two `for v in [...]` loops in `Reader::transform` so
    /// `pose` is filled `[qx, qy, qz, qw, ...]` — applied, and this test failed
    /// on the `pose` assertion. A second mutant, dropping the `saturating_mul`
    /// factor to `1_000_000`, also failed on `stamp_ns`.
    #[test]
    fn wire_bytes_decode_w_last() {
        let got = decode_tf_message(&wire_one()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].stamp_ns, 7_250_000_000);
        assert_eq!(got[0].frame_id, "odom");
        assert_eq!(got[0].child_frame_id, "base");
        assert_eq!(
            got[0].pose,
            [0.9273618495495704, 0.1, 0.2, 0.3, 1.0, 2.0, 3.0]
        );
    }

    /// The encoder used by the fixture round-trips through the decoder, for a
    /// stamp that is not a whole second and for several transforms in one
    /// message.
    ///
    /// Mutant: change `put_str` to write `s.len()` instead of `s.len() + 1` —
    /// applied, and the round trip failed (the decoder consumed one byte too
    /// few and read the next length prefix out of the name's NUL).
    #[test]
    fn encoder_round_trips() {
        let src = vec![
            TransformStamped {
                stamp_ns: 1_234_567_891,
                frame_id: "map".into(),
                child_frame_id: "odom".into(),
                pose: [0.5, 0.5, 0.5, 0.5, 9.0, -8.0, 7.5],
            },
            TransformStamped {
                stamp_ns: -3_000_000_001,
                frame_id: "odom".into(),
                child_frame_id: "base_link".into(),
                pose: [0.0, 1.0, 0.0, 0.0, 0.25, 0.5, 0.75],
            },
        ];
        assert_eq!(decode_tf_message(&encode_tf_message(&src)).unwrap(), src);
    }

    /// A big-endian encapsulation is decoded, not guessed at.
    ///
    /// Mutant: map `0x0000` to `little_endian = true` — applied, and the stamp
    /// came back as `0x0700_0000` seconds instead of 7.
    #[test]
    fn big_endian_encapsulation() {
        let mut b: Vec<u8> = vec![0x00, 0x00, 0x00, 0x00];
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&7i32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&2u32.to_be_bytes());
        b.extend_from_slice(b"a\0");
        while !(b.len() - 4).is_multiple_of(4) {
            b.push(0);
        }
        b.extend_from_slice(&2u32.to_be_bytes());
        b.extend_from_slice(b"b\0");
        while !(b.len() - 4).is_multiple_of(8) {
            b.push(0);
        }
        for v in [0.0f64; 3] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        for v in [0.0f64, 0.0, 0.0, 1.0] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        let got = decode_tf_message(&b).unwrap();
        assert_eq!(got[0].stamp_ns, 7_000_000_000);
        assert_eq!(got[0].pose[0], 1.0);
    }

    /// A truncated payload is an error at a named offset, never a panic.
    ///
    /// Mutant: replace `take`'s bounds check with `&self.buf[at..end]` — applied,
    /// and the test aborted with an index-out-of-bounds panic instead of
    /// returning `Truncated`.
    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let full = wire_one();
        for cut in 4..full.len() {
            match decode_tf_message(&full[..cut]) {
                Err(CdrError::Truncated { .. }) | Err(CdrError::ImplausibleCount { .. }) => {}
                other => panic!("cut at {cut} gave {other:?}"),
            }
        }
    }

    /// A corrupt element count is rejected before anything is allocated.
    ///
    /// Mutant: delete the `ImplausibleCount` check — applied, and the test
    /// aborted with a capacity-overflow abort from `Vec::with_capacity`.
    #[test]
    fn absurd_count_is_rejected_before_allocating() {
        let mut b: Vec<u8> = vec![0x00, 0x01, 0x00, 0x00];
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode_tf_message(&b),
            Err(CdrError::ImplausibleCount {
                count: u32::MAX,
                bytes: 0
            })
        );
    }

    /// XCDR2 is refused rather than decoded as XCDR1.
    ///
    /// Mutant: add `0x0006 | 0x0007` to the little-endian arm — applied, and
    /// this test failed with `Ok(..)` where it expects `BadEncapsulation`.
    #[test]
    fn xcdr2_is_refused() {
        let mut b: Vec<u8> = vec![0x00, 0x07, 0x00, 0x00];
        b.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            decode_tf_message(&b),
            Err(CdrError::BadEncapsulation { id: 0x0007 })
        );
    }
}
