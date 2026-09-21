//! Deterministic in-memory broker with the same semantics as the deployed
//! service, for tests and dry runs.
//!
//! The mock enforces the broker contract's observable rules: input validation
//! (shared with the real client), expected-head compare-and-swap with
//! [`BrokerErrorCode::StaleState`] and nothing written on conflict, broker-owned
//! trailers (`Project-UUID:`, `Broker:`, `Broker-Op:`) in commit messages, and
//! per-file SHA-256 read-back.
//!
//! It intentionally does not model multi-process concurrency; tests use
//! [`MockStateTransport::inject_competing_commit`] to simulate a competing
//! writer landing between two CAS attempts.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use serde_json::json;

use super::client::{
    BaselineObservation, CommitOutcome, CommitRequest, ProjectInfo, ProjectState,
    RegistryObservation, StateBlob, StateEntry, StateHead, StateStatus, VerifiedEntry,
    VerifiedFile,
};
use super::digest::{pseudo_git_sha, sha256_hex};
use super::error::{BrokerErrorCode, StateBrokerError};
use super::transport::ProjectStateTransport;
use super::validate::{validate_commit_sha, validate_logical_path};

/// In-memory broker transport.
#[derive(Clone)]
pub struct MockStateTransport {
    inner: Arc<Mutex<MockState>>,
}

struct MockState {
    project_uuid: String,
    head: Option<String>,
    message: Option<String>,
    files: BTreeMap<String, Vec<u8>>,
    counter: u64,
    commits: u64,
    /// Queue of injected commit failures, consumed one per commit call.
    fail_next: VecDeque<StateBrokerError>,
}

