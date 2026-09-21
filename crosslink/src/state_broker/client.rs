//! HTTP client for the Crosslink State Broker v1 contract.
//!
//! Six semantic operations, one project namespace, typed envelopes:
//!
//! | Operation | Method |
//! |---|---|
//! | `/v1/health` | `health()` |
//! | `/v1/whoami` | `whoami()` |
//! | `/v1/projects/{uuid}/state` | `state()` |
//! | `/v1/projects/{uuid}/state/blob` | `read_blob()` |
//! | `/v1/projects/{uuid}/state/verify` | `verify()` |
//! | `/v1/projects/{uuid}/state/commit` | `commit()` |
//!
//! The client never logs, formats, or serializes the bearer token; every
//! error string is passed through [`StateBrokerConfig::redact`] before it is
//! stored, and the token is sent only in the `Authorization` header to the
//! configured broker host.

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config::StateBrokerConfig;
use super::error::{redact_value, BrokerErrorCode, StateBrokerError};
use super::validate::{
    validate_commit_sha, validate_logical_path, validate_message, validate_op_id,
    MAX_FILES_PER_COMMIT, MAX_FILE_BYTES, MAX_TOTAL_BYTES, MAX_VERIFY_PATHS,
};

/// Body length included in protocol errors (already redacted).
const MAX_ERROR_BODY_EXCERPT: usize = 256;

// ── Contract response types ──────────────────────────────────────────

/// `GET /v1/health` result.
#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    /// `"ok"` on a live broker.
    pub status: String,
    /// Service name (`crosslink-state-broker`).
    pub service: String,
    /// Broker version.
    pub version: String,
    /// Broker clock at response time (RFC 3339).
    pub time: String,
}

/// `GET /v1/whoami` result.
#[derive(Debug, Clone, Deserialize)]
pub struct WhoAmI {
    /// Stable token identifier (never the token itself).
    pub token_id: String,
    /// Project UUID the token is bound to.
    pub project_uuid: String,
    /// Granted scopes, e.g. `["state:read", "state:write"]`.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Project identity as reported by the registry entry.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectInfo {
    /// Project UUID.
    pub uuid: String,
    /// Registry slug, when the registry has an entry.
    #[serde(default)]
    pub slug: Option<String>,
    /// Source repository URL, when the registry has an entry.
    #[serde(default)]
    pub source_repository: Option<String>,
}

/// Head commit of the durable state ref.
#[derive(Debug, Clone, Deserialize)]
pub struct StateHead {
    /// 40-hex commit sha.
    pub commit: String,
    /// Full commit message (includes the broker trailer block).
    pub message: String,
    /// Commit date, when the backend reports one.
    #[serde(default)]
    pub committed_at: Option<String>,
}

/// One file in the durable state inventory.
#[derive(Debug, Clone, Deserialize)]
pub struct StateEntry {
    /// Project-relative logical path.
    pub path: String,
    /// Git blob sha (informational).
    pub blob_sha: String,
    /// Byte size, when the backend reports one.
    #[serde(default)]
    pub size: Option<u64>,
}

/// Durable state status: ref, head, and file inventory.
#[derive(Debug, Clone, Deserialize)]
pub struct StateStatus {
    /// Full state ref name (`refs/heads/projects/<uuid>/state`).
    #[serde(rename = "ref")]
    pub state_ref: String,
    /// Whether the state ref exists.
    pub exists: bool,
    /// Head commit, `None` when the ref does not exist.
    #[serde(default)]
    pub head: Option<StateHead>,
    /// File inventory under the project namespace, sorted by path.
    #[serde(default)]
    pub entries: Vec<StateEntry>,
}

impl StateStatus {
    /// The head commit sha, if the ref exists.
    #[must_use]
    pub fn head_commit(&self) -> Option<&str> {
        self.head.as_ref().map(|head| head.commit.as_str())
    }
}

