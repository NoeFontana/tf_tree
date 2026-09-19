//! The §3.7 attach handshake, as bytes.
//!
//! `docs/PHASE2.md` §3.7: two fixed-size messages over a `SOCK_SEQPACKET`
//! connection, the segment fd riding as `SCM_RIGHTS` on the response.
//! [`crate::server`] and [`crate::client`] are the transport.
//!
//! # No arena types
//!
//! `docs/PHASE2.md` §2 forbids depending on `tf_tree_arena`: the handshake takes
//! a [`SegmentDescriptor`] of plain integers plus a borrowed fd
//! (`docs/decisions/0005`).
//!
//! # Framing
//!
//! The byte layout is normative, hand-encoded and pinned by tests.
//! `from_bytes` rejects any datagram whose length is not exactly the struct size
//! before reading a field.

use crate::identity::AccessMode;

/// Magic prefixing both messages: `TF_TREE\0`.
pub const WIRE_MAGIC: [u8; 8] = *b"TF_TREE\0";

/// Encoded size of a [`HelloRequest`].
pub const HELLO_REQUEST_LEN: usize = 88;

/// Encoded size of a [`HelloResponse`].
pub const HELLO_RESPONSE_LEN: usize = 56;

/// Maximum usable length of a `sockaddr_un.sun_path`, NUL included.
pub const MAX_SOCKET_PATH: usize = 108;

/// Outcome of a handshake, as carried in [`HelloResponse::status`].
///
/// The discriminants are a wire contract, pinned by a test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HelloStatus {
    /// Accepted. The response carries a slot and the fd rides with it.
    Ok = 0,
    /// The client speaks a different `FORMAT_VERSION`.
    VersionMismatch = 1,
    /// Same version, different record layout (§3.7).
    LayoutMismatch = 2,
    /// The client's boot id differs from the owner's; see [`HelloResponse`].
    BootIdMismatch = 3,
    /// Every participant slot is taken.
    NoParticipantSlots = 4,
    /// The client asked for read-write on an arena it may not write.
    ModeNotPermitted = 5,
    /// Wrong length, wrong magic, or an unknown mode.
    Malformed = 6,
}

impl HelloStatus {
    /// The wire value.
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a wire value.
    ///
    /// An unknown code becomes [`HelloStatus::Malformed`].
    #[must_use]
    pub fn from_u32(v: u32) -> HelloStatus {
        match v {
            0 => HelloStatus::Ok,
            1 => HelloStatus::VersionMismatch,
            2 => HelloStatus::LayoutMismatch,
            3 => HelloStatus::BootIdMismatch,
            4 => HelloStatus::NoParticipantSlots,
            5 => HelloStatus::ModeNotPermitted,
            _ => HelloStatus::Malformed,
        }
    }
}

/// Why a datagram could not be decoded, as distinct from attach policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WireError {
    /// The datagram was not exactly the expected size.
    BadLength {
        /// Bytes received.
        got: usize,
        /// Bytes the message must be.
        expected: usize,
    },
    /// The first eight bytes were not [`WIRE_MAGIC`].
    BadMagic,
    /// The mode byte named neither `ReadOnly` nor `ReadWrite`.
    ///
    /// Strict, unlike [`crate::AccessMode`]'s lenient decode.
    BadMode {
        /// The byte received.
        got: u8,
    },
}

/// What the owner knows about its segment: integers only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentDescriptor {
    /// The arena's `FORMAT_VERSION`.
    pub format_version: u32,
    /// The arena's `layout_hash`.
    pub layout_hash: u32,
    /// Size of the segment in bytes; the client `fstat`s the fd against this.
    pub arena_size: u64,
    /// Identifies this segment as distinct from another with the same name.
    pub instance_uuid: [u8; 16],
    /// Boot id of the host that created the segment.
    ///
    /// Not encoded into [`HelloResponse`]; the server compares it with
    /// [`HelloRequest::client_boot_id`]. Every owner must populate it.
    pub boot_id: [u8; 16],
}

