//! The narrow transport seam between Crosslink and a durable project-state
//! backend.
//!
//! # Why this trait exists
//!
//! Recon of the existing persistence boundaries found no single abstraction
//! spanning durable-state *read* and *mutation*:
//!
//! - [`crate::hub_source::HubSource`] is read-only (compaction input);
//! - [`crate::hub_v3`] owns CAS ref writes but is git-plumbing specific and
//!   per-agent-ref shaped;
//! - [`crate::sync::SyncManager`] owns remote fetch/push but is the git
//!   transport itself.
//!
//! This trait is the minimum change: it names the five semantic operations a
//! durable state backend must provide, with exactly the broker v1 contract's
//! semantics (expected-head CAS, typed `stale_state`, read-back verification).
//! The existing local/direct git behavior is untouched; adopting the broker is
//! opt-in (see [`crate::state_broker::config::StateBackend`]).
//!
//! # Implementing backends
//!
//! - [`crate::state_broker::client::StateBrokerClient`] — the deployed broker.
//! - [`crate::state_broker::mock::MockStateTransport`] — deterministic
//!   in-memory broker with identical CAS semantics, for tests and dry runs.
//!
//! # Local projections are disposable
//!
//! [`ProjectStateTransport::hydrate_into`] materializes durable state blobs
//! into a caller-chosen directory. That directory is a **cache**: it is never
//! the source of truth, it is not committed, and deleting it loses nothing.
//! The durable head is always re-read from the backend.

use std::path::Path;

use super::client::{
    CommitOutcome, CommitRequest, ProjectState, StateBlob, StateBrokerClient, VerifiedEntry,
    VerifiedFile,
};
use super::config::StateBrokerConfig;
// `BrokerErrorCode` is referenced by intra-doc links below; imported for
// rustdoc even though no code in this module names it.
#[allow(unused_imports)]
use super::error::BrokerErrorCode;
use super::error::StateBrokerError;
use super::validate::{projection_relative_path, validate_logical_path};

/// Outcome of materializing durable state into a disposable local directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionReport {
    /// Commit the projection was materialized from (`None` when the project has
    /// no durable state yet, in which case nothing was written).
    pub commit: Option<String>,
    /// Root directory the files were written under.
    pub root: std::path::PathBuf,
    /// Logical paths written, in hydration order.
    pub files: Vec<String>,
    /// Total bytes written.
    pub bytes: u64,
}

/// Outcome of a reconciled compare-and-swap commit.
#[derive(Debug, Clone)]
pub struct CasResolution {
    /// The broker commit outcome (the landed commit).
    pub outcome: CommitOutcome,
    /// `true` when a `stale_state` conflict was resolved by discovering that
    /// *our own* op id already produced the current head — i.e. the write had
    /// already landed and nothing new was written.
    pub already_applied: bool,
    /// Number of broker `commit` attempts made (1 = no conflict).
    pub attempts: u8,
}

/// Semantic durable-state operations Crosslink requires from a state backend.
///
/// All methods are synchronous: the broker client uses a blocking HTTP client,
/// matching Crosslink's CLI (and `sync`) execution model.
pub trait ProjectStateTransport {
    /// Current durable state: ref, head commit, file inventory, baseline.
    ///
    /// # Errors
    ///
    /// Transport/protocol failures, or a typed broker failure.
    fn read_state(&self) -> Result<ProjectState, StateBrokerError>;

    /// Hydrate one state file, at `at` (an exact commit sha) or the state head
    /// when `at` is `None`.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for a bad path/ref;
    /// [`BrokerErrorCode::NotFound`] when the file does not exist at that
    /// commit; plus transport/protocol failures.
    fn read_blob(&self, path: &str, at: Option<&str>) -> Result<StateBlob, StateBrokerError>;

    /// Read back digests for `paths` at an exact commit.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for a bad commit/paths, plus
    /// transport/protocol failures.
    fn verify(
        &self,
        commit: &str,
        paths: &[String],
    ) -> Result<Vec<VerifiedEntry>, StateBrokerError>;