impl MockStateTransport {
    /// An empty broker namespace (no durable state yet).
    #[must_use]
    pub fn new(project_uuid: &str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockState {
                project_uuid: project_uuid.to_string(),
                head: None,
                message: None,
                files: BTreeMap::new(),
                counter: 0,
                commits: 0,
                fail_next: VecDeque::new(),
            })),
        }
    }

    /// A namespace bootstrapped with `files` (a synthetic first commit).
    pub fn with_files<I, P, B>(project_uuid: &str, files: I) -> Self
    where
        I: IntoIterator<Item = (P, B)>,
        P: Into<String>,
        B: Into<Vec<u8>>,
    {
        let mock = Self::new(project_uuid);
        {
            let mut state = mock.lock();
            for (path, bytes) in files {
                state.files.insert(path.into(), bytes.into());
            }
            state.counter += 1;
            let paths: Vec<String> = state.files.keys().cloned().collect();
            let commit = next_commit_sha(&state, &paths);
            state.head = Some(commit);
            state.message = Some("mock: bootstrap\n\nProject-UUID: mock\nBroker: mock\n".to_string());
        }
        mock
    }

    /// The state ref this mock serves.
    #[must_use]
    pub fn state_ref(&self) -> String {
        format!("refs/heads/projects/{}/state", self.lock().project_uuid)
    }

    /// Current head commit, if any.
    #[must_use]
    pub fn head(&self) -> Option<String> {
        self.lock().head.clone()
    }

    /// Current head commit message, if any.
    #[must_use]
    pub fn head_message(&self) -> Option<String> {
        self.lock().message.clone()
    }

    /// Number of successful commits applied through [`Self::commit`].
    #[must_use]
    pub fn commit_count(&self) -> u64 {
        self.lock().commits
    }

    /// Raw bytes of a stored file.
    #[must_use]
    pub fn file_bytes(&self, path: &str) -> Option<Vec<u8>> {
        self.lock().files.get(path).cloned()
    }

    /// All stored logical paths, sorted.
    #[must_use]
    pub fn file_paths(&self) -> Vec<String> {
        self.lock().files.keys().cloned().collect()
    }

    /// Queue one injected failure for the next [`Self::commit`] call.
    /// Queueing twice makes the next two commit calls fail (used to exercise
    /// retry exhaustion).
    pub fn fail_next_commit(&self, error: StateBrokerError) {
        self.lock().fail_next.push_back(error);
    }

    /// Simulate a competing writer landing a commit before our next CAS
    /// attempt: upserts `files` and moves the head without any CAS check.
    /// Returns the synthetic commit sha.
    pub fn inject_competing_commit<I, P, B>(
        &self,
        files: I,
        message: &str,
        op_id: Option<&str>,
    ) -> String
    where
        I: IntoIterator<Item = (P, B)>,
        P: Into<String>,
        B: Into<Vec<u8>>,
    {
        let mut state = self.lock();
        for (path, bytes) in files {
            state.files.insert(path.into(), bytes.into());
        }
        state.counter += 1;
        let paths: Vec<String> = state.files.keys().cloned().collect();
        let commit = next_commit_sha(&state, &paths);
        state.head = Some(commit.clone());
        state.message = Some(build_message(&state.project_uuid, message, op_id));
        commit
    }

    fn lock(&self) -> MutexGuard<'_, MockState> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl ProjectStateTransport for MockStateTransport {
    fn read_state(&self) -> Result<ProjectState, StateBrokerError> {
        let state = self.lock();
        let entries: Vec<StateEntry> = state
            .files
            .iter()
            .map(|(path, bytes)| StateEntry {
                path: path.clone(),
                blob_sha: pseudo_git_sha(bytes),
                size: Some(bytes.len() as u64),
            })
            .collect();
        let head = state.head.clone().map(|commit| StateHead {
            commit,
            message: state.message.clone().unwrap_or_default(),
            committed_at: None,
        });
        Ok(ProjectState {
            project: ProjectInfo {
                uuid: state.project_uuid.clone(),
                slug: Some("mock".to_string()),
                source_repository: Some("https://example.invalid/mock".to_string()),
            },
            backend_repository: "mock/crosslink-state".to_string(),
            state: StateStatus {
                state_ref: format!("refs/heads/projects/{}/state", state.project_uuid),
                exists: head.is_some(),
                head,
                entries,
            },
            baseline: BaselineObservation {
                baseline_ref: "refs/heads/main".to_string(),
                expected_commit: "0".repeat(40),
                observed_commit: None,
                matches: true,
                error: None,
            },
            registry: RegistryObservation {
                source_ref: "refs/heads/main".to_string(),
                commit: None,
                present: true,
                entry: None,
                error: None,
            },
        })
    }

    fn read_blob(&self, path: &str, at: Option<&str>) -> Result<StateBlob, StateBrokerError> {
        validate_logical_path(path)?;
        if let Some(at) = at {
            validate_commit_sha(at)?;
        }
        let state = self.lock();
        let Some(head) = state.head.clone() else {
            return Err(StateBrokerError::not_found(
                "project has no durable state yet",
            ));
        };
        if let Some(at) = at {
            if at != head {
                return Err(StateBrokerError::not_found(
                    "state file not found at the requested commit",
                ));
            }
        }
        let Some(bytes) = state.files.get(path) else {
            return Err(StateBrokerError::not_found(format!(
                "state file not found: {path}"
            )));
        };
        Ok(StateBlob {
            path: path.to_string(),
            requested_ref: at.unwrap_or("state").to_string(),
            commit: head,
            blob_sha: pseudo_git_sha(bytes),
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    fn verify(
        &self,
        commit: &str,
        paths: &[String],
    ) -> Result<Vec<VerifiedEntry>, StateBrokerError> {
        validate_commit_sha(commit)?;
        if paths.is_empty() {
            return Err(StateBrokerError::invalid_input(
                "verify requires at least one path",
            ));
        }
        let state = self.lock();
        if state.head.as_deref() != Some(commit) {
            return Err(StateBrokerError::not_found(
                "commit is not the current state head in the mock",
            ));
        }
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            validate_logical_path(path)?;
            let entry = match state.files.get(path) {
                Some(bytes) => VerifiedEntry {
                    path: path.clone(),
                    present: true,
                    blob_sha: Some(pseudo_git_sha(bytes)),
                    sha256: Some(sha256_hex(bytes)),
                    size: Some(bytes.len() as u64),
                },
                None => VerifiedEntry {
                    path: path.clone(),
                    present: false,
                    blob_sha: None,
                    sha256: None,
                    size: None,
                },
            };
            entries.push(entry);
        }
        Ok(entries)
    }

    fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, StateBrokerError> {
        request.validate()?;
        let mut state = self.lock();
        if let Some(error) = state.fail_next.pop_front() {
            return Err(error);
        }
        if state.head != request.expected_head {
            let state_ref = format!("refs/heads/projects/{}/state", state.project_uuid);
            return Err(StateBrokerError::from_envelope(
                BrokerErrorCode::StaleState,
                "expected state head does not match the observed state head; no write was performed"
                    .to_string(),
                true,
                Some(json!({
                    "ref": state_ref,
                    "expected_head": request.expected_head,
                    "observed_head": state.head,
                })),
                Some(409),
                None,
                Some("state.commit".to_string()),
            ));
        }

        let previous_head = state.head.clone();
        state.counter += 1;
        let paths: Vec<String> = request.files.iter().map(|file| file.path.clone()).collect();
        let commit = next_commit_sha(&state, &paths);
        let message = build_message(&state.project_uuid, &request.message, request.op_id.as_deref());

        let mut files = Vec::with_capacity(request.files.len());
        for file in &request.files {
            state.files.insert(file.path.clone(), file.content.clone());
            files.push(VerifiedFile {
                path: file.path.clone(),
                blob_sha: Some(pseudo_git_sha(&file.content)),
                sha256: Some(sha256_hex(&file.content)),
                size: Some(file.content.len() as u64),
                verified: true,
            });
        }

        state.head = Some(commit.clone());
        state.message = Some(message.clone());
        state.commits += 1;
        Ok(CommitOutcome {
            state_ref: format!("refs/heads/projects/{}/state", state.project_uuid),
            commit: commit.clone(),
            previous_head,
            head_after: Some(commit),
            message,
            op_id: request.op_id.clone(),
            files,
            verified: true,
        })
    }
}

/// Deterministic commit sha for the mock's next commit.
fn next_commit_sha(state: &MockState, paths: &[String]) -> String {
    let mut seed = format!("{}:{}", state.project_uuid, state.counter);
    for path in paths {
        seed.push(':');
        seed.push_str(path);
    }
    pseudo_git_sha(seed.as_bytes())
}

/// Mirror the broker's commit-message construction (caller line + trailers).
fn build_message(project_uuid: &str, message: &str, op_id: Option<&str>) -> String {
    let mut trailers = vec![
        format!("Project-UUID: {project_uuid}"),
        "Broker: crosslink-state-broker/mock".to_string(),
    ];
    if let Some(op_id) = op_id {
        trailers.push(format!("Broker-Op: {op_id}"));
    }
    format!("{}\n\n{}\n", message.trim_end(), trailers.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_broker::transport::message_records_op;

    const UUID: &str = "1d440dcf-bcbf-4d1a-987c-d5334568a716";

    #[test]
    fn bootstrap_then_commit_moves_head_with_trailers() {
        let mock = MockStateTransport::new(UUID);
        assert_eq!(mock.state_ref(), format!("refs/heads/projects/{UUID}/state"));
        assert!(mock.head().is_none());
        assert_eq!(mock.commit_count(), 0);

        let request = CommitRequest::single(
            "checkpoints/first.json",
            br#"{"ok":true}"#.to_vec(),
            None,
            "checkpoint: first",
            Some("op-1".to_string()),
        );
        let outcome = mock.commit(&request).expect("bootstrap commit");
        assert!(outcome.verified);
        assert_eq!(outcome.previous_head, None);
        assert_eq!(outcome.head_after.as_deref(), Some(outcome.commit.as_str()));
        assert!(message_records_op(&outcome.message, "op-1"));
        assert_eq!(mock.commit_count(), 1);
        assert_eq!(mock.file_bytes("checkpoints/first.json").unwrap(), br#"{"ok":true}"#);
    }

    #[test]
    fn stale_expected_head_writes_nothing() {
        let mock = MockStateTransport::with_files(UUID, [("meta/counters.json", b"{}".to_vec())]);
        let before = mock.head().unwrap();
        let request = CommitRequest::single(
            "meta/counters.json",
            b"{\"next\":2}".to_vec(),
            Some("1".repeat(40)),
            "update counters",
            None,
        );
        let error = mock.commit(&request).unwrap_err();
        assert!(error.is_stale_state());
        assert_eq!(error.code(), BrokerErrorCode::StaleState);
        assert_eq!(mock.head().unwrap(), before, "head must not move");
        assert_eq!(mock.file_bytes("meta/counters.json").unwrap(), b"{}");
        assert_eq!(mock.commit_count(), 0);
    }

    #[test]
    fn verify_reports_presence_and_digests() {
        let mock = MockStateTransport::with_files(UUID, [("a/b.json", b"one".to_vec())]);
        let head = mock.head().unwrap();
        let entries = mock
            .verify(&head, &["a/b.json".to_string(), "missing.json".to_string()])
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_verified());
        assert_eq!(entries[0].sha256.as_deref(), Some(sha256_hex(b"one").as_str()));
        assert!(!entries[1].present);
        assert!(!entries[1].is_verified());
    }
}
