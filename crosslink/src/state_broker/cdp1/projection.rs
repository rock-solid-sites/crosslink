//! CDP-1 v2 projection: identity/freshness marker and the authoritative
//! hydration gate (spec §9).
//!
//! A projection is disposable. It carries a marker derived from the verified
//! manifest, and only a `journal_anchored` projection may be fed to the
//! authoritative hydration seam.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::reader::{read_derived_checkpoint, Provenance, ReadExpectations};
use super::source::JournalAnchor;
use super::{Cdp1Config, MANIFEST_PATH, PROJECTION_MARKER_FILE, PROJECTION_SCHEMA};
use crate::events::OrderingKey;
use crate::state_broker::error::StateBrokerError;
use crate::state_broker::transport::ProjectStateTransport;

/// One projected file's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionFileV2 {
    /// Path relative to the projection root.
    pub path: String,
    /// SHA-256 of the projected bytes.
    pub sha256: String,
    /// Byte size.
    pub size: u64,
}

/// Identity/freshness marker for a derived-checkpoint projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedProjectionMarker {
    /// Marker schema (`crosslink-state-projection/v2`).
    pub schema: String,
    /// Backend host the projection came from, when known.
    #[serde(default)]
    pub backend_host: Option<String>,
    /// Broker project UUID.
    pub project_uuid: String,
    /// Broker state ref.
    pub state_ref: String,
    /// Pinned broker commit.
    pub head_commit: String,
    /// `false` until every file is written.
    pub complete: bool,
    /// `journal_anchored` or `advisory`.
    pub provenance: String,
    /// Always `checkpoint/manifest.json`.
    pub manifest_path: String,
    /// SHA-256 of the manifest bytes at the pinned commit.
    pub manifest_sha256: String,
    /// Broker op id from the manifest.
    pub op_id: String,
    /// Source checkpoint ref.
    pub source_checkpoint_ref: String,
    /// Source checkpoint commit.
    pub source_checkpoint_commit: String,
    /// Source state blob sha.
    pub source_state_blob_sha: String,
    /// Uncompressed state digest.
    pub state_sha256: String,
    /// Source watermark.
    pub watermark: OrderingKey,
    /// Compressed payload digest.
    pub payload_sha256: String,
    /// Active chunk count.
    pub chunk_count: u32,
    /// Projected files.
    pub files: Vec<ProjectionFileV2>,
    /// Total projected bytes.
    pub bytes: u64,
}

/// The projected state path relative to the projection root.
pub const PROJECTED_STATE_PATH: &str = "checkpoint/state.json";

fn marker_path(dir: &Path) -> std::path::PathBuf {
    dir.join(PROJECTION_MARKER_FILE)
}

fn write_marker(dir: &Path, marker: &DerivedProjectionMarker) -> Result<(), StateBrokerError> {
    std::fs::create_dir_all(dir).map_err(|e| {
        StateBrokerError::local_io(format!(
            "cannot create projection directory {}: {e}",
            dir.display()
        ))
    })?;
    let bytes = serde_json::to_vec_pretty(marker).map_err(|e| {
        StateBrokerError::local_io(format!("cannot serialize the projection marker: {e}"))
    })?;
    crate::utils::atomic_write(&marker_path(dir), &bytes).map_err(|e| {
        StateBrokerError::local_io(format!(
            "cannot write projection marker {}: {e}",
            marker_path(dir).display()
        ))
    })
}

/// Read a v2 projection marker.
///
/// # Errors
///
/// Returns a local-IO error when the marker is missing or malformed — a
/// projection without a readable marker is never treated as current.
pub fn read_marker(dir: &Path) -> Result<DerivedProjectionMarker, StateBrokerError> {
    let path = marker_path(dir);
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        StateBrokerError::local_io(format!(
            "{} is not a broker projection (no readable {}): {e}",
            dir.display(),
            PROJECTION_MARKER_FILE
        ))
    })?;
    let marker: DerivedProjectionMarker = serde_json::from_str(&raw).map_err(|e| {
        StateBrokerError::local_io(format!("projection marker {} is malformed: {e}", path.display()))
    })?;
    if marker.schema != PROJECTION_SCHEMA {
        return Err(StateBrokerError::local_io(format!(
            "projection marker schema {:?} is not {PROJECTION_SCHEMA}",
            marker.schema
        )));
    }
    Ok(marker)
}

