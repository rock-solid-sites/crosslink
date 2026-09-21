//! CDP-1 source resolution: the pushed git checkpoint, the repository↔project
//! binding, and the journal anchor (spec §1, §12 item 9).

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{SOURCE_REF, SOURCE_STATE_PATH, SOURCE_TRACKING_REF};
use crate::checkpoint::CheckpointState;
use crate::events::OrderingKey;
use crate::state_broker::config::StateBrokerConfig;
use crate::state_broker::error::StateBrokerError;
use crate::state_broker::cdp1::manifest::SourceCheckpoint;

/// The exact pushed checkpoint blob a publish derives from.
#[derive(Debug, Clone)]
pub struct PushedCheckpoint {
    /// The pushed checkpoint commit `S`.
    pub commit: String,
    /// Git blob sha of `S:state.json`.
    pub state_blob_sha: String,
    /// The exact `state.json` bytes.
    pub state_bytes: Vec<u8>,
    /// Parsed checkpoint state.
    pub state: CheckpointState,
    /// `state.watermark`, required to be `Some`.
    pub watermark: OrderingKey,
    /// SHA-256 of `state_bytes`.
    pub state_sha256: String,
}

/// A source of a durably pushed v3 checkpoint.
pub trait CheckpointSource {
    /// Resolve the pushed checkpoint.
    ///
    /// # Errors
    ///
    /// Returns a configuration/local-IO error when the checkpoint cannot be
    /// proven pushed or its blob cannot be read/parsed.
    fn resolve_pushed_checkpoint(&self) -> Result<PushedCheckpoint, StateBrokerError>;
}

/// Git-backed anchor for the journal-anchored reader profile.
pub trait JournalAnchor {
    /// Prove that `state_bytes` is exactly the blob at
    /// `source.commit:source.state_path`.
    ///
    /// # Errors
    ///
    /// Returns a protocol error on any mismatch.
    fn verify_anchor(
        &self,
        source: &SourceCheckpoint,
        state_bytes: &[u8],
    ) -> Result<(), StateBrokerError>;
}

/// Reads the checkpoint ref from a git object store, proving durability with a
/// fetch (spec §1.1 P1–P5).
#[derive(Debug, Clone)]
pub struct GitCheckpointSource {
    cache_dir: PathBuf,
    remote: String,
}

impl GitCheckpointSource {
    /// Build a source over `cache_dir` (the v3 hub cache) and `remote`.
    #[must_use]
    pub fn new(cache_dir: PathBuf, remote: impl Into<String>) -> Self {
        Self {
            cache_dir,
            remote: remote.into(),
        }
    }

    /// Build a source from a `.crosslink` directory.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the cache directory is absent.
    pub fn from_crosslink_dir(crosslink_dir: &Path) -> Result<Self, StateBrokerError> {
        let cache_dir = crosslink_dir.join(crate::sync::HUB_CACHE_DIR);
        if !cache_dir.is_dir() {
            return Err(StateBrokerError::configuration(format!(
                "no v3 hub cache at {}; the checkpoint cannot be proven pushed",
                cache_dir.display()
            )));
        }
        Ok(Self::new(
            cache_dir,
            crate::sync::read_tracker_remote(crosslink_dir),
        ))
    }