/// A client asking to attach (§3.7 step 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloRequest {
    /// The `FORMAT_VERSION` this client speaks.
    pub format_version: u32,
    /// The `layout_hash` this client computes.
    pub layout_hash: u32,
    /// Read-only or read-write.
    pub mode: AccessMode,
    /// The client's pid.
    pub client_pid: u32,
    /// The client's process start time, which makes the pid reuse-proof.
    pub client_start_time: u64,
    /// The client's boot id.
    pub client_boot_id: [u8; 16],
    /// The client's process name, NUL-padded. Diagnostics only.
    pub client_name: [u8; 32],
}

impl HelloRequest {
    /// Encode to the §3.7 byte layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; HELLO_REQUEST_LEN] {
        let mut out = [0u8; HELLO_REQUEST_LEN];
        out[0..8].copy_from_slice(&WIRE_MAGIC);
        out[8..12].copy_from_slice(&self.format_version.to_le_bytes());
        out[12..16].copy_from_slice(&self.layout_hash.to_le_bytes());
        out[16] = self.mode as u8;
        // 17..24 padding
        out[24..28].copy_from_slice(&self.client_pid.to_le_bytes());
        // 28..32 padding
        out[32..40].copy_from_slice(&self.client_start_time.to_le_bytes());
        out[40..56].copy_from_slice(&self.client_boot_id);
        out[56..88].copy_from_slice(&self.client_name);
        out
    }

    /// Decode a datagram. Checks length and magic only, so a version or layout
    /// disagreement still decodes (§3.7).
    ///
    /// # Errors
    ///
    /// [`WireError::BadLength`] or [`WireError::BadMagic`].
    pub fn from_bytes(raw: &[u8]) -> Result<HelloRequest, WireError> {
        let raw = check(raw, HELLO_REQUEST_LEN)?;
        Ok(HelloRequest {
            format_version: le32(raw, 8),
            layout_hash: le32(raw, 12),
            mode: AccessMode::try_from_byte(raw[16]).ok_or(WireError::BadMode { got: raw[16] })?,
            client_pid: le32(raw, 24),
            client_start_time: le64(raw, 32),
            client_boot_id: bytes16(raw, 40),
            client_name: bytes32(raw, 56),
        })
    }
}

/// The owner's answer (§3.7 step 3).
///
/// # What a rejection can report
///
/// Version and layout mismatches carry the owner's value. [`HelloStatus::BootIdMismatch`]
/// cannot: the response has no field for the owner's boot id ([`0055`] step 7).
/// `docs/RUNBOOK.md`'s `HandshakeRejected` row is the triage.
///
/// [`0055`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloResponse {
    /// Accepted, or why not.
    pub status: HelloStatus,
    /// The owner's `FORMAT_VERSION`.
    pub format_version: u32,
    /// The owner's `layout_hash`.
    pub layout_hash: u32,
    /// The slot the client must take, in the arena table and as its lock-file
    /// byte. Meaningless unless `status` is [`HelloStatus::Ok`].
    pub participant_slot: u32,
    /// Segment size, checked by the client against `fstat` (§3.7 step 4).
    pub arena_size: u64,
    /// Which segment this is.
    pub instance_uuid: [u8; 16],
    /// The owner's pid, for diagnostics.
    pub owner_pid: u32,
}

impl HelloResponse {
    /// An acceptance for `desc`, granting `slot`.
    #[must_use]
    pub fn accept(desc: &SegmentDescriptor, slot: u32, owner_pid: u32) -> HelloResponse {
        HelloResponse {
            status: HelloStatus::Ok,
            format_version: desc.format_version,
            layout_hash: desc.layout_hash,
            participant_slot: slot,
            arena_size: desc.arena_size,
            instance_uuid: desc.instance_uuid,
            owner_pid,
        }
    }

    /// A rejection carrying the owner's side of the comparison.
    #[must_use]
    pub fn reject(status: HelloStatus, desc: &SegmentDescriptor, owner_pid: u32) -> HelloResponse {
        HelloResponse {
            status,
            format_version: desc.format_version,
            layout_hash: desc.layout_hash,
            // `u32::MAX`: 0 is a real slot.
            participant_slot: u32::MAX,
            arena_size: desc.arena_size,
            instance_uuid: desc.instance_uuid,
            owner_pid,
        }
    }

