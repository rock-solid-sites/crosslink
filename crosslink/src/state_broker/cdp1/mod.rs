//! CDP-1 — the ADR-802 C-now derived-checkpoint publish protocol.
//!
//! This module implements the protocol frozen at
//! `.design/state-broker-derived-publish.md` (revision `7e3f71f94`): publish one
//! derived Crosslink checkpoint into broker v1 as a manifest plus fixed-slot
//! chunks in a single whole-tree CAS commit.
//!
//! # What CDP-1 is
//!
//! - **Source authority.** The published document is the exact blob of
//!   `state.json` in a *pushed* git checkpoint commit
//!   (`refs/heads/crosslink/checkpoint`). The publisher copies bytes; it never
//!   re-reduces state and never reads a local projection or `SQLite`.
//! - **Canonical serialization.** `payload = gzip(state.json bytes)` with a
//!   deterministic header (`mtime=0, xfl=2, os=255`, level 9).
//! - **Fixed slots.** `checkpoint/chunks/0000..0003` plus
//!   `checkpoint/manifest.json`, written in one CAS commit. Old unlisted slots
//!   are inert; readers never glob the chunk directory.
//! - **Semantic identity.** `(watermark, source.state_sha256)` decides
//!   `AlreadyCurrent` vs `Diverged`; `source.commit`, `state_blob_sha`,
//!   `state_bytes`, and `payload_sha256` are provenance.
//! - **Provenance.** A reader is either `JournalAnchored` (git cross-check of
//!   the source blob) or `Advisory`; only journal-anchored projections may feed
//!   the authoritative hydration seam.
//!
//! # What CDP-1 is not
//!
//! It is not a journal write path, not a prune gate, and not a `SyncManager`
//! transport. The broker remains a derived checkpoint/read tier (ADR-802 §3,
//! §14). There is no delete, no inbox, and no broker v2 here.

pub mod attempt;
pub mod capacity;
pub mod manifest;
pub mod payload;
pub mod projection;
pub mod publisher;
pub mod reader;
pub mod source;

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use attempt::{AttemptPhase, AttemptRecord, AttemptResolution, AttemptStore};
#[allow(unused_imports)]
pub use capacity::{check_capacity, CapacityError, CapacityPlan};
#[allow(unused_imports)]
pub use manifest::{
    CheckpointManifestV1, ChunkEntry, Encoding, ManifestContext, ManifestDefect, ManifestError,
    SourceCheckpoint,
};
#[allow(unused_imports)]
pub use payload::{gzip_decode_bounded, gzip_encode, join_payload, split_payload};
#[allow(unused_imports)]
pub use projection::{
    verify_derived_projection, write_derived_projection, DerivedProjectionMarker,
    ProjectionFileV2,
};
#[allow(unused_imports)]
pub use publisher::{
    classify_head, plan_publish, publish_checkpoint, Cdp1Config, DivergenceReason, HeadVerdict,
    PublishOptions, PublishOutcome, PublishPlan, PublisherIdentity, RefusalReason,
    ReconcileFailure,
};
#[allow(unused_imports)]
pub use reader::{read_derived_checkpoint, Provenance, ReadExpectations, VerifiedCheckpoint};
#[allow(unused_imports)]
pub use source::{
    CheckpointSource, GitCheckpointSource, JournalAnchor, PushedCheckpoint, RepositoryBinding,
};

/// Manifest schema identifier (`CheckpointManifestV1::schema`).
pub const MANIFEST_SCHEMA: &str = "crosslink-checkpoint-manifest/v1";
/// Broker logical path of the manifest.
pub const MANIFEST_PATH: &str = "checkpoint/manifest.json";
/// Broker logical directory of the fixed slots.
pub const CHUNK_DIR: &str = "checkpoint/chunks";
/// Git ref holding the authoritative v3 checkpoint.
pub const SOURCE_REF: &str = "refs/heads/crosslink/checkpoint";
/// Remote-tracking ref used to prove the checkpoint is pushed.
pub const SOURCE_TRACKING_REF: &str = "refs/crosslink-remote/checkpoint";
/// Git tree path of the state blob inside the checkpoint commit.
pub const SOURCE_STATE_PATH: &str = "state.json";
/// State document family recorded in the manifest.
pub const STATE_FORMAT: &str = "crosslink-checkpoint-state/json";
/// Compression identifier recorded in the manifest.
pub const COMPRESSION: &str = "gzip";
/// Compression level used by the publisher.
pub const COMPRESSION_LEVEL: u32 = 9;
/// Exact gzip header the publisher must emit.
pub const GZIP_HEADER: &str = "mtime=0,xfl=2,os=255";
/// Compression implementation provenance (updated with the lockfile).
pub const COMPRESSION_IMPL: &str = "flate2-1.1.10/miniz_oxide-0.9.1";
/// Broker per-commit budget in decoded bytes.
pub const COMMIT_BUDGET_BYTES: u64 = 1_048_576;
/// Fail-closed cap on the manifest's own serialized size.
pub const MAX_MANIFEST_BYTES: usize = 4_096;
/// Broker files-per-commit limit.
pub const MAX_FILES_PER_COMMIT: usize = 32;
/// Reader-side decompression cap.
pub const MAX_STATE_BYTES: u64 = 16_777_216;
/// Maximum number of fixed slots under the broker-v1 budget.
pub const MAX_SLOTS: u32 = 4;
/// v2 projection marker schema.
pub const PROJECTION_SCHEMA: &str = "crosslink-state-projection/v2";
/// v2 projection marker file name (`+` is outside the broker path grammar).
pub const PROJECTION_MARKER_FILE: &str = ".crosslink-state-projection+v2.json";
/// Attempt-record schema.
pub const ATTEMPT_SCHEMA: &str = "crosslink-publish-attempt/v1";
/// Attempt-record path relative to `.crosslink/`.
pub const ATTEMPT_REL_PATH: &str = "state-broker/publish-attempt.json";
/// Hook-config key carrying the repository binding.
pub const BINDING_KEY: &str = "state_broker_binding";

