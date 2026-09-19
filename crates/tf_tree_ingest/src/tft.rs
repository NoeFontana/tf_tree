//! Recording → `.tft`: a real `source_digest` (BLAKE3 of the recording's bytes,
//! streamed; §2.3) and a report, which a `--from-live` `.tft` cannot have.

use std::path::Path;

use tf_tree::FrozenHeader;

use crate::{IngestError, IngestOptions, Ingested};

/// Ingest `source` and write the result to `out` as a `.tft`.
///
/// Returns the ingest alongside the header that was written.
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
