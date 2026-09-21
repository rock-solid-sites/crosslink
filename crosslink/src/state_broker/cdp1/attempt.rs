//! CDP-1 local write-ahead attempt record (spec Appendix C).
//!
//! The record is local and non-authoritative. It exists so a crashed or
//! ambiguous publish can be reconciled by `op_id` before any new write, and so
//! a blocked/diverged state survives process restarts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::SOURCE_STATE_PATH;
use crate::events::OrderingKey;
use crate::state_broker::error::StateBrokerError;

/// Attempt phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptPhase {
    /// Plan + op id persisted, no write attempted.
    Prepared,
    /// A commit attempt is in flight (or its outcome is unknown).
    InFlight,
    /// The attempt has a terminal resolution.
    Resolved,
}

impl AttemptPhase {
    /// Stable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::InFlight => "in_flight",
            Self::Resolved => "resolved",
        }
    }

    /// Parse a label.
    #[must_use]
    pub fn from_label(value: &str) -> Option<Self> {
        match value {
            "prepared" => Some(Self::Prepared),
            "in_flight" => Some(Self::InFlight),
            "resolved" => Some(Self::Resolved),
            _ => None,
        }
    }
}

/// Terminal resolution of an attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptResolution {
    /// Outcome label (`landed`, `landed_superseded`, `already_current`,
    /// `not_landed`, `refused`, `diverged`, `unknown`, `failed_closed`).
    pub outcome: String,
    /// Landed/observed commit, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Stable reason label, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Local timestamp (non-authoritative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// Source provenance copied into the attempt record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptSource {
    /// Pushed checkpoint commit.
    pub commit: String,
    /// Git tree path inside the checkpoint.
    pub state_path: String,
    /// Git blob sha.
    pub state_blob_sha: String,
    /// SHA-256 of the uncompressed state.
    pub state_sha256: String,
    /// Watermark.
    pub watermark: OrderingKey,
}

/// The on-disk attempt record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// Schema id.
    pub schema: String,
    /// Broker project UUID.
    pub project_uuid: String,
    /// Publisher identity.
    pub publisher_id: String,
    /// Broker op id.
    pub op_id: String,
    /// Phase label.
    pub phase: String,
    /// Source provenance.
    pub source: AttemptSource,
    /// Compressed payload length.
    pub payload_bytes: u64,
    /// SHA-256 of the compressed payload.
    pub payload_sha256: String,
    /// SHA-256 of the canonical manifest bytes.
    pub manifest_sha256: String,
    /// Active chunk count.
    pub chunk_count: u32,
    /// Expected head at the first attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_head: Option<String>,
    /// Terminal resolution, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<AttemptResolution>,
}

impl AttemptRecord {
    /// Build a `prepared` record.
    #[must_use]
    pub fn prepared(
        project_uuid: impl Into<String>,
        publisher_id: impl Into<String>,
        op_id: impl Into<String>,
        source: AttemptSource,
        payload_bytes: u64,
        payload_sha256: impl Into<String>,
        manifest_sha256: impl Into<String>,
        chunk_count: u32,
        expected_head: Option<String>,
    ) -> Self {
        Self {
            schema: super::ATTEMPT_SCHEMA.to_string(),
            project_uuid: project_uuid.into(),
            publisher_id: publisher_id.into(),
            op_id: op_id.into(),
            phase: AttemptPhase::Prepared.label().to_string(),
            source,
            payload_bytes,
            payload_sha256: payload_sha256.into(),
            manifest_sha256: manifest_sha256.into(),
            chunk_count,
            expected_head,
            resolution: None,
        }
    }

    /// Whether the attempt is unresolved (crash recovery required).
    #[must_use]
    pub fn is_unresolved(&self) -> bool {
        self.phase != AttemptPhase::Resolved.label()
    }

    /// Whether this record blocks new publishes.
    ///
    /// `unknown` blocks until a successful reconcile; `diverged` is terminal.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        match &self.resolution {
            Some(resolution) => {
                matches!(resolution.outcome.as_str(), "unknown" | "diverged")
                    || self.is_unresolved()
            }
            None => self.is_unresolved(),
        }
    }

    /// The source ref name recorded for this protocol.
    #[must_use]
    pub fn source_ref(&self) -> &'static str {
        super::SOURCE_REF
    }

    /// The source state path recorded for this protocol.
    #[must_use]
    pub fn state_path(&self) -> &str {
        if self.source.state_path.is_empty() {
            SOURCE_STATE_PATH
        } else {
            &self.source.state_path
        }
    }
}