/// Broker limit-accounting model (spec §8.5).
///
/// The deployed broker's authoritative accounting is not yet resolved; the
/// primary model is [`AccountingModel::Decoded`] because the client mirror in
/// `state_broker::validate` was derived from the broker source and checks
/// decoded content lengths. The `Wire` model is the fail-closed fallback and
/// parameterizes every capacity constant so the switch is one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountingModel {
    /// Limits apply to decoded file content (primary).
    Decoded,
    /// Limits apply to the base64-encoded wire body (fallback).
    Wire,
}

impl AccountingModel {
    /// Stable label for reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Decoded => "decoded",
            Self::Wire => "wire",
        }
    }

    /// Maximum decoded bytes in one chunk slot.
    #[must_use]
    pub const fn slot_bytes(self) -> u64 {
        match self {
            Self::Decoded => 262_144,
            Self::Wire => 196_608,
        }
    }

    /// Maximum decoded payload across all chunks (manifest allowance removed).
    #[must_use]
    pub const fn max_payload_bytes(self) -> u64 {
        match self {
            Self::Decoded => 1_044_480,
            Self::Wire => 782_336,
        }
    }
}

/// The accounting model this build publishes with.
pub const COMMIT_BUDGET_ACCOUNTING: AccountingModel = AccountingModel::Decoded;

/// Broker logical path of one fixed slot.
///
/// `slot` is bounded by [`MAX_SLOTS`]; the path is always
/// `checkpoint/chunks/<slot:04>`.
#[must_use]
pub fn chunk_path(slot: u32) -> String {
    format!("{CHUNK_DIR}/{slot:04}")
}

/// The owned broker namespace under C-now (ADR-802 §5).
#[must_use]
pub fn owned_paths() -> Vec<String> {
    let mut paths = Vec::with_capacity(MAX_SLOTS as usize + 1);
    paths.push(MANIFEST_PATH.to_string());
    for slot in 0..MAX_SLOTS {
        paths.push(chunk_path(slot));
    }
    paths
}

#[cfg(test)]
mod core_tests {
    use super::*;

    #[test]
    fn chunk_paths_are_zero_padded_and_grammar_legal() {
        assert_eq!(chunk_path(0), "checkpoint/chunks/0000");
        assert_eq!(chunk_path(3), "checkpoint/chunks/0003");
        for path in owned_paths() {
            crate::state_broker::validate::validate_logical_path(&path)
                .unwrap_or_else(|e| panic!("{path} must be broker-legal: {e}"));
        }
    }

    #[test]
    fn accounting_models_are_consistent() {
        // Decoded: 1 MiB - 4 KiB manifest allowance, 4 slots of 256 KiB.
        assert_eq!(AccountingModel::Decoded.slot_bytes(), 262_144);
        assert_eq!(AccountingModel::Decoded.max_payload_bytes(), 1_044_480);
        // Wire: 3/4 of each cap (base64), same 4-slot bound.
        assert_eq!(AccountingModel::Wire.slot_bytes(), 196_608);
        assert_eq!(AccountingModel::Wire.max_payload_bytes(), 782_336);
        for model in [AccountingModel::Decoded, AccountingModel::Wire] {
            let slots = model.max_payload_bytes().div_ceil(model.slot_bytes());
            assert!(slots <= u64::from(MAX_SLOTS), "{model:?} needs {slots} slots");
        }
    }
}