/// Baseline observation for the broker's configured baseline ref.
#[derive(Debug, Clone, Deserialize)]
pub struct BaselineObservation {
    /// Baseline ref observed.
    #[serde(rename = "ref")]
    pub baseline_ref: String,
    /// Commit the broker expects on that ref.
    pub expected_commit: String,
    /// Commit actually observed, when readable.
    #[serde(default)]
    pub observed_commit: Option<String>,
    /// Whether observed equals expected.
    pub matches: bool,
    /// Observation error label, when the baseline could not be read.
    #[serde(default)]
    pub error: Option<String>,
}

/// Registry observation for the project.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryObservation {
    /// Registry ref observed.
    pub source_ref: String,
    /// Registry commit, when readable.
    #[serde(default)]
    pub commit: Option<String>,
    /// Whether the registry lists this project.
    pub present: bool,
    /// Raw registry entry, when present.
    #[serde(default)]
    pub entry: Option<Value>,
    /// Observation error label, when the registry could not be read.
    #[serde(default)]
    pub error: Option<String>,
}

/// `GET /v1/projects/{uuid}/state` result.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectState {
    /// Project identity.
    pub project: ProjectInfo,
    /// Backend repository (`owner/repo`) the broker writes to.
    pub backend_repository: String,
    /// Durable state status.
    pub state: StateStatus,
    /// Baseline observation.
    pub baseline: BaselineObservation,
    /// Registry observation.
    pub registry: RegistryObservation,
}

/// `GET .../state/blob` result.
#[derive(Debug, Clone, Deserialize)]
pub struct StateBlob {
    /// Logical path requested.
    pub path: String,
    /// Caller-named ref (the state ref by default).
    #[serde(rename = "ref")]
    pub requested_ref: String,
    /// Commit the blob was read at.
    pub commit: String,
    /// Git blob sha (informational).
    pub blob_sha: String,
    /// SHA-256 of the decoded content.
    pub sha256: String,
    /// Byte size of the decoded content.
    pub size: u64,
    /// Base64-encoded content.
    pub content_base64: String,
}

impl StateBlob {
    /// Decode and verify the blob content against `size` and `sha256`.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerErrorCode::Protocol`] when the content cannot be
    /// decoded, the size disagrees, or the digest disagrees — a broken
    /// transport must never surface as trusted state.
    pub fn bytes(&self) -> Result<Vec<u8>, StateBrokerError> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(self.content_base64.as_bytes())
            .map_err(|e| {
                StateBrokerError::protocol(format!("blob content is not valid base64: {e}"))
            })?;
        if decoded.len() as u64 != self.size {
            return Err(StateBrokerError::protocol(format!(
                "blob size mismatch for {}: envelope says {}, decoded {}",
                self.path,
                self.size,
                decoded.len()
            )));
        }
        let digest = crate::state_broker::digest::sha256_hex(&decoded);
        if digest != self.sha256 {
            return Err(StateBrokerError::protocol(format!(
                "blob digest mismatch for {}: envelope says {}, computed {digest}",
                self.path, self.sha256
            )));
        }
        Ok(decoded)
    }
}

/// `GET .../state/verify` result.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyResponse {
    /// Commit the paths were verified at.
    pub commit: String,
    /// One entry per requested path, in request order.
    pub entries: Vec<VerifiedEntry>,
}

/// Read-back digest for one path at a commit.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifiedEntry {
    /// Logical path.
    pub path: String,
    /// Whether the path exists at that commit.
    pub present: bool,
    /// Git blob sha (informational), when present.
    #[serde(default)]
    pub blob_sha: Option<String>,
    /// SHA-256 of the content, when present.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Byte size, when present.
    #[serde(default)]
    pub size: Option<u64>,
}

impl VerifiedEntry {
    /// Whether the entry exists and carries digests.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        self.present && self.sha256.is_some()
    }
}

/// Read-back digest for one committed file.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifiedFile {
    /// Logical path.
    pub path: String,
    /// Git blob sha (informational), when present.
    #[serde(default)]
    pub blob_sha: Option<String>,
    /// SHA-256 of the written content, when present.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Byte size, when present.
    #[serde(default)]
    pub size: Option<u64>,
    /// Whether the broker's read-back matched the submitted content.
    pub verified: bool,
}

