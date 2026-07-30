//! Regenerate `testdata/zstd_conformance.mcap`: an MCAP whose chunk payloads are
//! compressed by the **real** `zstd` CLI, i.e. by libzstd.
//!
//! # Why this exists at all
//!
//! Every other compressed fixture in this crate is encoded by `ruzstd` and decoded
//! by `ruzstd`. That proves round-trip and **not** conformance: an encoder and a
//! decoder from the same crate can agree with each other and both disagree with the
//! zstd that `rosbag2` links. So one fixture is compressed by libzstd and committed,
//! and `tests/ingest.rs`'s `a_real_libzstd_recording_ingests` reads it.
//!
//! # Why it is an example and not a test
//!
//! It shells out to `zstd`, which is a build-host assumption no gate may depend on
//! — the committed output is what the gate reads. Run it only when the corpus in
//! `fixture::conformance_recording` changes:
//!
//! ```text
//! cargo run -p tf_tree_ingest --features fixture --example gen_zstd_conformance
//! ```
//!
//! # What is ours and what is libzstd's
//!
//! The MCAP framing is entirely ours: `fixture::chunked_mcap_bytes` writes an
//! uncompressed hand-rolled file, and this program then walks its nine-byte record
//! framing, replaces each chunk's records field with `zstd`'s output, and rewrites
//! that chunk's `compression` and `compressed_size`. `uncompressed_size` and
//! `uncompressed_crc` are carried across **untouched**, which is what the MCAP
//! specification says they cover — so the committed file's CRC is a check on
//! libzstd's output against our hash of its input.
//!
//! The rewrite is done here rather than by teaching `fixture` to invoke a
//! subprocess, because a fixture writer that can shell out is a fixture writer a
//! test will eventually shell out from.

use std::io::Write;
use std::process::{Command, Stdio};

use tf_tree_ingest::fixture::{
    chunked_mcap_bytes, conformance_recording, ChunkedSpec, CONFORMANCE_MESSAGES_PER_CHUNK,
};

/// MCAP's `Chunk` opcode.
const OP_CHUNK: u8 = 0x06;
/// A record's framing: `opcode: u8` then `len: u64` little-endian.
const RECORD_HEADER: usize = 1 + 8;
/// Bytes of a chunk body before the `compression` string's length prefix:
/// `message_start_time`, `message_end_time`, `uncompressed_size`,
/// `uncompressed_crc`.
const CHUNK_FIXED: usize = 8 + 8 + 8 + 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let messages = conformance_recording();
    let plain = chunked_mcap_bytes(&messages, ChunkedSpec::new(CONFORMANCE_MESSAGES_PER_CHUNK))?;

    let magic = mcap::MAGIC.len();
    let mut out = Vec::with_capacity(plain.len());
    out.extend_from_slice(&plain[..magic]);

    let inner = &plain[magic..plain.len() - magic];
    let mut at = 0usize;
    let mut chunks = 0usize;
    while at < inner.len() {
        let opcode = inner[at];
        let len_bytes: [u8; 8] = inner[at + 1..at + RECORD_HEADER].try_into()?;
        let len = usize::try_from(u64::from_le_bytes(len_bytes))?;
        let body = &inner[at + RECORD_HEADER..at + RECORD_HEADER + len];
        let body = if opcode == OP_CHUNK {
            chunks += 1;
            recompress_chunk(body)?
        } else {
            body.to_vec()
        };
        out.push(opcode);
        out.extend_from_slice(&(body.len() as u64).to_le_bytes());
        out.extend_from_slice(&body);
        at += RECORD_HEADER + len;
    }
    out.extend_from_slice(&plain[plain.len() - magic..]);

    // A file with no chunk to compress would be a conformance fixture that
    // conforms to nothing, and the failure would be invisible in the committed
    // bytes.
    if chunks == 0 {
        return Err("the corpus produced no chunk records; nothing was compressed".into());
    }

    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/zstd_conformance.mcap");
    std::fs::write(&path, &out)?;
    // `print_stdout` is a workspace lint at `warn`, and a generator whose whole
    // purpose is to be run by hand has to say what it wrote and with what.
    #[allow(clippy::print_stdout)]
    {
        println!(
            "wrote {} ({} B, {chunks} chunks, from {} uncompressed B) using {}",
            path.display(),
            out.len(),
            plain.len(),
            zstd_version()?
        );
    }
    Ok(())
}

/// Replace one chunk body's records field with libzstd's compression of it.
///
/// The header's first four fields are copied as one slice rather than re-emitted
/// field by field, so the two that must **not** change — `uncompressed_size` and
/// `uncompressed_crc` — are carried across by construction rather than by care.
fn recompress_chunk(body: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let name_len = usize::try_from(u32::from_le_bytes(
        body[CHUNK_FIXED..CHUNK_FIXED + 4].try_into()?,
    ))?;
    if name_len != 0 {
        return Err(
            "the source chunk is already compressed; this program expects a plain one".into(),
        );
    }
    let size_at = CHUNK_FIXED + 4 + name_len;
    let records = &body[size_at + 8..];
    let packed = zstd_compress(records)?;

    let mut out = Vec::with_capacity(body.len());
    out.extend_from_slice(&body[..CHUNK_FIXED]);
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(b"zstd");
    out.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    out.extend_from_slice(&packed);
    Ok(out)
}

/// Pipe `bytes` through the host's `zstd` CLI.
///
/// Level 19 rather than the default 3: the corpus is a couple of kilobytes, so the
/// cost is nothing and a higher level exercises more of the format — longer matches
/// and larger Huffman tables — which is the point of testing against libzstd rather
/// than against an encoder whose decoder we already read.
///
/// `--no-check` omits zstd's own content checksum. Deliberate, and it makes the
/// fixture *stronger*: nothing in `decompress` verifies that checksum (ruzstd
/// computes one and compares nothing), so leaving it in would invite a reader to
/// believe it is what validates the frame. The chunk CRC32 is the check that runs.
fn zstd_compress(bytes: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut child = Command::new("zstd")
        .args(["-19", "--no-check", "-c", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run the `zstd` CLI, which this generator needs: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("the zstd child has no stdin")?
        .write_all(bytes)?;
    let done = child.wait_with_output()?;
    if !done.status.success() {
        return Err(format!("zstd exited with {}", done.status).into());
    }
    Ok(done.stdout)
}

/// The `zstd` version string, so a regeneration records which libzstd produced the
/// bytes rather than leaving `ATTRIBUTION.md` to be trusted.
fn zstd_version() -> Result<String, Box<dyn std::error::Error>> {
    let out = Command::new("zstd").arg("--version").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
