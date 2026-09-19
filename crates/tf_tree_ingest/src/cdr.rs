//! CDR decoding of `tf2_msgs/msg/TFMessage` — `docs/PHASE5.md` §3.3.
//!
//! ROS transmits quaternions **w-last**; the canonical `[f64; 7]` order is
//! w-first (`docs/PHASE1.md` §3.1). The transposition happens once, in
//! `Reader::transform`, and is tested by `wire_bytes_decode_w_last`.

/// Why a `TFMessage` payload could not be decoded; `Copy` and `String`-free (`docs/PROJECT.md` §5).
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
    /// The encapsulation header was missing or unimplemented (XCDR2 lands here).
    #[error("unsupported CDR encapsulation 0x{id:04x}")]
    BadEncapsulation {
        /// The representation identifier that was found.
        id: u16,
    },
    /// A string length prefix was zero or ran past the payload.
    #[error("bad CDR string length {len} at byte {at}")]
    BadString {
        /// Offset of the length prefix.
        at: usize,
        /// The length that was read.
        len: u32,
    },
    /// A frame name was not UTF-8.
    #[error("frame name at byte {at} is not UTF-8")]
    NotUtf8 {
        /// Offset of the string body.
        at: usize,
    },
    /// The array length prefix exceeds what the payload could hold; checked before allocating.
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
    /// `[qw qx qy qz tx ty tz]` (`docs/PHASE1.md` §3.1), already out of ROS's w-last order.
    pub pose: [f64; 7],
}

/// A lower bound on one encoded `TransformStamped`, for the plausibility check.
const MIN_TRANSFORM_BYTES: usize = 4 + 4 + 5 + 5 + 56;

/// A cursor over one CDR body (after the 4-byte header); primitives align to their size.
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

    /// A CDR `string`: `u32` length including the NUL, then the bytes. A missing NUL is tolerated.
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

    fn transform(&mut self) -> Result<TransformStamped, CdrError> {
        let sec = i64::from(self.i32()?);
        let nanosec = i64::from(self.u32()?);
        let frame_id = self.string()?;
        let child_frame_id = self.string()?;
        let tx = self.f64()?;
        let ty = self.f64()?;
        let tz = self.f64()?;
        let qx = self.f64()?;
        let qy = self.f64()?;
        let qz = self.f64()?;
        let qw = self.f64()?;
        Ok(TransformStamped {
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
/// [`CdrError`]; every wire length is bounds-checked, so nothing panics.
pub fn decode_tf_message(payload: &[u8]) -> Result<Vec<TransformStamped>, CdrError> {
    if payload.len() < 4 {
        return Err(CdrError::Truncated {
            at: 0,
            want: 4 - payload.len(),
        });
    }
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

/// Encode a `TFMessage` payload the way ROS 2 does (XCDR1, little-endian), for [`crate::fixture`].
#[cfg(any(test, feature = "fixture"))]
#[must_use]
pub fn encode_tf_message(transforms: &[TransformStamped]) -> Vec<u8> {
    let mut out: Vec<u8> = vec![0x00, 0x01, 0x00, 0x00];
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

    /// A little-endian one-transform `TFMessage` with distinct quaternion components.
    fn wire_one() -> Vec<u8> {
        let mut b: Vec<u8> = vec![0x00, 0x01, 0x00, 0x00];
        b.extend_from_slice(&1u32.to_le_bytes()); // 1 transform
        b.extend_from_slice(&7i32.to_le_bytes()); // sec
        b.extend_from_slice(&250_000_000u32.to_le_bytes()); // nanosec
        b.extend_from_slice(&5u32.to_le_bytes()); // "odom\0"
        b.extend_from_slice(b"odom\0");
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

    #[test]
    fn xcdr2_is_refused() {
        let mut b: Vec<u8> = vec![0x00, 0x07, 0x00, 0x00];
        b.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            decode_tf_message(&b),
            Err(CdrError::BadEncapsulation { id: 0x0007 })
        );
    }

    /// A malformed frame name is a named error at a named offset (`wire_one`'s
    /// first length prefix is at body offset 12).
    #[test]
    fn a_malformed_frame_name_is_a_named_error_not_a_guess() {
        let mut b = wire_one();
        b[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            decode_tf_message(&b),
            Err(CdrError::BadString { at: 12, len: 0 })
        );

        let mut b = wire_one();
        b[20] = 0xFF;
        assert_eq!(decode_tf_message(&b), Err(CdrError::NotUtf8 { at: 16 }));

        let mut b = wire_one();
        b[16..20].copy_from_slice(&4096u32.to_le_bytes());
        assert_eq!(
            decode_tf_message(&b),
            Err(CdrError::Truncated { at: 16, want: 4096 }),
            "a length past the payload must be refused, not sliced"
        );
    }
}