/// `POST .../state/commit` result.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitOutcome {
    /// State ref that moved.
    #[serde(rename = "ref")]
    pub state_ref: String,
    /// The new commit.
    pub commit: String,
    /// Head before the write (`None` for a bootstrap commit).
    #[serde(default)]
    pub previous_head: Option<String>,
    /// Head after the write (read back by the broker).
    #[serde(default)]
    pub head_after: Option<String>,
    /// Full commit message, including broker trailers.
    pub message: String,
    /// Op id recorded in the trailers, when supplied.
    #[serde(default)]
    pub op_id: Option<String>,
    /// Per-file read-back results.
    #[serde(default)]
    pub files: Vec<VerifiedFile>,
    /// Broker's overall read-back verdict (always `true` on success).
    pub verified: bool,
}

/// A file to submit in one CAS commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFile {
    /// Project-relative logical path.
    pub path: String,
    /// File content bytes.
    pub content: Vec<u8>,
}

/// One compare-and-swap mutation request.
#[derive(Debug, Clone)]
pub struct CommitRequest {
    /// Head the caller believes is current. `None` means "expect no state ref"
    /// (bootstrap). The broker rejects a mismatch with `stale_state` and writes
    /// nothing.
    pub expected_head: Option<String>,
    /// Single-line commit message (broker trailer block is added server-side).
    pub message: String,
    /// Optional op id recorded in the commit trailers for later reconciliation.
    pub op_id: Option<String>,
    /// Files to upsert (whole-file replacement; 1..=32).
    pub files: Vec<CommitFile>,
}

impl CommitRequest {
    /// Build a request for one whole-file upsert.
    #[must_use]
    pub fn single(
        path: impl Into<String>,
        content: impl Into<Vec<u8>>,
        expected_head: Option<String>,
        message: impl Into<String>,
        op_id: Option<String>,
    ) -> Self {
        Self {
            expected_head,
            message: message.into(),
            op_id,
            files: vec![CommitFile {
                path: path.into(),
                content: content.into(),
            }],
        }
    }

    /// Validate every field against the broker contract limits.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerErrorCode::InvalidInput`] on the first violation.
    pub fn validate(&self) -> Result<(), StateBrokerError> {
        if let Some(head) = &self.expected_head {
            validate_commit_sha(head)?;
        }
        validate_message(&self.message)?;
        if let Some(op_id) = &self.op_id {
            validate_op_id(op_id)?;
        }
        if self.files.is_empty() {
            return Err(StateBrokerError::invalid_input(
                "a commit requires at least one file",
            ));
        }
        if self.files.len() > MAX_FILES_PER_COMMIT {
            return Err(StateBrokerError::invalid_input(format!(
                "at most {MAX_FILES_PER_COMMIT} files may be committed per request"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        let mut total: usize = 0;
        for file in &self.files {
            validate_logical_path(&file.path)?;
            if !seen.insert(file.path.as_str()) {
                return Err(StateBrokerError::invalid_input(format!(
                    "duplicate path in commit request: {}",
                    file.path
                )));
            }
            if file.content.is_empty() || file.content.len() > MAX_FILE_BYTES {
                return Err(StateBrokerError::invalid_input(format!(
                    "file {} must be 1-{MAX_FILE_BYTES} bytes",
                    file.path
                )));
            }
            total += file.content.len();
            if total > MAX_TOTAL_BYTES {
                return Err(StateBrokerError::invalid_input(format!(
                    "total commit size must not exceed {MAX_TOTAL_BYTES} bytes"
                )));
            }
        }
        Ok(())
    }

    /// The paths in this request.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.files.iter().map(|f| f.path.clone()).collect()
    }
}

// ── Wire envelopes ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    ok: bool,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error: Option<ErrorEnvelope>,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    code: String,
    message: String,
    /// Broker-supplied retryability. `None` falls back to the code default;
    /// an explicit `false` (e.g. a read-back mismatch) is never overridden.
    #[serde(default)]
    retryable: Option<bool>,
    #[serde(default)]
    details: Option<Value>,
}

#[derive(Debug, Serialize)]
struct CommitBody<'a> {
    expected_head: Option<&'a str>,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    op_id: Option<&'a str>,
    files: Vec<CommitFileBody<'a>>,
}

#[derive(Debug, Serialize)]
struct CommitFileBody<'a> {
    path: &'a str,
    content_base64: String,
}

