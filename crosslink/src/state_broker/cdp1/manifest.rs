//! CDP-1 manifest: exact schema, canonical bytes, and validation (spec §4).
//!
//! The manifest is serialized with compact JSON (`serde_json::to_vec`) over a
//! fixed field-order struct, contains no wall-clock fields and no maps, and
//! therefore has exactly one canonical byte representation per value set. It is
//! the only index of the active chunk set: readers must never enumerate the
//! chunk directory.

use serde::{Deserialize, Serialize};

use super::{AccountingModel, MAX_MANIFEST_BYTES, MAX_SLOTS, MAX_STATE_BYTES};
use crate::events::OrderingKey;
use crate::state_broker::validate::{is_canonical_uuid, is_commit_sha, validate_op_id};

/// Compression/encoding provenance block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Encoding {
    /// State document family (`crosslink-checkpoint-state/json`).
    pub state_format: String,
    /// Compression identifier (`gzip`).
    pub compression: String,
    /// Compression level (9).
    pub compression_level: u32,
    /// Codec provenance (not a reader requirement).
    pub compression_impl: String,
    /// Header contract (`mtime=0,xfl=2,os=255`).
    pub gzip_header: String,
}

/// Provenance of the exact git blob this publish derives from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCheckpoint {
    /// Always `refs/heads/crosslink/checkpoint`.
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// Pushed git checkpoint commit `S`.
    pub commit: String,
    /// Git tree path inside `S` (`state.json`).
    pub state_path: String,
    /// Git blob sha of `S:state.json`.
    pub state_blob_sha: String,
    /// SHA-256 of the uncompressed state bytes.
    pub state_sha256: String,
    /// Uncompressed length.
    pub state_bytes: u64,
    /// Checkpoint watermark (must equal the decoded state's watermark).
    pub watermark: OrderingKey,
}

/// One active chunk slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    /// 0-based slot index; contiguous ascending.
    pub slot: u32,
    /// Always `checkpoint/chunks/<slot:04>`.
    pub path: String,
    /// Compressed bytes in this slot (1..=slot_bytes).
    pub size: u64,
    /// SHA-256 of this slot's bytes.
    pub sha256: String,
}

/// The CDP-1 manifest (schema `crosslink-checkpoint-manifest/v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifestV1 {
    /// Schema identifier.
    pub schema: String,
    /// Broker project UUID (canonical lowercase).
    pub project_uuid: String,
    /// Broker state ref (`refs/heads/projects/<uuid>/state`).
    pub state_ref: String,
    /// Publisher identity (agent-id charset, 3..=64).
    pub publisher_id: String,
    /// Broker op id (`[A-Za-z0-9._:-]{1,128}`).
    pub op_id: String,
    /// Source checkpoint provenance.
    pub source: SourceCheckpoint,
    /// Encoding provenance.
    pub encoding: Encoding,
    /// Compressed payload length (sum of chunk sizes).
    pub payload_bytes: u64,
    /// SHA-256 of the concatenated compressed payload.
    pub payload_sha256: String,
    /// Slot payload cap used by this publish (accounting model).
    pub chunk_slot_bytes: u64,
    /// Number of active chunks (`1..=MAX_SLOTS`).
    pub chunk_count: u32,
    /// Active chunks, ordered by ascending slot.
    pub chunks: Vec<ChunkEntry>,
}

/// Class of a manifest defect (spec §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestDefect {
    /// M1: schema id mismatch.
    WrongFormat,
    /// M2/M3: project UUID or state ref mismatch.
    WrongProject,
    /// M4/M5/M6/M13: source ref/commit/path/watermark mismatch.
    WrongCheckpoint,
    /// M7/M11: malformed digests or sizes.
    Corruption,
    /// M8: declared lengths disagree with the content.
    Truncation,
    /// M9/M10: slot ordering, count, or path derivation.
    WrongOrdering,
    /// M12/M14: manifest or state exceeds a cap.
    Oversized,
    /// M15: unknown encoding/compression.
    UnsupportedEncoding,
    /// M16: op id grammar.
    WrongOperation,
}

