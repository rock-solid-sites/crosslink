//! CDP-1 reader: pinned-commit, digest-ladder, provenance (spec §6).
//!
//! The reader pins one broker commit `C`, reads only the manifest's active
//! chunks at that commit, verifies every digest and size, then decompresses and
//! parses. It never enumerates the chunk directory, and it never treats a
//! missing path as deletion.

use std::collections::HashMap;

use super::manifest::{CheckpointManifestV1, ManifestContext, ManifestDefect, SourceCheckpoint};
use super::source::JournalAnchor;
use super::{Cdp1Config, MANIFEST_PATH, MAX_MANIFEST_BYTES};
use crate::checkpoint::CheckpointState;
use crate::events::OrderingKey;
use crate::state_broker::client::StateEntry;
use crate::state_broker::error::StateBrokerError;
use crate::state_broker::transport::ProjectStateTransport;

/// Reader provenance profile (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// The reconstructed bytes were cross-checked against the git checkpoint
    /// blob: `source.state_blob_sha` and `source.state_sha256` matched.
    JournalAnchored,
    /// Broker-only read: internally consistent, but not proven against the git
    /// journal. Advisory only; never decision-bearing.
    Advisory,
}

impl Provenance {
    /// Stable label used in the projection marker.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::JournalAnchored => "journal_anchored",
            Self::Advisory => "advisory",
        }
    }
}

/// Caller expectations for a derived read.
#[derive(Debug, Clone, Default)]
pub struct ReadExpectations {
    /// Pin the source checkpoint commit.
    pub source_commit: Option<String>,
    /// Reject a manifest older than this watermark.
    pub min_watermark: Option<OrderingKey>,
    /// Require the git journal anchor (mandatory for hydration).
    pub journal_anchored: bool,
}

/// Reader defect classes (spec §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderDefect {
    /// No durable state at all.
    NoDurableState,
    /// No manifest at the pinned commit.
    ManifestMissing,
    /// Manifest over the size cap.
    OversizedManifest,
    /// Manifest JSON/schema invalid.
    MalformedManifest,
    /// A blob was answered at another commit than the pinned one.
    ProtocolMismatch,
    /// A blob disagrees with the head inventory.
    InventoryMismatch,
    /// A listed chunk is missing or corrupt.
    CorruptChunk,
    /// Declared lengths disagree with the content.
    Truncated,
    /// Decompressed state digest/size mismatch.
    CorruptState,
    /// State JSON invalid.
    MalformedState,
    /// Manifest/state watermark disagreement.
    WrongCheckpoint,
    /// `min_watermark` not satisfied.
    Stale,
    /// Journal anchoring was required but no anchor was supplied.
    ProvenanceRequired,
    /// The git anchor check failed.
    ProvenanceMismatch,
}

impl ReaderDefect {
    /// Stable machine-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoDurableState => "no_durable_state",
            Self::ManifestMissing => "manifest_missing",
            Self::OversizedManifest => "oversized_manifest",
            Self::MalformedManifest => "malformed_manifest",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::InventoryMismatch => "inventory_mismatch",
            Self::CorruptChunk => "corrupt_chunk",
            Self::Truncated => "truncated",
            Self::CorruptState => "corrupt_state",
            Self::MalformedState => "malformed_state",
            Self::WrongCheckpoint => "wrong_checkpoint",
            Self::Stale => "stale",
            Self::ProvenanceRequired => "provenance_required",
            Self::ProvenanceMismatch => "provenance_mismatch",
        }
    }
}

fn defect(defect: ReaderDefect, detail: impl Into<String>) -> StateBrokerError {
    StateBrokerError::protocol_with_details(
        format!("{}: {}", defect.label(), detail.into()),
        Some(serde_json::json!({ "defect": defect.label() })),
    )
}