/// Materialize a `journal_anchored` projection into `dir`.
///
/// # Errors
///
/// Returns a protocol error when the read is not journal-anchored, and a
/// local-IO error when writing fails.
pub fn write_derived_projection(
    transport: &dyn ProjectStateTransport,
    cfg: &Cdp1Config,
    dir: &Path,
    expect: &ReadExpectations,
    anchor: Option<&dyn JournalAnchor>,
) -> Result<DerivedProjectionMarker, StateBrokerError> {
    let anchored = ReadExpectations {
        journal_anchored: true,
        ..expect.clone()
    };
    let verified = read_derived_checkpoint(transport, cfg, &anchored, anchor)?;
    if verified.provenance != Provenance::JournalAnchored {
        return Err(StateBrokerError::local_io(
            "refusing to hydrate an advisory projection; journal anchoring is required",
        ));
    }
    let mut marker = DerivedProjectionMarker {
        schema: PROJECTION_SCHEMA.to_string(),
        backend_host: transport.backend_host(),
        project_uuid: cfg.project_uuid.clone(),
        state_ref: cfg.state_ref.clone(),
        head_commit: verified.commit.clone(),
        complete: false,
        provenance: verified.provenance.label().to_string(),
        manifest_path: MANIFEST_PATH.to_string(),
        manifest_sha256: verified.manifest_sha256.clone(),
        op_id: verified.manifest.op_id.clone(),
        source_checkpoint_ref: verified.manifest.source.ref_name.clone(),
        source_checkpoint_commit: verified.manifest.source.commit.clone(),
        source_state_blob_sha: verified.manifest.source.state_blob_sha.clone(),
        state_sha256: verified.manifest.source.state_sha256.clone(),
        watermark: verified.manifest.source.watermark.clone(),
        payload_sha256: verified.manifest.payload_sha256.clone(),
        chunk_count: verified.manifest.chunk_count,
        files: Vec::new(),
        bytes: 0,
    };
    // Invalidate any previous marker before touching files.
    write_marker(dir, &marker)?;

    let target = dir.join(PROJECTED_STATE_PATH);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            StateBrokerError::local_io(format!(
                "cannot create projection directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    std::fs::write(&target, &verified.state_bytes).map_err(|e| {
        StateBrokerError::local_io(format!(
            "cannot write projected state {}: {e}",
            target.display()
        ))
    })?;
    let digest = crate::state_broker::digest::sha256_hex(&verified.state_bytes);
    if digest != marker.state_sha256 {
        return Err(StateBrokerError::local_io(
            "projected state failed its digest check",
        ));
    }
    marker.files = vec![ProjectionFileV2 {
        path: PROJECTED_STATE_PATH.to_string(),
        sha256: digest,
        size: verified.state_bytes.len() as u64,
    }];
    marker.bytes = verified.state_bytes.len() as u64;
    marker.complete = true;
    write_marker(dir, &marker)?;
    Ok(marker)
}