    /// Fetch the checkpoint ref into the remote-tracking ref (P1).
    ///
    /// # Errors
    ///
    /// Returns a transport-style local error when the fetch fails; durability
    /// cannot be proven without it.
    pub fn fetch_checkpoint(&self) -> Result<(), StateBrokerError> {
        let spec = format!("+{SOURCE_REF}:{SOURCE_TRACKING_REF}");
        let output = Command::new("git")
            .current_dir(&self.cache_dir)
            .args(["fetch", &self.remote, &spec])
            .output()
            .map_err(|e| {
                StateBrokerError::local_io(format!("cannot run git fetch for the checkpoint: {e}"))
            })?;
        if !output.status.success() {
            return Err(StateBrokerError::local_io(format!(
                "git fetch of {SOURCE_REF} from '{}' failed: {}",
                self.remote,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    fn rev_parse(&self, spec: &str) -> Result<Option<String>, StateBrokerError> {
        crate::hub_v3::git_rev_parse_optional(&self.cache_dir, spec)
            .map_err(|e| StateBrokerError::local_io(format!("git rev-parse {spec} failed: {e}")))
    }

    fn cat_blob(&self, spec: &str) -> Result<Option<Vec<u8>>, StateBrokerError> {
        crate::hub_v3::git_cat_file_blob_optional(&self.cache_dir, spec)
            .map_err(|e| StateBrokerError::local_io(format!("git cat-file {spec} failed: {e}")))
    }
}

impl CheckpointSource for GitCheckpointSource {
    fn resolve_pushed_checkpoint(&self) -> Result<PushedCheckpoint, StateBrokerError> {
        self.fetch_checkpoint()?;
        let commit = self
            .rev_parse(SOURCE_TRACKING_REF)?
            .ok_or_else(|| {
                StateBrokerError::configuration(format!(
                    "{SOURCE_TRACKING_REF} is absent after a successful fetch; \
                     no pushed v3 checkpoint exists to publish"
                ))
            })?;
        let blob_spec = format!("{commit}:{SOURCE_STATE_PATH}");
        let state_blob_sha = self.rev_parse(&blob_spec)?.ok_or_else(|| {
            StateBrokerError::configuration(format!(
                "checkpoint commit {commit} has no {SOURCE_STATE_PATH} blob"
            ))
        })?;
        let state_bytes = self.cat_blob(&blob_spec)?.ok_or_else(|| {
            StateBrokerError::configuration(format!(
                "checkpoint blob {blob_spec} could not be read"
            ))
        })?;
        let state = CheckpointState::from_slice(&state_bytes).map_err(|e| {
            StateBrokerError::protocol(format!("checkpoint {commit} state.json is invalid: {e}"))
        })?;
        let watermark = state.watermark.clone().ok_or_else(|| {
            StateBrokerError::protocol(format!(
                "checkpoint {commit} has no watermark; a full-reset genesis is not publishable"
            ))
        })?;
        let state_sha256 = crate::state_broker::digest::sha256_hex(&state_bytes);
        Ok(PushedCheckpoint {
            commit,
            state_blob_sha,
            state_bytes,
            state,
            watermark,
            state_sha256,
        })
    }
}

impl JournalAnchor for GitCheckpointSource {
    fn verify_anchor(
        &self,
        source: &SourceCheckpoint,
        state_bytes: &[u8],
    ) -> Result<(), StateBrokerError> {
        let blob_spec = format!("{}:{}", source.commit, source.state_path);
        let observed_blob = self.rev_parse(&blob_spec)?;
        if observed_blob.as_deref() != Some(source.state_blob_sha.as_str()) {
            return Err(StateBrokerError::protocol(format!(
                "journal anchor mismatch: {} is {:?}, manifest records {}",
                blob_spec, observed_blob, source.state_blob_sha
            )));
        }
        let observed = self.cat_blob(&blob_spec)?.ok_or_else(|| {
            StateBrokerError::protocol(format!("journal anchor blob {blob_spec} is unreadable"))
        })?;
        if observed != state_bytes {
            return Err(StateBrokerError::protocol(format!(
                "journal anchor content mismatch for {blob_spec}"
            )));
        }
        let digest = crate::state_broker::digest::sha256_hex(&observed);
        if digest != source.state_sha256 {
            return Err(StateBrokerError::protocol(format!(
                "journal anchor digest mismatch for {blob_spec}"
            )));
        }
        Ok(())
    }
}

/// A repository↔project-UUID binding (ADR-802 §16; spec §12 item 9).
///
/// The binding lives in `.crosslink/hook-config.json`:
///
/// ```json
/// "state_broker_binding": {
///   "project_uuid": "7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f",
///   "repository": "github.com/example/repo"
/// }
/// ```
///
/// The publisher refuses to run when the binding is absent or does not match
/// both the configured broker project UUID and this repository's normalized
/// remote. A missing binding is a configuration error, never a silent pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryBinding {
    /// Broker project UUID this repository is bound to.
    pub project_uuid: String,
    /// Normalized repository identity (`host/path`).
    pub repository: String,
}

impl RepositoryBinding {
    /// Read the binding from `.crosslink/hook-config.json`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the file exists but is unreadable or
    /// the binding is malformed.
    pub fn from_hook_config(
        crosslink_dir: &Path,
    ) -> Result<Option<Self>, StateBrokerError> {
        let path = crosslink_dir.join("hook-config.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(StateBrokerError::configuration(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            StateBrokerError::configuration(format!(
                "{} is not valid JSON: {e}",
                path.display()
            ))
        })?;
        let Some(binding) = value.get(super::BINDING_KEY) else {
            return Ok(None);
        };
        let project_uuid = binding
            .get("project_uuid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StateBrokerError::configuration(format!(
                    "{} binding has no string project_uuid",
                    super::BINDING_KEY
                ))
            })?;
        let repository = binding
            .get("repository")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StateBrokerError::configuration(format!(
                    "{} binding has no string repository",
                    super::BINDING_KEY
                ))
            })?;
        if !crate::state_broker::validate::is_canonical_uuid(project_uuid) {
            return Err(StateBrokerError::configuration(format!(
                "{} binding project_uuid {project_uuid:?} is not a canonical UUID",
                super::BINDING_KEY
            )));
        }
        Ok(Some(Self {
            project_uuid: project_uuid.to_string(),
            repository: normalize_remote(repository),
        }))
    }

    /// Resolve and enforce the binding for a broker configuration.
    ///
    /// # Errors
    ///
    /// Returns a configuration/identity error when the binding is missing, the
    /// project UUID disagrees, or the repository identity disagrees.
    pub fn require_for(
        crosslink_dir: &Path,
        config: &StateBrokerConfig,
    ) -> Result<Self, StateBrokerError> {
        let binding = Self::from_hook_config(crosslink_dir)?.ok_or_else(|| {
            StateBrokerError::configuration(format!(
                "no {} in hook-config.json; the broker project UUID is not bound to this \
                 repository. Add {{\"project_uuid\": ..., \"repository\": ...}} before publishing \
                 (ADR-802 §16)",
                super::BINDING_KEY
            ))
        })?;
        let remote_url = repo_remote_url(crosslink_dir)?;
        binding.check(config.project_uuid(), &remote_url)?;
        Ok(binding)
    }

    /// Check the binding against the configured UUID and the repository remote.
    ///
    /// # Errors
    ///
    /// Returns an identity-mismatch error on any disagreement.
    pub fn check(
        &self,
        configured_project_uuid: &str,
        remote_url: &str,
    ) -> Result<(), StateBrokerError> {
        if self.project_uuid != configured_project_uuid {
            return Err(StateBrokerError::identity_mismatch(format!(
                "repository binding names project {} but the broker is configured for {}",
                self.project_uuid, configured_project_uuid
            )));
        }
        let normalized = normalize_remote(remote_url);
        if self.repository != normalized {
            return Err(StateBrokerError::identity_mismatch(format!(
                "repository binding names {:?} but this repository is {normalized:?}",
                self.repository
            )));
        }
        Ok(())
    }
}