    /// Encode to the §3.7 byte layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; HELLO_RESPONSE_LEN] {
        let mut out = [0u8; HELLO_RESPONSE_LEN];
        out[0..8].copy_from_slice(&WIRE_MAGIC);
        out[8..12].copy_from_slice(&self.status.as_u32().to_le_bytes());
        out[12..16].copy_from_slice(&self.format_version.to_le_bytes());
        out[16..20].copy_from_slice(&self.layout_hash.to_le_bytes());
        out[20..24].copy_from_slice(&self.participant_slot.to_le_bytes());
        out[24..32].copy_from_slice(&self.arena_size.to_le_bytes());
        out[32..48].copy_from_slice(&self.instance_uuid);
        out[48..52].copy_from_slice(&self.owner_pid.to_le_bytes());
        // 52..56 padding
        out
    }

    /// Decode a datagram. Length and magic only; see [`HelloRequest::from_bytes`].
    ///
    /// # Errors
    ///
    /// [`WireError::BadLength`] or [`WireError::BadMagic`].
    pub fn from_bytes(raw: &[u8]) -> Result<HelloResponse, WireError> {
        let raw = check(raw, HELLO_RESPONSE_LEN)?;
        Ok(HelloResponse {
            status: HelloStatus::from_u32(le32(raw, 8)),
            format_version: le32(raw, 12),
            layout_hash: le32(raw, 16),
            participant_slot: le32(raw, 20),
            arena_size: le64(raw, 24),
            instance_uuid: bytes16(raw, 32),
            owner_pid: le32(raw, 48),
        })
    }
}

/// Length then magic, before any field is read.
fn check(raw: &[u8], expected: usize) -> Result<&[u8], WireError> {
    if raw.len() != expected {
        return Err(WireError::BadLength {
            got: raw.len(),
            expected,
        });
    }
    if raw[0..8] != WIRE_MAGIC {
        return Err(WireError::BadMagic);
    }
    Ok(raw)
}

// Fixed-offset readers: sound because `check` guarantees the exact length and
// every `at` is a literal inside it.
fn le32(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

fn le64(raw: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&raw[at..at + 8]);
    u64::from_le_bytes(b)
}

fn bytes16(raw: &[u8], at: usize) -> [u8; 16] {
    let mut b = [0u8; 16];
    b.copy_from_slice(&raw[at..at + 16]);
    b
}