/// Verify that `dir` holds a complete, current, correctly-provenanced
/// projection, returning its marker.
///
/// # Errors
///
/// Fails closed on a missing/incomplete/stale/tampered projection, on identity
/// mismatch, and on a required-but-absent journal anchor.
pub fn verify_derived_projection(
    transport: &dyn ProjectStateTransport,
    cfg: &Cdp1Config,
    dir: &Path,
    expect: &ReadExpectations,
    anchor: Option<&dyn JournalAnchor>,
) -> Result<DerivedProjectionMarker, StateBrokerError> {
    let marker = read_marker(dir)?;
    if !marker.complete {
        return Err(StateBrokerError::local_io(format!(
            "projection in {} is incomplete; re-hydrate before using it",
            dir.display()
        )));
    }
    if marker.project_uuid != cfg.project_uuid || marker.state_ref != cfg.state_ref {
        return Err(StateBrokerError::identity_mismatch(format!(
            "projection in {} belongs to {}/{}",
            dir.display(),
            marker.project_uuid,
            marker.state_ref
        )));
    }
    if let (Some(marked), Some(current)) = (marker.backend_host.as_deref(), transport.backend_host())
    {
        if marked != current {
            return Err(StateBrokerError::identity_mismatch(format!(
                "projection in {} belongs to backend {marked}, not {current}",
                dir.display()
            )));
        }
    }
    let state = transport.read_state()?;
    let Some(head) = state.state.head.as_ref() else {
        return Err(StateBrokerError::local_io(
            "the durable state has no head; the projection cannot be current",
        ));
    };
    if head.commit != marker.head_commit {
        return Err(StateBrokerError::local_io(format!(
            "projection in {} is stale: marker head {} but durable head {}",
            dir.display(),
            marker.head_commit,
            head.commit
        )));
    }
    // Re-read the manifest and re-check the marker's derived identity.
    let blob = transport.read_blob(MANIFEST_PATH, Some(&head.commit))?;
    let bytes = blob.bytes()?;
    let digest = crate::state_broker::digest::sha256_hex(&bytes);
    if digest != marker.manifest_sha256 {
        return Err(StateBrokerError::local_io(
            "the current manifest no longer matches the projection marker",
        ));
    }
    let manifest = super::manifest::CheckpointManifestV1::from_slice(&bytes)
        .map_err(|e| StateBrokerError::local_io(format!("current manifest is unreadable: {e}")))?;
    if manifest.op_id != marker.op_id
        || manifest.source.state_sha256 != marker.state_sha256
        || manifest.source.watermark != marker.watermark
    {
        return Err(StateBrokerError::local_io(
            "the current manifest identity disagrees with the projection marker",
        ));
    }
    if let Some(min) = &expect.min_watermark {
        if marker.watermark < *min {
            return Err(StateBrokerError::local_io(format!(
                "projection watermark {}/{} is older than the required minimum",
                marker.watermark.agent_id, marker.watermark.agent_seq
            )));
        }
    }
    // Verify the projected file.
    let target = dir.join(PROJECTED_STATE_PATH);
    let projected = std::fs::read(&target).map_err(|e| {
        StateBrokerError::local_io(format!("projected state {} is unreadable: {e}", target.display()))
    })?;
    if projected.len() as u64 != marker.bytes
        || crate::state_broker::digest::sha256_hex(&projected) != marker.state_sha256
    {
        return Err(StateBrokerError::local_io(
            "projected state failed its size/digest check",
        ));
    }
    // Provenance gate: authoritative hydration requires a journal anchor.
    if expect.journal_anchored {
        if marker.provenance != Provenance::JournalAnchored.label() {
            return Err(StateBrokerError::local_io(format!(
                "projection in {} is {:?}, not journal_anchored; refusing authoritative hydration",
                dir.display(),
                marker.provenance
            )));
        }
        let anchor = anchor.ok_or_else(|| {
            StateBrokerError::local_io(
                "journal anchoring was required but no anchor was supplied",
            )
        })?;
        anchor
            .verify_anchor(&manifest.source, &projected)
            .map_err(|e| StateBrokerError::local_io(e.message().to_string()))?;
    }
    Ok(marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_file_is_not_a_broker_path() {
        assert!(crate::state_broker::validate::validate_logical_path(PROJECTION_MARKER_FILE)
            .is_err());
        assert_eq!(PROJECTED_STATE_PATH, "checkpoint/state.json");
    }
}
