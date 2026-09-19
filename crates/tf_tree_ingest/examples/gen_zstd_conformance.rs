//! Regenerate `testdata/zstd_conformance.mcap`: an MCAP whose chunk payloads are
//! compressed by the real `zstd` CLI (libzstd), so `tests/ingest.rs`'s
//! `a_real_libzstd_recording_ingests` checks conformance and not just an
//! `ruzstd` round-trip.
//!
//! An example, not a test: it shells out to `zstd`, which no gate may depend on.
//! Run it only when `fixture::conformance_recording` changes:
//!
//! ```text
//! cargo run -p tf_tree_ingest --features fixture --example gen_zstd_conformance
//! ```
//!
//! The MCAP framing is ours: this walks `fixture::chunked_mcap_bytes`' records,
//! replaces each chunk's records field with `zstd`'s output and rewrites
//! `compression` and `compressed_size`. `uncompressed_size` and
//! `uncompressed_crc` are carried across untouched, so the chunk CRC checks
//! libzstd's output against our hash of its input.

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

    // No chunk means a fixture that conforms to nothing, invisibly.
    if chunks == 0 {
        return Err("the corpus produced no chunk records; nothing was compressed".into());
    }

    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/zstd_conformance.mcap");
    std::fs::write(&path, &out)?;
    // A hand-run generator must say what it wrote.
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
/// The first four header fields are copied as one slice, so `uncompressed_size`
/// and `uncompressed_crc` carry across by construction.
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

/// Pipe `bytes` through the host's `zstd -19` (more of the format than the default).
///
/// `--no-check` omits zstd's content checksum: `decompress` verifies none, and the
/// chunk CRC32 is the check that runs. The write runs on its own thread so
/// something always drains stdout; a single-threaded write deadlocks on large input.
fn zstd_compress(bytes: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut child = Command::new("zstd")
        .args(["-19", "--no-check", "-c", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run the `zstd` CLI, which this generator needs: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("the zstd child has no stdin")?;
    // Owned by the thread so the child sees EOF even if the write fails.
    let input = bytes.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let done = child.wait_with_output()?;
    // Joined after exit so an early-closing `zstd` reports its exit status, not `BrokenPipe`.
    let wrote = writer
        .join()
        .map_err(|_| "the stdin writer thread panicked")?;
    if !done.status.success() {
        return Err(format!("zstd exited with {}", done.status).into());
    }
    wrote?;
    Ok(done.stdout)
}

/// The `zstd` version string, recording which libzstd produced the bytes.
fn zstd_version() -> Result<String, Box<dyn std::error::Error>> {
    let out = Command::new("zstd").arg("--version").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