impl ManifestDefect {
    /// Stable machine-readable label (used in error `details.defect`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WrongFormat => "wrong_format",
            Self::WrongProject => "wrong_project",
            Self::WrongCheckpoint => "wrong_checkpoint",
            Self::Corruption => "corruption",
            Self::Truncation => "truncation",
            Self::WrongOrdering => "wrong_ordering",
            Self::Oversized => "oversized",
            Self::UnsupportedEncoding => "unsupported_encoding",
            Self::WrongOperation => "wrong_operation",
        }
    }
}

/// A typed manifest defect with context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    /// Defect class.
    pub defect: ManifestDefect,
    /// Human-readable detail.
    pub detail: String,
}

impl ManifestError {
    fn new(defect: ManifestDefect, detail: impl Into<String>) -> Self {
        Self {
            defect,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.defect.label(), self.detail)
    }
}

impl std::error::Error for ManifestError {}

/// Context a manifest is validated against.
#[derive(Debug, Clone, Copy)]
pub struct ManifestContext<'a> {
    /// The transport's configured project UUID.
    pub project_uuid: &'a str,
    /// The transport's state ref.
    pub state_ref: &'a str,
    /// Caller-pinned source commit, when supplied.
    pub expected_source_commit: Option<&'a str>,
    /// Active accounting model.
    pub accounting: AccountingModel,
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_publisher_id(value: &str) -> bool {
    let len = value.len();
    (3..=64).contains(&len)
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl CheckpointManifestV1 {
    /// Canonical compact JSON bytes (the exact bytes committed at
    /// [`super::MANIFEST_PATH`]).
    ///
    /// # Errors
    ///
    /// Returns a protocol error if serialization fails (it cannot for this
    /// struct) or if the bytes exceed [`MAX_MANIFEST_BYTES`].
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, crate::state_broker::StateBrokerError> {
        let bytes = serde_json::to_vec(self).map_err(|e| {
            crate::state_broker::StateBrokerError::protocol(format!(
                "manifest serialization failed: {e}"
            ))
        })?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(crate::state_broker::StateBrokerError::protocol(format!(
                "manifest is {} bytes, over the {MAX_MANIFEST_BYTES}-byte cap",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// Parse a manifest from raw bytes without validating it.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the bytes are not JSON or not a manifest
    /// shape.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, crate::state_broker::StateBrokerError> {
        serde_json::from_slice(bytes).map_err(|e| {
            crate::state_broker::StateBrokerError::protocol(format!(
                "manifest is not valid crosslink-checkpoint-manifest/v1 JSON: {e}"
            ))
        })
    }

    /// Validate the manifest against `ctx` (spec §4.3 M1–M16, except M13/M14
    /// which need the decoded state and are checked by the reader).
    ///
    /// # Errors
    ///
    /// Returns the first [`ManifestError`].
    pub fn validate(&self, ctx: &ManifestContext<'_>) -> Result<(), ManifestError> {
        if self.schema != super::MANIFEST_SCHEMA {
            return Err(ManifestError::new(
                ManifestDefect::WrongFormat,
                format!("schema {:?}", self.schema),
            ));
        }
        if !is_canonical_uuid(&self.project_uuid) || self.project_uuid != ctx.project_uuid {
            return Err(ManifestError::new(
                ManifestDefect::WrongProject,
                format!(
                    "manifest project {} does not match configured {}",
                    self.project_uuid, ctx.project_uuid
                ),
            ));
        }
        if self.state_ref != ctx.state_ref {
            return Err(ManifestError::new(
                ManifestDefect::WrongProject,
                format!("state ref {:?}", self.state_ref),
            ));
        }
        if !is_publisher_id(&self.publisher_id) {
            return Err(ManifestError::new(
                ManifestDefect::WrongOperation,
                format!("publisher_id {:?}", self.publisher_id),
            ));
        }
        if validate_op_id(&self.op_id).is_err() {
            return Err(ManifestError::new(
                ManifestDefect::WrongOperation,
                format!("op_id {:?}", self.op_id),
            ));
        }
        if self.source.ref_name != super::SOURCE_REF {
            return Err(ManifestError::new(
                ManifestDefect::WrongCheckpoint,
                format!("source ref {:?}", self.source.ref_name),
            ));
        }
        if !is_commit_sha(&self.source.commit) {
            return Err(ManifestError::new(
                ManifestDefect::WrongCheckpoint,
                format!("source commit {:?}", self.source.commit),
            ));
        }
        if let Some(expected) = ctx.expected_source_commit {
            if self.source.commit != expected {
                return Err(ManifestError::new(
                    ManifestDefect::WrongCheckpoint,
                    format!(
                        "source commit {} does not match expected {expected}",
                        self.source.commit
                    ),
                ));
            }
        }
        if self.source.state_path != super::SOURCE_STATE_PATH {
            return Err(ManifestError::new(
                ManifestDefect::WrongCheckpoint,
                format!("source state path {:?}", self.source.state_path),
            ));
        }
        if !is_commit_sha(&self.source.state_blob_sha)
            || !is_sha256(&self.source.state_sha256)
            || !is_sha256(&self.payload_sha256)
        {
            return Err(ManifestError::new(
                ManifestDefect::Corruption,
                "digest field is not canonical hex",
            ));
        }
        if self.source.state_bytes == 0 {
            return Err(ManifestError::new(
                ManifestDefect::Corruption,
                "state_bytes is zero",
            ));
        }
        if self.source.state_bytes > MAX_STATE_BYTES {
            return Err(ManifestError::new(
                ManifestDefect::Oversized,
                format!("state_bytes {} over the safety cap", self.source.state_bytes),
            ));
        }
        if self.encoding.state_format != super::STATE_FORMAT {
            return Err(ManifestError::new(
                ManifestDefect::UnsupportedEncoding,
                format!("state_format {:?}", self.encoding.state_format),
            ));
        }
        if self.encoding.compression != super::COMPRESSION {
            return Err(ManifestError::new(
                ManifestDefect::UnsupportedEncoding,
                format!("compression {:?}", self.encoding.compression),
            ));
        }
        if self.chunk_slot_bytes != ctx.accounting.slot_bytes() {
            return Err(ManifestError::new(
                ManifestDefect::Corruption,
                format!(
                    "chunk_slot_bytes {} is not the active model's {}",
                    self.chunk_slot_bytes,
                    ctx.accounting.slot_bytes()
                ),
            ));
        }
        if self.chunk_count == 0
            || self.chunk_count > MAX_SLOTS
            || self.chunk_count as usize != self.chunks.len()
        {
            return Err(ManifestError::new(
                ManifestDefect::WrongOrdering,
                format!(
                    "chunk_count {} vs {} chunks (max {MAX_SLOTS})",
                    self.chunk_count,
                    self.chunks.len()
                ),
            ));
        }
        let mut sum: u64 = 0;
        for (index, chunk) in self.chunks.iter().enumerate() {
            if chunk.slot as usize != index {
                return Err(ManifestError::new(
                    ManifestDefect::WrongOrdering,
                    format!("chunk {index} carries slot {}", chunk.slot),
                ));
            }
            if chunk.path != super::chunk_path(chunk.slot) {
                return Err(ManifestError::new(
                    ManifestDefect::WrongOrdering,
                    format!("chunk {} path {:?}", chunk.slot, chunk.path),
                ));
            }
            if chunk.size == 0 || chunk.size > self.chunk_slot_bytes {
                return Err(ManifestError::new(
                    ManifestDefect::Corruption,
                    format!("chunk {} size {}", chunk.slot, chunk.size),
                ));
            }
            let is_last = index + 1 == self.chunks.len();
            if !is_last && chunk.size != self.chunk_slot_bytes {
                return Err(ManifestError::new(
                    ManifestDefect::Corruption,
                    format!(
                        "chunk {} is {} bytes but only the last chunk may be short",
                        chunk.slot, chunk.size
                    ),
                ));
            }
            if !is_sha256(&chunk.sha256) {
                return Err(ManifestError::new(
                    ManifestDefect::Corruption,
                    format!("chunk {} digest is not canonical hex", chunk.slot),
                ));
            }
            sum = sum.saturating_add(chunk.size);
        }
        if self.payload_bytes == 0 || self.payload_bytes != sum {
            return Err(ManifestError::new(
                ManifestDefect::Truncation,
                format!(
                    "payload_bytes {} but chunk sizes sum to {sum}",
                    self.payload_bytes
                ),
            ));
        }
        if self.payload_bytes > ctx.accounting.max_payload_bytes() {
            return Err(ManifestError::new(
                ManifestDefect::Oversized,
                format!(
                    "payload_bytes {} over the model cap {}",
                    self.payload_bytes,
                    ctx.accounting.max_payload_bytes()
                ),
            ));
        }
        if 1 + self.chunks.len() > super::MAX_FILES_PER_COMMIT {
            return Err(ManifestError::new(
                ManifestDefect::Oversized,
                "manifest plus chunks exceeds the broker file limit",
            ));
        }
        Ok(())
    }

    /// Validate that the manifest's declared payload length matches `payload`
    /// and that the digest ladder holds for the concatenation (M8, corruption).
    ///
    /// # Errors
    ///
    /// Returns the defect.
    pub fn validate_payload(&self, payload: &[u8]) -> Result<(), ManifestError> {
        if payload.len() as u64 != self.payload_bytes {
            return Err(ManifestError::new(
                ManifestDefect::Truncation,
                format!(
                    "payload is {} bytes but the manifest declares {}",
                    payload.len(),
                    self.payload_bytes
                ),
            ));
        }
        let digest = crate::state_broker::digest::sha256_hex(payload);
        if digest != self.payload_sha256 {
            return Err(ManifestError::new(
                ManifestDefect::Corruption,
                "payload_sha256 mismatch",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::OrderingKey;
    use chrono::{DateTime, Utc};

    pub(crate) fn sample_manifest() -> CheckpointManifestV1 {
        let state = b"{\"watermark\":null}";
        let payload = super::super::gzip_encode(state).expect("gzip");
        let slot = super::super::AccountingModel::Decoded.slot_bytes();
        let chunks: Vec<ChunkEntry> = payload
            .chunks(slot as usize)
            .enumerate()
            .map(|(i, bytes)| ChunkEntry {
                slot: i as u32,
                path: super::super::chunk_path(i as u32),
                size: bytes.len() as u64,
                sha256: crate::state_broker::digest::sha256_hex(bytes),
            })
            .collect();
        CheckpointManifestV1 {
            schema: super::super::MANIFEST_SCHEMA.to_string(),
            project_uuid: "7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f".to_string(),
            state_ref: "refs/heads/projects/7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f/state"
                .to_string(),
            publisher_id: "test-publisher".to_string(),
            op_id: "ckpt-a1b2c3d4e5f6-0123456789abcdef".to_string(),
            source: SourceCheckpoint {
                ref_name: super::super::SOURCE_REF.to_string(),
                commit: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string(),
                state_path: super::super::SOURCE_STATE_PATH.to_string(),
                state_blob_sha: "b2c3d4e5f60718293a4b5c6d7e8f901234567890".to_string(),
                state_sha256: crate::state_broker::digest::sha256_hex(state),
                state_bytes: state.len() as u64,
                watermark: OrderingKey {
                    timestamp: DateTime::<Utc>::UNIX_EPOCH,
                    agent_id: "driver".to_string(),
                    agent_seq: 1,
                },
            },
            encoding: Encoding {
                state_format: super::super::STATE_FORMAT.to_string(),
                compression: super::super::COMPRESSION.to_string(),
                compression_level: super::super::COMPRESSION_LEVEL,
                compression_impl: super::super::COMPRESSION_IMPL.to_string(),
                gzip_header: super::super::GZIP_HEADER.to_string(),
            },
            payload_bytes: payload.len() as u64,
            payload_sha256: crate::state_broker::digest::sha256_hex(&payload),
            chunk_slot_bytes: slot,
            chunk_count: chunks.len() as u32,
            chunks,
        }
    }

    pub(crate) fn ctx<'a>(
        uuid: &'a str,
        state_ref: &'a str,
    ) -> ManifestContext<'a> {
        ManifestContext {
            project_uuid: uuid,
            state_ref,
            expected_source_commit: None,
            accounting: super::super::AccountingModel::Decoded,
        }
    }

    #[test]
    fn canonical_bytes_round_trip_and_are_stable() {
        let manifest = sample_manifest();
        let a = manifest.to_canonical_bytes().unwrap();
        let b = manifest.to_canonical_bytes().unwrap();
        assert_eq!(a, b);
        let parsed = CheckpointManifestV1::from_slice(&a).unwrap();
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn valid_manifest_passes_validation() {
        let manifest = sample_manifest();
        let ctx = ctx(&manifest.project_uuid, &manifest.state_ref);
        manifest.validate(&ctx).unwrap();
    }

    #[test]
    fn validation_rejects_each_defect_class() {
        let base = sample_manifest();
        let ctx = ctx(&base.project_uuid, &base.state_ref);

        let mut wrong_schema = base.clone();
        wrong_schema.schema = "other/v1".into();
        assert_eq!(
            wrong_schema.validate(&ctx).unwrap_err().defect,
            ManifestDefect::WrongFormat
        );

        let mut wrong_project = base.clone();
        wrong_project.project_uuid = "0f0e0d0c-0b0a-4908-8706-050403020100".into();
        assert_eq!(
            wrong_project.validate(&ctx).unwrap_err().defect,
            ManifestDefect::WrongProject
        );

        let mut wrong_commit = base.clone();
        wrong_commit.source.commit = "deadbeef".into();
        assert_eq!(
            wrong_commit.validate(&ctx).unwrap_err().defect,
            ManifestDefect::WrongCheckpoint
        );

        let mut wrong_order = base.clone();
        wrong_order.chunk_count += 1; // count disagrees with the chunk list
        assert_eq!(
            wrong_order.validate(&ctx).unwrap_err().defect,
            ManifestDefect::WrongOrdering
        );

        let mut wrong_size = base.clone();
        wrong_size.payload_bytes += 1;
        assert_eq!(
            wrong_size.validate(&ctx).unwrap_err().defect,
            ManifestDefect::Truncation
        );

        let mut wrong_digest = base.clone();
        wrong_digest.payload_sha256 = "0".repeat(64);
        assert!(wrong_digest.validate(&ctx).is_ok());
        let right_length = vec![0u8; wrong_digest.payload_bytes as usize];
        assert_eq!(
            wrong_digest.validate_payload(&right_length).unwrap_err().defect,
            ManifestDefect::Corruption
        );
        assert_eq!(
            wrong_digest.validate_payload(&[0u8; 1]).unwrap_err().defect,
            ManifestDefect::Truncation
        );

        let mut oversized = base.clone();
        oversized.source.state_bytes = super::super::MAX_STATE_BYTES + 1;
        assert_eq!(
            oversized.validate(&ctx).unwrap_err().defect,
            ManifestDefect::Oversized
        );

        let mut bad_encoding = base.clone();
        bad_encoding.encoding.compression = "zstd".into();
        assert_eq!(
            bad_encoding.validate(&ctx).unwrap_err().defect,
            ManifestDefect::UnsupportedEncoding
        );
    }
}