/// Atomic local store for the single in-flight attempt record.
#[derive(Debug, Clone)]
pub struct AttemptStore {
    path: PathBuf,
}

impl AttemptStore {
    /// Store at `.crosslink/state-broker/publish-attempt.json`.
    #[must_use]
    pub fn new(crosslink_dir: &Path) -> Self {
        Self {
            path: crosslink_dir.join(super::ATTEMPT_REL_PATH),
        }
    }

    /// Store at an explicit path (tests).
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The record path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the record, if present.
    ///
    /// # Errors
    ///
    /// Returns a local-IO error when the file exists but is unreadable or
    /// malformed — a corrupt attempt record must fail closed, never be ignored.
    pub fn load(&self) -> Result<Option<AttemptRecord>, StateBrokerError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(StateBrokerError::local_io(format!(
                    "cannot read attempt record {}: {error}",
                    self.path.display()
                )));
            }
        };
        let record: AttemptRecord = serde_json::from_str(&raw).map_err(|e| {
            StateBrokerError::local_io(format!(
                "attempt record {} is malformed: {e}",
                self.path.display()
            ))
        })?;
        if record.schema != super::ATTEMPT_SCHEMA {
            return Err(StateBrokerError::local_io(format!(
                "attempt record {} has schema {:?}, expected {:?}",
                self.path.display(),
                record.schema,
                super::ATTEMPT_SCHEMA
            )));
        }
        Ok(Some(record))
    }

    /// Atomically write the record.
    ///
    /// # Errors
    ///
    /// Returns a local-IO error when the write fails.
    pub fn save(&self, record: &AttemptRecord) -> Result<(), StateBrokerError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                StateBrokerError::local_io(format!(
                    "cannot create attempt-record directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let bytes = serde_json::to_vec_pretty(record).map_err(|e| {
            StateBrokerError::local_io(format!("cannot serialize the attempt record: {e}"))
        })?;
        crate::utils::atomic_write(&self.path, &bytes).map_err(|e| {
            StateBrokerError::local_io(format!(
                "cannot write attempt record {}: {e}",
                self.path.display()
            ))
        })
    }

    /// Remove the record (used after a clean, resolved attempt).
    ///
    /// # Errors
    ///
    /// Returns a local-IO error when the file exists but cannot be removed.
    pub fn clear(&self) -> Result<(), StateBrokerError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StateBrokerError::local_io(format!(
                "cannot remove attempt record {}: {error}",
                self.path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn source() -> AttemptSource {
        AttemptSource {
            commit: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string(),
            state_path: SOURCE_STATE_PATH.to_string(),
            state_blob_sha: "b2c3d4e5f60718293a4b5c6d7e8f901234567890".to_string(),
            state_sha256: "c".repeat(64),
            watermark: OrderingKey {
                timestamp: DateTime::<Utc>::UNIX_EPOCH,
                agent_id: "driver".to_string(),
                agent_seq: 7,
            },
        }
    }

    #[test]
    fn round_trip_and_blocking_rules() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::new(dir.path());
        let mut record = AttemptRecord::prepared(
            "7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f",
            "test-publisher",
            "ckpt-a1b2c3d4e5f6-0123456789abcdef",
            source(),
            10,
            "d".repeat(64),
            "e".repeat(64),
            1,
            None,
        );
        store.save(&record).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded, record);
        assert!(loaded.is_unresolved());
        assert!(loaded.is_blocking());

        record.phase = AttemptPhase::Resolved.label().to_string();
        record.resolution = Some(AttemptResolution {
            outcome: "landed".to_string(),
            commit: Some("f".repeat(40)),
            reason: None,
            at: None,
        });
        store.save(&record).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert!(!loaded.is_unresolved());
        assert!(!loaded.is_blocking());

        // unknown blocks, diverged is terminal.
        for outcome in ["unknown", "diverged"] {
            let mut blocking = loaded.clone();
            blocking.resolution.as_mut().unwrap().outcome = outcome.to_string();
            assert!(blocking.is_blocking(), "{outcome} must block");
        }

        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn corrupt_record_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::new(dir.path());
        std::fs::create_dir_all(dir.path().join("state-broker")).unwrap();
        std::fs::write(store.path(), b"{not json").unwrap();
        assert!(store.load().is_err());
    }
}