/// Normalize a git remote URL to a stable `host/path` identity.
///
/// Handles `git@host:path`, `scheme://[user@]host[:port]/path`, and plain
/// paths; strips a trailing `.git`, a trailing `/`, and lowercases the host.
#[must_use]
pub fn normalize_remote(url: &str) -> String {
    let mut rest = url.trim().to_string();
    if let Some(scp) = rest.strip_prefix("git@") {
        rest = scp.replacen(':', "/", 1);
    } else if let Some(index) = rest.find("://") {
        let after_scheme = &rest[index + 3..];
        let without_user = after_scheme
            .split_once('@')
            .map_or(after_scheme, |(_, tail)| tail);
        rest = without_user.to_string();
    }
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    // Drop a port from the host for stability across transports.
    let host = host.split_once(':').map_or(host.as_str(), |(h, _)| h);
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        host.to_string()
    } else {
        format!("{host}/{path}")
    }
}

/// The repository's remote URL for the tracker remote.
///
/// # Errors
///
/// Returns a configuration error when the remote cannot be resolved.
pub fn repo_remote_url(crosslink_dir: &Path) -> Result<String, StateBrokerError> {
    let repo_root = crosslink_dir.parent().unwrap_or(crosslink_dir);
    let remote = crate::sync::read_tracker_remote(crosslink_dir);
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["remote", "get-url", &remote])
        .output()
        .map_err(|e| StateBrokerError::configuration(format!("cannot run git remote get-url: {e}")))?;
    if !output.status.success() {
        return Err(StateBrokerError::configuration(format!(
            "git remote get-url {remote} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_remote_handles_transport_forms() {
        let expected = "github.com/example/repo";
        for url in [
            "git@github.com:example/repo.git",
            "https://github.com/example/repo.git",
            "https://user@github.com/example/repo",
            "ssh://git@github.com/example/repo.git",
            "http://GitHub.com/example/repo/",
        ] {
            assert_eq!(normalize_remote(url), expected, "for {url}");
        }
        assert_eq!(normalize_remote("git@example.com:team/sub/repo.git"), "example.com/team/sub/repo");
        assert_eq!(normalize_remote("https://github.com:443/example/repo.git"), "github.com/example/repo");
    }

    #[test]
    fn binding_check_enforces_both_halves() {
        let binding = RepositoryBinding {
            project_uuid: "7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f".to_string(),
            repository: "github.com/example/repo".to_string(),
        };
        binding
            .check(
                "7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f",
                "git@github.com:example/repo.git",
            )
            .unwrap();
        assert!(binding
            .check("0f0e0d0c-0b0a-4908-8706-050403020100", "git@github.com:example/repo.git")
            .is_err());
        assert!(binding
            .check("7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f", "git@github.com:other/repo.git")
            .is_err());
    }

    #[test]
    fn binding_from_hook_config_reads_and_normalizes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"state_broker_binding":{"project_uuid":"7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f","repository":"git@github.com:example/repo.git"}}"#,
        )
        .unwrap();
        let binding = RepositoryBinding::from_hook_config(dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(binding.repository, "github.com/example/repo");
        // Missing binding is None (the caller fails closed).
        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join("hook-config.json"), "{}").unwrap();
        assert!(RepositoryBinding::from_hook_config(empty.path())
            .unwrap()
            .is_none());
    }
}
