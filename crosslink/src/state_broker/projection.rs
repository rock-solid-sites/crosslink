//! Disposable local projections of durable broker state.
//!
//! A projection is a *cache*: it is never the source of truth. To make that
//! property enforceable rather than conventional, every projection written by
//! [`hydrate`] carries a marker file ([`PROJECTION_MARKER_FILE`]) recording
//!
//! - the broker project identity and state ref the projection came from
//!   (backend/project binding),
//! - the durable head commit it was materialized from, and
//! - the exact logical paths, sizes, and SHA-256 digests written, plus a
//!   `complete` flag that is only set after every file is on disk.
//!
//! [`verify_projection`] re-reads the marker, re-reads the durable head, and
//! fails closed when the projection is missing, incomplete, belongs to another
//! project, or is stale relative to the current head. A stale or partial
//! projection therefore cannot masquerade as authoritative/current state.
//!
//! The marker file name deliberately contains `+`, a character the broker's
//! path grammar rejects, so a marker can never collide with a broker state
//! path (`unittest` below asserts this).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::client::{StateBlob, StateEntry};
use super::error::StateBrokerError;
use super::transport::{ProjectStateTransport, ProjectionReport};
use super::validate::{projection_relative_path, validate_logical_path};

/// Marker file written beside a hydrated projection.
pub const PROJECTION_MARKER_FILE: &str = ".crosslink-state-projection+v1.json";

/// Schema identifier recorded in [`ProjectionMarker::schema`].
pub const PROJECTION_MARKER_SCHEMA: &str = "crosslink-state-projection/v1";

/// One projected file's identity, recorded in the projection marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionFile {
    /// Logical broker path.
    pub path: String,
    /// SHA-256 of the projected bytes (the value verified before writing).
    pub sha256: String,
    /// Byte size of the projected file.
    pub size: u64,
}

/// On-disk identity/freshness record for a hydrated projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionMarker {
    /// Marker schema; see [`PROJECTION_MARKER_SCHEMA`].
    pub schema: String,
    /// Broker project UUID the projection came from.
    pub project_uuid: String,
    /// Durable state ref the projection came from.
    pub state_ref: String,
    /// Durable head commit the projection was materialized from.
    pub head_commit: String,
    /// `false` until every file has been written. An interrupted hydration
    /// leaves the marker incomplete and consumers must refuse it.
    pub complete: bool,
    /// Projected files with their verified digests.
    pub files: Vec<ProjectionFile>,
    /// Total bytes written.
    pub bytes: u64,
}

impl ProjectionMarker {
    /// Whether every projected file is present with the recorded size and
    /// digest.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerErrorCode::LocalIo`] for a missing, unreadable, sized,
    /// or digest-mismatched file.
    ///
    /// [`BrokerErrorCode::LocalIo`]: super::error::BrokerErrorCode::LocalIo
    pub fn verify_files(&self, dir: &Path) -> Result<(), StateBrokerError> {
        for file in &self.files {
            let relative = projection_relative_path(&file.path)?;
            let target = dir.join(relative);
            let bytes = std::fs::read(&target).map_err(|e| {
                StateBrokerError::local_io(format!(
                    "projected file {} is missing or unreadable: {e}",
                    target.display()
                ))
            })?;
            if bytes.len() as u64 != file.size {
                return Err(StateBrokerError::local_io(format!(
                    "projected file {} has size {} but the marker records {}",
                    target.display(),
                    bytes.len(),
                    file.size
                )));
            }
            let digest = super::digest::sha256_hex(&bytes);
            if digest != file.sha256 {
                return Err(StateBrokerError::local_io(format!(
                    "projected file {} does not match the marker digest",
                    target.display()
                )));
            }
        }
        Ok(())
    }
}

/// Read the projection marker in `dir`, if one exists.
///
/// A present but unreadable/unparsable marker is a hard
/// [`BrokerErrorCode::LocalIo`] failure — it is *not* treated as "no marker",
/// because that would let a corrupted marker silently downgrade the
/// projection to an unidentifiable directory.
///
/// [`BrokerErrorCode::LocalIo`]: super::error::BrokerErrorCode::LocalIo
///
/// # Errors
///
/// See above.
pub fn read_projection_marker(dir: &Path) -> Result<Option<ProjectionMarker>, StateBrokerError> {
    let path = dir.join(PROJECTION_MARKER_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(StateBrokerError::local_io(format!(
                "cannot read projection marker {}: {error}",
                path.display()
            )));
        }
    };
    let marker: ProjectionMarker = serde_json::from_str(&raw).map_err(|error| {
        StateBrokerError::local_io(format!(
            "projection marker {} is not valid JSON for schema {PROJECTION_MARKER_SCHEMA}: {error}",
            path.display()
        ))
    })?;
    Ok(Some(marker))
}