fn bytes32(raw: &[u8], at: usize) -> [u8; 32] {
    let mut b = [0u8; 32];
    b.copy_from_slice(&raw[at..at + 32]);
    b
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn desc() -> SegmentDescriptor {
        SegmentDescriptor {
            format_version: 2,
            layout_hash: 0x9075_90F5,
            arena_size: 1 << 20,
            instance_uuid: [0xAB; 16],
            boot_id: [0xCD; 16],
        }
    }

    fn request() -> HelloRequest {
        HelloRequest {
            format_version: 2,
            layout_hash: 0x9075_90F5,
            mode: AccessMode::ReadWrite,
            client_pid: 4242,
            client_start_time: 0x0102_0304_0506_0708,
            client_boot_id: [0xCD; 16],
            client_name: *b"consumer\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
        }
    }

    #[test]
    fn request_round_trips() {
        let r = request();
        assert_eq!(HelloRequest::from_bytes(&r.to_bytes()).unwrap(), r);
    }

    #[test]
    fn response_round_trips() {
        let r = HelloResponse::accept(&desc(), 3, 999);
        assert_eq!(HelloResponse::from_bytes(&r.to_bytes()).unwrap(), r);
        let j = HelloResponse::reject(HelloStatus::LayoutMismatch, &desc(), 999);
        assert_eq!(HelloResponse::from_bytes(&j.to_bytes()).unwrap(), j);
    }

    /// Pins the byte offsets; a round trip passes for any consistent encoding.
    #[test]
    fn the_byte_layout_is_pinned() {
        let raw = request().to_bytes();
        assert_eq!(raw.len(), 88);
        assert_eq!(&raw[0..8], b"TF_TREE\0");
        assert_eq!(le32(&raw, 8), 2, "format_version at 8");
        assert_eq!(le32(&raw, 12), 0x9075_90F5, "layout_hash at 12");
        assert_eq!(raw[16], 1, "mode at 16");
        assert_eq!(le32(&raw, 24), 4242, "client_pid at 24");
        assert_eq!(le64(&raw, 32), 0x0102_0304_0506_0708, "start_time at 32");
        assert_eq!(bytes16(&raw, 40), [0xCD; 16], "boot_id at 40");
        assert_eq!(&raw[56..88], &request().client_name[..], "name at 56");
        assert_eq!(&raw[17..24], &[0; 7], "padding 17..24");
        assert_eq!(&raw[28..32], &[0; 4], "padding 28..32");

        let raw = HelloResponse::accept(&desc(), 3, 999).to_bytes();
        assert_eq!(raw.len(), 56);
        assert_eq!(&raw[0..8], b"TF_TREE\0");
        assert_eq!(le32(&raw, 8), 0, "status at 8");
        assert_eq!(le32(&raw, 12), 2, "format_version at 12");
        assert_eq!(le32(&raw, 16), 0x9075_90F5, "layout_hash at 16");
        assert_eq!(le32(&raw, 20), 3, "participant_slot at 20");
        assert_eq!(le64(&raw, 24), 1 << 20, "arena_size at 24");
        assert_eq!(bytes16(&raw, 32), [0xAB; 16], "instance_uuid at 32");
        assert_eq!(le32(&raw, 48), 999, "owner_pid at 48");
        assert_eq!(&raw[52..56], &[0; 4], "padding 52..56");
    }

    /// Status codes are a wire contract.
    #[test]
    fn status_codes_are_pinned() {
        for (status, code) in [
            (HelloStatus::Ok, 0),
            (HelloStatus::VersionMismatch, 1),
            (HelloStatus::LayoutMismatch, 2),
            (HelloStatus::BootIdMismatch, 3),
            (HelloStatus::NoParticipantSlots, 4),
            (HelloStatus::ModeNotPermitted, 5),
            (HelloStatus::Malformed, 6),
        ] {
            assert_eq!(status.as_u32(), code, "{status:?}");
            assert_eq!(HelloStatus::from_u32(code), status, "code {code}");
        }
        assert_eq!(HelloStatus::from_u32(9999), HelloStatus::Malformed);
    }

    /// Length is checked before any field is read (§3.7 framing).
    #[test]
    fn a_wrong_length_datagram_is_rejected_before_parsing() {
        let full = request().to_bytes();
        for len in [0usize, 1, 8, 87] {
            assert_eq!(
                HelloRequest::from_bytes(&full[..len]),
                Err(WireError::BadLength {
                    got: len,
                    expected: 88
                }),
                "len {len}"
            );
        }
        let mut over = full.to_vec();
        over.push(0);
        assert_eq!(
            HelloRequest::from_bytes(&over),
            Err(WireError::BadLength {
                got: 89,
                expected: 88
            })
        );
    }

    #[test]
    fn a_foreign_datagram_is_rejected_on_magic() {
        let mut raw = request().to_bytes();
        raw[0] = b'X';
        assert_eq!(HelloRequest::from_bytes(&raw), Err(WireError::BadMagic));
    }

    /// An unknown mode byte is rejected, not downgraded to `ReadOnly`.
    #[test]
    fn an_unknown_mode_is_malformed_rather_than_downgraded() {
        let mut raw = request().to_bytes();
        raw[16] = 0xFF;
        assert_eq!(
            HelloRequest::from_bytes(&raw),
            Err(WireError::BadMode { got: 0xFF })
        );
    }

    /// A rejection carries the owner's values and grants no slot.
    #[test]
    fn a_rejection_names_the_owners_values() {
        let j = HelloResponse::reject(HelloStatus::VersionMismatch, &desc(), 7);
        assert_eq!(j.format_version, 2);
        assert_eq!(j.layout_hash, 0x9075_90F5);
        assert_eq!(j.instance_uuid, [0xAB; 16]);
        assert_eq!(j.participant_slot, u32::MAX);
    }
}