    /// Submit one compare-and-swap mutation. Never blind-retried.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::StaleState`] when `expected_head` does not match the
    /// observed head (nothing was written); `upstream_error` with a commit sha
    /// in `details` when the broker's read-back disagreed; plus
    /// transport/protocol failures.
    fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, StateBrokerError>;

    /// The current durable head commit, or `None` when the project has no
    /// durable state yet.
    ///
    /// # Errors
    ///
    /// See [`Self::read_state`].
    fn current_head(&self) -> Result<Option<String>, StateBrokerError> {
        Ok(self.read_state()?.state.head.map(|head| head.commit))
    }

    /// Materialize durable state files into `dir` (a disposable projection).
    ///
    /// `paths = None` hydrates the full inventory; `Some` hydrates exactly the
    /// named paths (each validated). A project with no durable state writes
    /// nothing and returns a report with `commit: None`.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for unsafe paths,
    /// [`BrokerErrorCode::LocalIo`] when writing fails, plus the read errors of
    /// [`Self::read_blob`].
    fn hydrate_into(
        &self,
        dir: &Path,
        paths: Option<&[String]>,
    ) -> Result<ProjectionReport, StateBrokerError> {
        let state = self.read_state()?;
        let Some(head) = state.state.head else {
            return Ok(ProjectionReport {
                commit: None,
                root: dir.to_path_buf(),
                files: Vec::new(),
                bytes: 0,
            });
        };

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
            None => state
                .state
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect(),
        };

        std::fs::create_dir_all(dir).map_err(|e| {
            StateBrokerError::local_io(format!(
                "cannot create projection directory {}: {e}",
                dir.display()
            ))
        })?;