fn manifest_defect(error: &super::manifest::ManifestError) -> StateBrokerError {
    let label = match error.defect {
        ManifestDefect::WrongFormat
        | ManifestDefect::WrongOrdering
        | ManifestDefect::UnsupportedEncoding
        | ManifestDefect::WrongOperation => ReaderDefect::MalformedManifest,
        ManifestDefect::WrongProject | ManifestDefect::WrongCheckpoint => {
            ReaderDefect::WrongCheckpoint
        }
        ManifestDefect::Corruption => ReaderDefect::CorruptState,
        ManifestDefect::Truncation => ReaderDefect::Truncated,
        ManifestDefect::Oversized => ReaderDefect::OversizedManifest,
    };
    defect(label, error.to_string())
}

/// A fully verified derived checkpoint.
#[derive(Debug, Clone)]
pub struct VerifiedCheckpoint {
    /// The pinned broker commit.
    pub commit: String,
    /// Parsed manifest.
    pub manifest: CheckpointManifestV1,
    /// SHA-256 of the manifest bytes.
    pub manifest_sha256: String,
    /// Parsed checkpoint state.
    pub state: CheckpointState,
    /// The exact decompressed state bytes.
    pub state_bytes: Vec<u8>,
    /// Provenance profile.
    pub provenance: Provenance,
}