/// Write `marker` into `dir` atomically (temp file + rename).
fn write_projection_marker(dir: &Path, marker: &ProjectionMarker) -> Result<(), StateBrokerError> {
    let target = dir.join(PROJECTION_MARKER_FILE);
    let temp = dir.join(format!("{PROJECTION_MARKER_FILE}.tmp"));
    let body = serde_json::to_vec_pretty(marker).map_err(|e| {
        StateBrokerError::local_io(format!("cannot serialize the projection marker: {e}"))
    })?;
    std::fs::write(&temp, &body).map_err(|e| {
        StateBrokerError::local_io(format!(
            "cannot write projection marker {}: {e}",
            temp.display()
        ))
    })?;
    // `rename` does not replace an existing file on Windows; remove first.
    if target.exists() {
        std::fs::remove_file(&target).map_err(|e| {
            StateBrokerError::local_io(format!(
                "cannot replace projection marker {}: {e}",
                target.display()
            ))
        })?;
    }
    std::fs::rename(&temp, &target).map_err(|e| {
        StateBrokerError::local_io(format!(
            "cannot finalize projection marker {}: {e}",
            target.display()
        ))
    })
}

/// Materialize durable state files into `dir` (a disposable projection).
///
/// See [`crate::state_broker::ProjectStateTransport::hydrate_into`], whose
/// default implementation delegates here.
pub(super) fn hydrate<T>(
    transport: &T,
    dir: &Path,
    paths: Option<&[String]>,
) -> Result<ProjectionReport, StateBrokerError>
where
    T: ProjectStateTransport + ?Sized,
{
    let state = transport.read_state()?;
    let Some(head) = state.state.head else {
        // Nothing durable to project: write nothing, not even a marker.
        return Ok(ProjectionReport {
            commit: None,
            root: dir.to_path_buf(),
            files: Vec::new(),
            bytes: 0,
            marker: None,
        });
    };

    let inventory: BTreeMap<&str, &StateEntry> = state
        .state
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();

    let selected: Vec<String> = match paths {
        Some([]) => {
            return Err(StateBrokerError::invalid_input(
                "hydrate path list must not be empty (use None for the full inventory)",
            ));
        }
        Some(list) => {
            for path in list {
                validate_logical_path(path)?;
            }
            list.to_vec()
        }
        None => inventory.keys().map(|path| (*path).to_string()).collect(),
    };

    let previous = read_projection_marker(dir)?;
    if let Some(previous) = &previous {
        if previous.project_uuid != state.project.uuid
            || previous.state_ref != state.state.state_ref
        {
            return Err(StateBrokerError::identity_mismatch(format!(
                "projection directory {} belongs to project {} ({}), not {}/{}",
                dir.display(),
                previous.project_uuid,
                previous.state_ref,
                state.project.uuid,
                state.state.state_ref,
            )));
        }
    }

    std::fs::create_dir_all(dir).map_err(|e| {
        StateBrokerError::local_io(format!(
            "cannot create projection directory {}: {e}",
            dir.display()
        ))
    })?;

    // Invalidate the previous marker *before* touching files: an interrupted
    // hydration must never leave a complete-looking projection.
    let mut marker = ProjectionMarker {
        schema: PROJECTION_MARKER_SCHEMA.to_string(),
        project_uuid: state.project.uuid.clone(),
        state_ref: state.state.state_ref.clone(),
        head_commit: head.commit.clone(),
        complete: false,
        files: Vec::with_capacity(selected.len()),
        bytes: 0,
    };
    write_projection_marker(dir, &marker)?;

    let mut files = Vec::with_capacity(selected.len());
    let mut total: u64 = 0;
    for path in &selected {
        let blob = transport.read_blob(path, Some(&head.commit))?;
        check_blob(&blob, path, &head.commit, inventory.get(path.as_str()).copied())?;
        // `bytes()` verifies the envelope digest/size before anything is written.
        let bytes = blob.bytes()?;
        let relative = projection_relative_path(path)?;
        let target = dir.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                StateBrokerError::local_io(format!(
                    "cannot create projection directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(&target, &bytes).map_err(|e| {
            StateBrokerError::local_io(format!(
                "cannot write projection file {}: {e}",
                target.display()
            ))
        })?;
        total += bytes.len() as u64;
        files.push(ProjectionFile {
            path: path.clone(),
            sha256: blob.sha256.clone(),
            size: blob.size,
        });
    }

    // Drop files that belonged to a previous projection but are not part of
    // this one, so the directory matches the marker manifest exactly.
    if let Some(previous) = &previous {
        let keep: HashSet<&str> = selected.iter().map(String::as_str).collect();
        for stale in &previous.files {
            if keep.contains(stale.path.as_str()) {
                continue;
            }
            let relative = projection_relative_path(&stale.path)?;
            let target = dir.join(relative);
            if target.is_file() {
                std::fs::remove_file(&target).map_err(|e| {
                    StateBrokerError::local_io(format!(
                        "cannot remove stale projection file {}: {e}",
                        target.display()
                    ))
                })?;
            }
        }
    }

    marker.files = files;
    marker.bytes = total;
    marker.complete = true;
    write_projection_marker(dir, &marker)?;

    Ok(ProjectionReport {
        commit: Some(head.commit),
        root: dir.to_path_buf(),
        files: selected,
        bytes: total,
        marker: Some(marker),
    })
}