// ── Client ───────────────────────────────────────────────────────────

/// Blocking HTTP client for one broker project namespace.
pub struct StateBrokerClient {
    config: StateBrokerConfig,
    http: reqwest::blocking::Client,
}

impl std::fmt::Debug for StateBrokerClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateBrokerClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl StateBrokerClient {
    /// Build a client for `config`.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerErrorCode::Configuration`] when the HTTP client cannot
    /// be constructed.
    pub fn new(config: StateBrokerConfig) -> Result<Self, StateBrokerError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(config.timeout())
            .user_agent(concat!(
                "crosslink-state-broker-client/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| {
                StateBrokerError::configuration(format!("HTTP client init failed: {e}"))
            })?;
        Ok(Self { config, http })
    }

    /// The configuration this client was built with.
    #[must_use]
    pub const fn config(&self) -> &StateBrokerConfig {
        &self.config
    }

    /// Host-only label of the configured broker (never the token or a path).
    #[must_use]
    pub fn backend_host(&self) -> String {
        host_label(self.config.base_url())
    }

    /// Liveness probe (unauthenticated).
    ///
    /// # Errors
    ///
    /// Transport, protocol, or typed broker failures.
    pub fn health(&self) -> Result<Health, StateBrokerError> {
        self.get(&format!("{}/v1/health", self.config.base_url()), &[])
    }

    /// Caller identity: token id, project UUID, scopes.
    ///
    /// The broker-reported project UUID is checked against the configured UUID;
    /// a mismatch is [`BrokerErrorCode::IdentityMismatch`] (never a silently
    /// accepted answer from another project).
    ///
    /// # Errors
    ///
    /// Transport, protocol, identity-mismatch, or typed broker failures.
    ///
    /// [`BrokerErrorCode::IdentityMismatch`]: super::error::BrokerErrorCode::IdentityMismatch
    pub fn whoami(&self) -> Result<WhoAmI, StateBrokerError> {
        let who = self.get::<WhoAmI>(&format!("{}/v1/whoami", self.config.base_url()), &[])?;
        if who.project_uuid != self.config.project_uuid() {
            return Err(StateBrokerError::identity_mismatch(format!(
                "broker reports project {} but this client is configured for {}",
                who.project_uuid,
                self.config.project_uuid()
            )));
        }
        Ok(who)
    }

    /// Read the current durable state: ref, head, inventory, baseline.
    ///
    /// The reported project UUID and state ref are checked against the
    /// configured identity before the state is returned: a broker (or a
    /// misrouted request) that answers for another project is a hard
    /// [`BrokerErrorCode::IdentityMismatch`] error, not data.
    ///
    /// # Errors
    ///
    /// Transport, protocol, identity-mismatch, or typed broker failures.
    ///
    /// [`BrokerErrorCode::IdentityMismatch`]: super::error::BrokerErrorCode::IdentityMismatch
    pub fn state(&self) -> Result<ProjectState, StateBrokerError> {
        let state: ProjectState = self.get(&self.state_url(), &[])?;
        if state.project.uuid != self.config.project_uuid() {
            return Err(StateBrokerError::identity_mismatch(format!(
                "broker state belongs to project {} but this client is configured for {}",
                state.project.uuid,
                self.config.project_uuid()
            )));
        }
        if state.state.state_ref != self.config.state_ref() {
            return Err(StateBrokerError::identity_mismatch(format!(
                "broker state ref {} does not match the configured {}",
                state.state.state_ref,
                self.config.state_ref()
            )));
        }
        Ok(state)
    }

    /// Hydrate one state file at `at` (default: the state head).
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for a bad path/ref, plus transport,
    /// protocol, and typed broker failures (including
    /// [`BrokerErrorCode::NotFound`]).
    pub fn read_blob(&self, path: &str, at: Option<&str>) -> Result<StateBlob, StateBrokerError> {
        validate_logical_path(path)?;
        let mut query: Vec<(&str, String)> = vec![("path", path.to_string())];
        if let Some(reference) = at {
            if !self.config.is_acceptable_ref(reference) {
                return Err(StateBrokerError::invalid_input(
                    "ref must be an exact 40-character commit sha or the project's own state ref",
                ));
            }
            query.push(("ref", reference.to_string()));
        }
        self.get(&format!("{}/blob", self.state_url()), &query)
    }

    /// Read back digests for up to 32 paths at an exact commit.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for a bad commit/paths, plus
    /// transport, protocol, and typed broker failures.
    pub fn verify(
        &self,
        commit: &str,
        paths: &[String],
    ) -> Result<VerifyResponse, StateBrokerError> {
        validate_commit_sha(commit)?;
        if paths.is_empty() {
            return Err(StateBrokerError::invalid_input(
                "verify requires at least one path",
            ));
        }
        if paths.len() > MAX_VERIFY_PATHS {
            return Err(StateBrokerError::invalid_input(format!(
                "at most {MAX_VERIFY_PATHS} paths may be verified per request"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for path in paths {
            validate_logical_path(path)?;
            if !seen.insert(path.as_str()) {
                return Err(StateBrokerError::invalid_input(format!(
                    "duplicate path in verify request: {path}"
                )));
            }
        }
        self.get(
            &format!("{}/verify", self.state_url()),
            &[("commit", commit.to_string()), ("paths", paths.join(","))],
        )
    }

    /// Submit one compare-and-swap mutation.
    ///
    /// The broker checks `expected_head` **before** creating any object; a
    /// mismatch is [`BrokerErrorCode::StaleState`] and nothing is written.
    /// Never blind-retry this call — use
    /// [`crate::state_broker::ProjectStateTransport::commit_cas`].
    ///
    /// # Write ambiguity
    ///
    /// A write that fails *after* the request was sent can have landed anyway.
    /// Every such failure (transport timeout/reset, unparseable response,
    /// upstream 502, internal 500, a success envelope with
    /// `verified: false`, or a success envelope that does not carry a verified
    /// read-back for every submitted path —
    /// `details.reason = "incomplete_readback"`) is surfaced as
    /// [`BrokerErrorCode::ReconcileRequired`] with
    /// `details.reason`, the intended `op_id`, and any known commit/paths —
    /// never as an ordinary error or success. Reconcile by op id before
    /// deciding whether to write again.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for locally-rejected input,
    /// [`BrokerErrorCode::ReconcileRequired`] for an ambiguous write, and the
    /// definite rejections (`unauthorized`, `scope_violation`, `not_found`,
    /// `method_not_allowed`, `stale_state`) unchanged. Any transport failure on
    /// the POST is treated as ambiguous, even when the request may not have
    /// left the machine: a blind write retry is never allowed.
    ///
    /// [`BrokerErrorCode::InvalidInput`]: super::error::BrokerErrorCode::InvalidInput
    /// [`BrokerErrorCode::ReconcileRequired`]: super::error::BrokerErrorCode::ReconcileRequired
    pub fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, StateBrokerError> {
        request.validate()?;
        let body = CommitBody {
            expected_head: request.expected_head.as_deref(),
            message: &request.message,
            op_id: request.op_id.as_deref(),
            files: request
                .files
                .iter()
                .map(|file| CommitFileBody {
                    path: &file.path,
                    content_base64: base64::engine::general_purpose::STANDARD.encode(&file.content),
                })
                .collect(),
        };
        let body = serde_json::to_value(&body).map_err(|e| {
            StateBrokerError::invalid_input(format!("commit body serialization failed: {e}"))
        })?;
        let outcome: CommitOutcome = match self.send(
            reqwest::Method::POST,
            &format!("{}/commit", self.state_url()),
            &[],
            Some(&body),
        ) {
            Ok(outcome) => outcome,
            Err(error) => return Err(self.classify_commit_failure(request, error)),
        };
        // A success must be content-verified for every submitted path: the
        // response must carry exactly one verified entry per requested path,
        // with no missing, extra, duplicate, or mismatched paths. A transport
        // that disagrees with itself is not an ordinary success.
        let failed_paths = unverified_response_paths(request, &outcome);
        if !outcome.verified || !failed_paths.is_empty() {
            let has_unverified_files = outcome.files.iter().any(|file| !file.verified);
            let (reason, message) = if !outcome.verified || has_unverified_files {
                (
                    "verified_false",
                    "broker reported an unverified write; the write may have landed partially — \
                     reconcile by op id before retrying",
                )
            } else {
                (
                    "incomplete_readback",
                    "broker reported success without a verified read-back for every submitted \
                     path; the write may have landed partially — reconcile by op id before retrying",
                )
            };
            return Err(reconcile_required_for_outcome(
                request,
                &outcome,
                reason,
                message,
                failed_paths,
            ));
        }
        Ok(outcome)
    }

    /// Classify a failed `POST /commit`: definite rejections pass through;
    /// anything that may have been applied becomes `reconcile_required`.
    fn classify_commit_failure(
        &self,
        request: &CommitRequest,
        error: StateBrokerError,
    ) -> StateBrokerError {
        use super::error::BrokerErrorCode;
        match error.code() {
            // Definite rejections and already-classified client errors pass
            // through unchanged; the broker rejected the request (or the
            // failure never got that far).
            BrokerErrorCode::Unauthorized
            | BrokerErrorCode::ScopeViolation
            | BrokerErrorCode::InvalidInput
            | BrokerErrorCode::NotFound
            | BrokerErrorCode::MethodNotAllowed
            | BrokerErrorCode::StaleState
            | BrokerErrorCode::Configuration
            | BrokerErrorCode::LocalIo
            | BrokerErrorCode::ReconcileRequired
            | BrokerErrorCode::IdentityMismatch => error,
            // Ambiguous: the write may have landed (or the broker landed it and
            // failed while reading back).
            BrokerErrorCode::Transport
            | BrokerErrorCode::Protocol
            | BrokerErrorCode::InternalError => {
                let reason = if error.code() == BrokerErrorCode::Transport {
                    "transport_ambiguous"
                } else {
                    "response_ambiguous"
                };
                self.reconcile_required_for_error(request, &error, reason)
            }
            BrokerErrorCode::UpstreamError => {
                self.reconcile_required_for_error(request, &error, "readback_mismatch")
            }
        }
    }

    /// Build a `reconcile_required` error from a lower-level failure.
    fn reconcile_required_for_error(
        &self,
        request: &CommitRequest,
        error: &StateBrokerError,
        reason: &str,
    ) -> StateBrokerError {
        let mut details = serde_json::Map::new();
        details.insert("reason".to_string(), Value::String(reason.to_string()));
        details.insert(
            "cause".to_string(),
            Value::String(self.config.redact(error.message())),
        );
        if let Some(op_id) = &request.op_id {
            details.insert("op_id".to_string(), Value::String(op_id.clone()));
        }
        details.insert(
            "paths".to_string(),
            Value::Array(request.paths().into_iter().map(Value::String).collect()),
        );
        if let Some(previous) = error.details() {
            for key in ["commit", "failed_paths", "observed_head"] {
                if let Some(value) = previous.get(key) {
                    details.insert(key.to_string(), value.clone());
                }
            }
        }
        StateBrokerError::reconcile_required(
            format!(
                "write outcome is unknown ({reason}); reconcile by op_id before retrying: {error}"
            ),
            Some(Value::Object(details)),
        )
    }

    fn state_url(&self) -> String {
        format!(
            "{}/v1/projects/{}/state",
            self.config.base_url(),
            self.config.project_uuid()
        )
    }

    fn get<T: DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<T, StateBrokerError> {
        self.send(reqwest::Method::GET, url, query, None)
    }

    fn send<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<T, StateBrokerError> {
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(self.config.token());
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = match request.send() {
            Ok(response) => response,
            Err(error) => {
                let retryable = error.is_timeout() || error.is_connect();
                let message = self
                    .config
                    .redact(&format!("request to {} failed: {error}", host_label(url)));
                return Err(StateBrokerError::transport(message, retryable));
            }
        };

        let status = response.status().as_u16();
        let text = match response.text() {
            Ok(text) => text,
            Err(error) => {
                let message = self.config.redact(&format!(
                    "reading broker response from {} failed: {error}",
                    host_label(url)
                ));
                return Err(StateBrokerError::transport(message, true));
            }
        };

        let envelope: Envelope<Value> = match serde_json::from_str(&text) {
            Ok(envelope) => envelope,
            Err(error) => {
                let excerpt = truncate_excerpt(&self.config.redact(&text));
                return Err(StateBrokerError::protocol(format!(
                    "broker response was not a JSON envelope (http {status}): {error}; body: {excerpt}"
                )));
            }
        };

        if envelope.ok {
            let result = envelope.result.ok_or_else(|| {
                StateBrokerError::protocol("broker success envelope carried no result")
            })?;
            return serde_json::from_value(result).map_err(|error| {
                StateBrokerError::protocol(format!(
                    "broker result did not match the v1 contract: {error}"
                ))
            });
        }

        let error = envelope.error.ok_or_else(|| {
            StateBrokerError::protocol(format!(
                "broker failure envelope (http {status}) carried no error object"
            ))
        })?;
        let code = BrokerErrorCode::from_contract_str(&error.code).ok_or_else(|| {
            StateBrokerError::protocol(format!(
                "broker returned an unknown error code {:?} (http {status})",
                error.code
            ))
        })?;
        Err(StateBrokerError::from_envelope(
            code,
            self.config.redact(&error.message),
            error.retryable.unwrap_or_else(|| code.default_retryable()),
            error
                .details
                .as_ref()
                .map(|d| redact_value(d, self.config.token())),
            Some(status),
            envelope.request_id,
            envelope.operation,
        ))
    }
}

/// Build a `reconcile_required` error from a received but unverified or
/// incomplete success outcome.
fn reconcile_required_for_outcome(
    request: &CommitRequest,
    outcome: &CommitOutcome,
    reason: &str,
    message: &str,
    failed_paths: Vec<String>,
) -> StateBrokerError {
    let mut details = serde_json::Map::new();
    details.insert("reason".to_string(), Value::String(reason.to_string()));
    details.insert("commit".to_string(), Value::String(outcome.commit.clone()));
    if let Some(op_id) = &request.op_id {
        details.insert("op_id".to_string(), Value::String(op_id.clone()));
    }
    if !failed_paths.is_empty() {
        details.insert(
            "failed_paths".to_string(),
            Value::Array(failed_paths.into_iter().map(Value::String).collect()),
        );
    }
    StateBrokerError::reconcile_required(message.to_string(), Some(Value::Object(details)))
}

/// Whether `outcome` carries exactly one verified read-back entry for every
/// path in `request`, with no missing, extra, duplicate, or mismatched paths.
pub(crate) fn outcome_verifies_every_requested_path(
    request: &CommitRequest,
    outcome: &CommitOutcome,
) -> bool {
    unverified_response_paths(request, outcome).is_empty()
}

/// Paths whose read-back result is missing, unverified, duplicated, or not part
/// of the request (sorted and deduplicated).
pub(crate) fn unverified_response_paths(
    request: &CommitRequest,
    outcome: &CommitOutcome,
) -> Vec<String> {
    let mut failed = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for file in &outcome.files {
        let requested = request
            .files
            .iter()
            .any(|candidate| candidate.path == file.path);
        if !requested || !file.verified || !seen.insert(file.path.as_str()) {
            failed.push(file.path.clone());
        }
    }
    for file in &request.files {
        if !seen.contains(file.path.as_str()) {
            failed.push(file.path.clone());
        }
    }
    failed.sort();
    failed.dedup();
    failed
}

/// Host-only label for an error message (never the full URL).
fn host_label(url: &str) -> String {
    url.split_once("://").map_or_else(
        || url.to_string(),
        |(scheme, rest)| {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}")
        },
    )
}

/// Truncate a body excerpt for error messages.
fn truncate_excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_ERROR_BODY_EXCERPT {
        return trimmed.to_string();
    }
    let excerpt: String = trimmed.chars().take(MAX_ERROR_BODY_EXCERPT).collect();
    format!("{excerpt}…")
}