        let mut report = ProjectionReport {
            commit: Some(head.commit.clone()),
            root: dir.to_path_buf(),
            files: Vec::with_capacity(selected.len()),
            bytes: 0,
        };
        for path in selected {
            let blob = self.read_blob(&path, Some(&head.commit))?;
            let bytes = blob.bytes()?;
            let relative = projection_relative_path(&path)?;
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
            report.bytes += bytes.len() as u64;
            report.files.push(path);
        }
        Ok(report)
    }

    /// Submit a mutation through the expected-head/CAS path, reconciling at
    /// most `max_retries` `stale_state` conflicts.
    ///
    /// Reconciliation is deliberately conservative:
    ///
    /// 1. On `stale_state`, re-read the durable head once.
    /// 2. If the head commit's trailers record **our own `op_id`**, the write
    ///    already landed: verify our paths at that head and return
    ///    [`CasResolution::already_applied`] without writing again.
    /// 3. Otherwise re-issue with the freshly observed head.
    ///
    /// This is only safe for whole-file upserts (the request content is
    /// idempotent); append-style mutations must use [`Self::commit`] directly
    /// and reconcile explicitly. `op_id` is required.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] when `op_id` is absent, plus the
    /// errors of [`Self::read_state`], [`Self::verify`], and [`Self::commit`]
    /// (typically the final `stale_state` when retries are exhausted).
    fn commit_cas(
        &self,
        request: &CommitRequest,
        max_retries: u8,
    ) -> Result<CasResolution, StateBrokerError> {
        let Some(op_id) = request.op_id.clone() else {
            return Err(StateBrokerError::invalid_input(
                "commit_cas requires op_id so a stale_state conflict can be reconciled; \
                 use commit() for a single unretried attempt",
            ));
        };

        let mut expected_head = request.expected_head.clone();
        let mut attempt: u8 = 0;
        loop {
            let mut current = request.clone();
            current.expected_head.clone_from(&expected_head);
            match self.commit(&current) {
                Ok(outcome) => {
                    return Ok(CasResolution {
                        outcome,
                        already_applied: false,
                        attempts: attempt.saturating_add(1),
                    });
                }
                Err(error) if error.is_stale_state() && attempt < max_retries => {
                    let state = self.read_state()?;
                    let head = state.state.head.clone();
                    if let Some(head) = &head {
                        if message_records_op(&head.message, &op_id) {
                            // The op-id trailer says this head came from our
                            // operation, but only a content comparison proves
                            // the head still carries the payload we intended:
                            // a reused op id (or any later overwrite) must not
                            // be reported as a verified write.
                            let intended: std::collections::HashMap<&str, (String, u64)> = request
                                .files
                                .iter()
                                .map(|file| {
                                    (
                                        file.path.as_str(),
                                        (
                                            super::digest::sha256_hex(&file.content),
                                            file.content.len() as u64,
                                        ),
                                    )
                                })
                                .collect();
                            let entries = self.verify(&head.commit, &request.paths())?;
                            let files: Vec<VerifiedFile> = entries
                                .into_iter()
                                .map(|entry| {
                                    let verified = intended.get(entry.path.as_str()).is_some_and(
                                        |(expected_sha, expected_size)| {
                                            entry.sha256.as_deref() == Some(expected_sha.as_str())
                                                && entry.size == Some(*expected_size)
                                        },
                                    );
                                    VerifiedFile {
                                        path: entry.path,
                                        blob_sha: entry.blob_sha,
                                        sha256: entry.sha256,
                                        size: entry.size,
                                        verified,
                                    }
                                })
                                .collect();
                            let verified = files.iter().all(|file| file.verified);
                            return Ok(CasResolution {
                                outcome: CommitOutcome {
                                    state_ref: state.state.state_ref,
                                    commit: head.commit.clone(),
                                    previous_head: None,
                                    head_after: Some(head.commit.clone()),
                                    message: head.message.clone(),
                                    op_id: Some(op_id),
                                    files,
                                    verified,
                                },
                                already_applied: true,
                                attempts: attempt.saturating_add(1),
                            });
                        }
                    }
                    expected_head = head.map(|head| head.commit);
                    attempt = attempt.saturating_add(1);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// Whether `message`'s trailer block records `op_id` (`Broker-Op: <op_id>`).
///
/// The broker owns the trailer block, so a caller message can never spoof it.
#[must_use]
pub fn message_records_op(message: &str, op_id: &str) -> bool {
    let expected = format!("Broker-Op: {op_id}");
    message
        .lines()
        .any(|line| line.trim_end().trim_start() == expected)
}

impl ProjectStateTransport for StateBrokerClient {
    fn read_state(&self) -> Result<ProjectState, StateBrokerError> {
        // Inherent method (HTTP call) — inherent methods take precedence here.
        self.state()
    }

    fn read_blob(&self, path: &str, at: Option<&str>) -> Result<StateBlob, StateBrokerError> {
        self.read_blob(path, at)
    }

    fn verify(
        &self,
        commit: &str,
        paths: &[String],
    ) -> Result<Vec<VerifiedEntry>, StateBrokerError> {
        Ok(self.verify(commit, paths)?.entries)
    }

    fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, StateBrokerError> {
        self.commit(request)
    }
}

/// Build a broker transport from the environment.
///
/// Returns `None` when no broker variable is present at all.
///
/// # Errors
///
/// [`BrokerErrorCode::Configuration`] for a partial/invalid environment, or
/// when the HTTP client cannot be constructed.
pub fn transport_from_env() -> Result<Option<StateBrokerClient>, StateBrokerError> {
    StateBrokerConfig::from_env()?.map_or_else(
        || Ok(None),
        |config| StateBrokerClient::new(config).map(Some),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_trailer_detection_is_line_exact() {
        let message =
            "checkpoint: probe\n\nProject-UUID: 1d440dcf-bcbf-4d1a-987c-d5334568a716\nBroker: crosslink-state-broker/0.1.0\nBroker-Op: codex-1\n";
        assert!(message_records_op(message, "codex-1"));
        assert!(!message_records_op(message, "codex-2"));
        // A caller message mentioning the trailer text is not a trailer line.
        assert!(!message_records_op("note: Broker-Op: codex-1", "codex-1"));
        assert!(!message_records_op("Broker-Op: codex-10", "codex-1"));
    }
}
