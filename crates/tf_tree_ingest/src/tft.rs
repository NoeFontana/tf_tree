//! Recording → `.tft`, the shape §3 and §2 were designed to meet in.
//!
//! §2's frozen container already exists and `Tree::freeze_to` already writes it.
//! What this module adds is the two things a *bag*-sourced `.tft` has that a
//! `--from-live` one cannot: a real `source_digest`, and a report.
//!
//! # `source_digest` is what makes a `.tft` traceable
//!
//! §2.3: *"`source_digest` makes a `.tft` traceable to the recording it came
//! from, which matters the first time a training result cannot be reproduced."*
//! It is BLAKE3 of the recording's bytes — the file, not the transforms — so it
//! answers "was this index built from *that* file" without needing to re-ingest.
//! `--from-live` writes all-zero here because a live arena has no recording; a
//! bag ingest has no excuse to.
//!
//! The digest is computed in a streaming pass over the file rather than by
//! reading it in, for the same reason the reader streams: the recording is
//! allowed to be larger than memory.

use std::path::Path;

use tf_tree::FrozenHeader;

use crate::{IngestError, IngestOptions, Ingested};

/// Ingest `source` and write the result to `out` as a `.tft`.
///
/// Returns the ingest alongside the container header that was written, so a
/// caller can print both the report and the file's geometry without re-opening
/// it.
///
/// # Errors
///
/// Any [`IngestError`], including [`IngestError::Frozen`] for a failing write.
pub fn freeze_bag(
    source: &Path,
    out: &Path,
    opts: &IngestOptions,
    frames: &mut crate::Frames,
) -> Result<(Ingested, FrozenHeader), IngestError> {
    let digest = crate::digest_file(source)?;
    let ingested = crate::run(source, opts, frames)?;
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    let header = ingested
        .tree
        .freeze_to(out, Some(&source.display().to_string()), digest, created)
        .map_err(IngestError::Frozen)?;
    Ok((ingested, header))
}