/// Cross-check one blob read against the requested path, the pinned commit,
/// and (when listed) the durable inventory entry.
fn check_blob(
    blob: &StateBlob,
    requested_path: &str,
    pinned_commit: &str,
    entry: Option<&StateEntry>,
) -> Result<(), StateBrokerError> {
    if blob.path != requested_path {
        return Err(StateBrokerError::protocol(format!(
            "blob response path mismatch: requested {requested_path:?}, got {:?}",
            blob.path
        )));
    }
    if blob.commit != pinned_commit {
        return Err(StateBrokerError::protocol(format!(
            "blob response for {requested_path:?} was read at {} but the projection is pinned to {pinned_commit}",
            blob.commit
        )));
    }
    if let Some(entry) = entry {
        if blob.blob_sha != entry.blob_sha {
            return Err(StateBrokerError::protocol(format!(
                "blob sha mismatch for {requested_path:?}: inventory says {}, blob read says {}",
                entry.blob_sha, blob.blob_sha
            )));
        }
        if let Some(size) = entry.size {
            if size != blob.size {
                return Err(StateBrokerError::protocol(format!(
                    "blob size mismatch for {requested_path:?}: inventory says {size}, blob read says {}",
                    blob.size
                )));
            }
        }
    }
    Ok(())
}