/// Read and verify the derived checkpoint at the current broker head (spec §6.2).
///
/// # Errors
///
/// Returns a protocol error with `details.defect` on any validation failure, or
/// the underlying transport error.
pub fn read_derived_checkpoint(
    transport: &dyn ProjectStateTransport,
    cfg: &Cdp1Config,
    expect: &ReadExpectations,
    anchor: Option<&dyn JournalAnchor>,
) -> Result<VerifiedCheckpoint, StateBrokerError> {
    let state = transport.read_state()?;
    let Some(head) = state.state.head.as_ref() else {
        return Err(defect(
            ReaderDefect::NoDurableState,
            "the project has no durable state to read",
        ));
    };
    let commit = head.commit.clone();
    let inventory: HashMap<&str, &StateEntry> = state
        .state
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();

    // Manifest, pinned to C.
    let manifest_blob = match transport.read_blob(MANIFEST_PATH, Some(&commit)) {
        Ok(blob) => blob,
        Err(error) if error.code() == crate::state_broker::BrokerErrorCode::NotFound => {
            return Err(defect(
                ReaderDefect::ManifestMissing,
                format!("no {MANIFEST_PATH} at {commit}"),
            ));
        }
        Err(error) => return Err(error),
    };
    if manifest_blob.commit != commit {
        return Err(defect(
            ReaderDefect::ProtocolMismatch,
            format!(
                "manifest was answered at {} but the read was pinned to {commit}",
                manifest_blob.commit
            ),
        ));
    }
    if let Some(entry) = inventory.get(MANIFEST_PATH) {
        if entry.blob_sha != manifest_blob.blob_sha
            || entry.size.is_some_and(|size| size != manifest_blob.size)
        {
            return Err(defect(
                ReaderDefect::InventoryMismatch,
                "manifest blob disagrees with the head inventory",
            ));
        }
    }
    let manifest_bytes = manifest_blob.bytes()?;
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(defect(
            ReaderDefect::OversizedManifest,
            format!("manifest is {} bytes", manifest_bytes.len()),
        ));
    }
    let manifest = CheckpointManifestV1::from_slice(&manifest_bytes)
        .map_err(|e| defect(ReaderDefect::MalformedManifest, e.message()))?;
    manifest
        .validate(&ManifestContext {
            project_uuid: &cfg.project_uuid,
            state_ref: &cfg.state_ref,
            expected_source_commit: expect.source_commit.as_deref(),
            accounting: cfg.accounting,
        })
        .map_err(|e| manifest_defect(&e))?;

    if let Some(min) = &expect.min_watermark {
        if manifest.source.watermark < *min {
            return Err(defect(
                ReaderDefect::Stale,
                format!(
                    "manifest watermark {}/{} is older than the required minimum",
                    manifest.source.watermark.agent_id, manifest.source.watermark.agent_seq
                ),
            ));
        }
    }

    // Active chunks, pinned to C.
    let mut payload = Vec::with_capacity(manifest.payload_bytes as usize);
    for chunk in &manifest.chunks {
        let blob = match transport.read_blob(&chunk.path, Some(&commit)) {
            Ok(blob) => blob,
            Err(error) if error.code() == crate::state_broker::BrokerErrorCode::NotFound => {
                return Err(defect(
                    ReaderDefect::Truncated,
                    format!("active chunk {} is missing at {commit}", chunk.slot),
                ));
            }
            Err(error) => return Err(error),
        };
        if blob.commit != commit {
            return Err(defect(
                ReaderDefect::ProtocolMismatch,
                format!(
                    "chunk {} was answered at {} but the read was pinned to {commit}",
                    chunk.slot, blob.commit
                ),
            ));
        }
        if let Some(entry) = inventory.get(chunk.path.as_str()) {
            if entry.blob_sha != blob.blob_sha || entry.size.is_some_and(|size| size != blob.size) {
                return Err(defect(
                    ReaderDefect::InventoryMismatch,
                    format!(
                        "chunk {} blob disagrees with the head inventory",
                        chunk.slot
                    ),
                ));
            }
        }
        let bytes = blob.bytes()?;
        if bytes.len() as u64 != chunk.size
            || crate::state_broker::digest::sha256_hex(&bytes) != chunk.sha256
        {
            return Err(defect(
                ReaderDefect::CorruptChunk,
                format!("chunk {} failed its size/digest check", chunk.slot),
            ));
        }
        payload.extend_from_slice(&bytes);
    }
    manifest
        .validate_payload(&payload)
        .map_err(|e| manifest_defect(&e))?;

    let state_bytes = super::payload::gzip_decode_bounded(&payload, manifest.source.state_bytes)
        .map_err(|e| defect(ReaderDefect::CorruptState, e.message()))?;
    if state_bytes.len() as u64 != manifest.source.state_bytes
        || crate::state_broker::digest::sha256_hex(&state_bytes) != manifest.source.state_sha256
    {
        return Err(defect(
            ReaderDefect::CorruptState,
            "decompressed state failed its size/digest check",
        ));
    }
    let parsed = CheckpointState::from_slice(&state_bytes)
        .map_err(|e| defect(ReaderDefect::MalformedState, e.to_string()))?;
    if parsed.watermark.as_ref() != Some(&manifest.source.watermark) {
        return Err(defect(
            ReaderDefect::WrongCheckpoint,
            "manifest watermark does not match the decoded state watermark",
        ));
    }

    let provenance = if expect.journal_anchored {
        let anchor = anchor.ok_or_else(|| {
            defect(
                ReaderDefect::ProvenanceRequired,
                "journal anchoring was required but no anchor was supplied",
            )
        })?;
        anchor
            .verify_anchor(&manifest.source, &state_bytes)
            .map_err(|e| defect(ReaderDefect::ProvenanceMismatch, e.message()))?;
        Provenance::JournalAnchored
    } else {
        Provenance::Advisory
    };

    Ok(VerifiedCheckpoint {
        commit,
        manifest_sha256: crate::state_broker::digest::sha256_hex(&manifest_bytes),
        manifest,
        state: parsed,
        state_bytes,
        provenance,
    })
}

/// Whether `state_bytes` is the blob named by `source` (anchor helper for
/// tests and callers without a git source).
#[must_use]
pub fn anchor_matches(source: &SourceCheckpoint, state_bytes: &[u8]) -> bool {
    state_bytes.len() as u64 == source.state_bytes
        && crate::state_broker::digest::sha256_hex(state_bytes) == source.state_sha256
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_labels_are_stable() {
        assert_eq!(Provenance::JournalAnchored.label(), "journal_anchored");
        assert_eq!(Provenance::Advisory.label(), "advisory");
    }
}