/// Verify that `dir` holds a complete projection whose identity and head match
/// the transport's current durable state.
///
/// Consumer-facing entry point: any code that is about to treat a projected
/// directory as current durable state must call this first. On success the
/// projection is complete, belongs to this project, and was materialized from
/// the current head; otherwise this fails closed with
/// [`BrokerErrorCode::LocalIo`] (missing/incomplete/stale/unreadable) or
/// [`BrokerErrorCode::IdentityMismatch`] (another project).
///
/// [`BrokerErrorCode::LocalIo`]: super::error::BrokerErrorCode::LocalIo
/// [`BrokerErrorCode::IdentityMismatch`]: super::error::BrokerErrorCode::IdentityMismatch
///
/// # Errors
///
/// See above; plus the read errors of [`ProjectStateTransport::read_state`].
pub(super) fn verify_projection<T>(
    transport: &T,
    dir: &Path,
) -> Result<ProjectionMarker, StateBrokerError>
where
    T: ProjectStateTransport + ?Sized,
{
    let marker = read_projection_marker(dir)?.ok_or_else(|| {
        StateBrokerError::local_io(format!(
            "{} is not a broker projection (no {PROJECTION_MARKER_FILE}); \
             refusing to treat it as durable state",
            dir.display()
        ))
    })?;
    if marker.schema != PROJECTION_MARKER_SCHEMA {
        return Err(StateBrokerError::local_io(format!(
            "projection marker schema {:?} is not {PROJECTION_MARKER_SCHEMA}",
            marker.schema
        )));
    }
    if !marker.complete {
        return Err(StateBrokerError::local_io(format!(
            "projection in {} is incomplete (hydration was interrupted); \
             re-hydrate before using it",
            dir.display()
        )));
    }

    let state = transport.read_state()?;
    if marker.project_uuid != state.project.uuid || marker.state_ref != state.state.state_ref {
        return Err(StateBrokerError::identity_mismatch(format!(
            "projection in {} belongs to project {} ({}), but the durable state is {}/{}",
            dir.display(),
            marker.project_uuid,
            marker.state_ref,
            state.project.uuid,
            state.state.state_ref,
        )));
    }
    let current_head = state.state.head.as_ref().map(|head| head.commit.as_str());
    if Some(marker.head_commit.as_str()) != current_head {
        return Err(StateBrokerError::local_io(format!(
            "projection in {} is stale: marker records head {}, durable head is {}",
            dir.display(),
            marker.head_commit,
            current_head.unwrap_or("(none)"),
        )));
    }
    marker.verify_files(dir)?;
    Ok(marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_broker::validate::validate_logical_path;

    /// The marker file must not be reachable as a broker state path.
    #[test]
    fn marker_file_is_not_a_broker_legal_path() {
        assert!(
            validate_logical_path(PROJECTION_MARKER_FILE).is_err(),
            "the marker name must be outside the broker path grammar"
        );
    }

    fn blob_for(path: &str, commit: &str, content: &[u8]) -> StateBlob {
        use base64::Engine as _;
        StateBlob {
            path: path.to_string(),
            requested_ref: "state".to_string(),
            commit: commit.to_string(),
            blob_sha: crate::state_broker::digest::sha256_hex(content),
            sha256: crate::state_broker::digest::sha256_hex(content),
            size: content.len() as u64,
            content_base64: base64::engine::general_purpose::STANDARD.encode(content),
        }
    }

    /// The inventory↔blob cross-check must reject a broker that swaps content
    /// or answers for another path/commit.
    #[test]
    fn blob_inventory_cross_checks_reject_mismatches() {
        let commit = "c".repeat(40);
        let blob = blob_for("a/b.json", &commit, b"one");
        let entry = StateEntry {
            path: "a/b.json".to_string(),
            blob_sha: blob.blob_sha.clone(),
            size: Some(blob.size),
        };
        check_blob(&blob, "a/b.json", &commit, Some(&entry)).expect("consistent blob");

        let mut wrong_path = blob.clone();
        wrong_path.path = "other.json".to_string();
        assert!(check_blob(&wrong_path, "a/b.json", &commit, Some(&entry)).is_err());

        let mut wrong_commit = blob.clone();
        wrong_commit.commit = "d".repeat(40);
        assert!(check_blob(&wrong_commit, "a/b.json", &commit, Some(&entry)).is_err());

        let mut wrong_blob_sha = blob.clone();
        wrong_blob_sha.blob_sha = "e".repeat(40);
        assert!(check_blob(&wrong_blob_sha, "a/b.json", &commit, Some(&entry)).is_err());

        let mut wrong_size = blob.clone();
        wrong_size.size = blob.size + 1;
        assert!(check_blob(&wrong_size, "a/b.json", &commit, Some(&entry)).is_err());

        // The blob's self-digest is still enforced by `bytes()`.
        let mut wrong_digest = blob.clone();
        wrong_digest.sha256 = "f".repeat(64);
        assert!(wrong_digest.bytes().is_err());
    }

    #[test]
    fn marker_file_digests_detect_local_tampering() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.json"), b"one").unwrap();
        let marker = ProjectionMarker {
            schema: PROJECTION_MARKER_SCHEMA.to_string(),
            project_uuid: "1d440dcf-bcbf-4d1a-987c-d5334568a716".to_string(),
            state_ref: "refs/heads/projects/x/state".to_string(),
            head_commit: "c".repeat(40),
            complete: true,
            files: vec![ProjectionFile {
                path: "a.json".to_string(),
                sha256: crate::state_broker::digest::sha256_hex(b"one"),
                size: 3,
            }],
            bytes: 3,
        };
        marker.verify_files(dir.path()).expect("intact projection");

        std::fs::write(dir.path().join("a.json"), b"two").unwrap();
        assert!(marker.verify_files(dir.path()).is_err());
    }
}
