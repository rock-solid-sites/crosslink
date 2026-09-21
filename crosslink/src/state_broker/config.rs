//! Broker configuration, secret handling, and backend selection.
//!
//! # Secret placement
//!
//! The bearer token is read from the environment (or from a file whose *path*
//! is configured) and is never written back to any Crosslink file. It is
//! wrapped in [`SecretToken`], which redacts itself in `Debug` output and
//! deliberately implements neither `Display` nor `Serialize`; its value is
//! reachable only from crate-internal code (the HTTP client and redaction).
//!
//! # Backend selection
//!
//! Crosslink keeps its existing local/direct behavior unless a broker backend
//! is selected explicitly:
//!
//! 1. `CROSSLINK_STATE_BACKEND=broker` (environment), or
//! 2. `"state_backend": "broker"` in `.crosslink/hook-config.json`.
//!
//! `CROSSLINK_STATE_BACKEND=git` (or `local`) forces the existing behavior;
//! so does the absence of both keys. Selecting `broker` without a usable URL,
//! token, and project UUID is a hard configuration error — there is no silent
//! fallback, because a silent fallback would write durable state to the wrong
//! backend.
//!
//! A present-but-unparsable `hook-config.json` is a hard error **when it may
//! contain the selection key** (the raw text mentions `state_backend`): a
//! corrupt config that may have selected the broker must never silently revert
//! to Local. When the file neither parses nor mentions the key, the backend is
//! Local with a warning — the file is shared with unrelated configuration and
//! this adapter must not turn unrelated config damage into a hard failure.
//!
//! The broker environment variables match the broker's own Codex Cloud
//! contract: `CROSSLINK_STATE_BROKER_URL`, `CROSSLINK_STATE_BROKER_TOKEN`,
//! `CROSSLINK_STATE_PROJECT_UUID`, `CROSSLINK_STATE_TIMEOUT_MS`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::error::{redact_secret, BrokerErrorCode, StateBrokerError};
use super::validate::{is_canonical_uuid, is_commit_sha};

/// Environment variable holding the broker base URL.
pub const ENV_BROKER_URL: &str = "CROSSLINK_STATE_BROKER_URL";
/// Environment variable holding the broker bearer token.
pub const ENV_BROKER_TOKEN: &str = "CROSSLINK_STATE_BROKER_TOKEN";
/// Environment variable holding a path to a file containing the token.
///
/// This is the workspace-preferred secret-placement pattern: the operator puts
/// the secret on the machine and tells the agent only where it lives.
pub const ENV_BROKER_TOKEN_FILE: &str = "CROSSLINK_STATE_BROKER_TOKEN_FILE";
/// Environment variable holding the project UUID the token is bound to.
pub const ENV_PROJECT_UUID: &str = "CROSSLINK_STATE_PROJECT_UUID";
/// Environment variable overriding the per-request timeout, in milliseconds.
pub const ENV_TIMEOUT_MS: &str = "CROSSLINK_STATE_TIMEOUT_MS";
/// Environment variable selecting the state backend (`git` | `broker`).
pub const ENV_BACKEND: &str = "CROSSLINK_STATE_BACKEND";

/// `hook-config.json` key selecting the state backend.
pub const HOOK_CONFIG_KEY: &str = "state_backend";

/// Backend label for the existing local/direct behavior.
pub const BACKEND_LOCAL: &str = "git";
/// Backend label for broker-backed durable state.
pub const BACKEND_BROKER: &str = "broker";

/// Default per-request timeout (matches the broker reference client).
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;
/// Maximum accepted per-request timeout.
pub const MAX_TIMEOUT_MS: u64 = 600_000;

/// A bearer token that never renders its value.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    /// Wrap a token value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the token value. Crate-internal only: the HTTP client attaches
    /// it and [`StateBrokerConfig::redact`] needs it; nothing public can render
    /// it.
    #[must_use]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the wrapped token is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken([redacted])")
    }
}

/// Validated broker connection settings.
#[derive(Clone)]
pub struct StateBrokerConfig {
    base_url: String,
    project_uuid: String,
    token: SecretToken,
    timeout: Duration,
}

impl fmt::Debug for StateBrokerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateBrokerConfig")
            .field("base_url", &self.base_url)
            .field("project_uuid", &self.project_uuid)
            .field("token", &self.token)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl StateBrokerConfig {
    /// Validate and construct broker settings.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerErrorCode::Configuration`] for a non-http(s) URL, an
    /// `http://` URL pointing at a non-loopback host (the token would travel in
    /// cleartext), a non-canonical project UUID, an empty token, or a timeout
    /// outside the range 1 to [`MAX_TIMEOUT_MS`].
    pub fn new(
        base_url: impl Into<String>,
        project_uuid: impl Into<String>,
        token: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, StateBrokerError> {
        let base_url = normalize_base_url(&base_url.into())?;
        let project_uuid = project_uuid.into();
        if !is_canonical_uuid(&project_uuid) {
            return Err(configuration_error(
                "project uuid must be a lowercase RFC 4122 uuid",
            ));
        }
        let token = SecretToken::new(token.into());
        if token.is_empty() {
            return Err(configuration_error("broker token must not be empty"));
        }
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        if timeout.is_zero() || timeout_ms > MAX_TIMEOUT_MS {
            return Err(configuration_error(format!(
                "broker timeout must be between 1 and {MAX_TIMEOUT_MS} milliseconds"
            )));
        }
        Ok(Self {
            base_url,
            project_uuid,
            token,
            timeout,
        })
    }

    /// Build settings from environment variables.
    ///
    /// Returns `Ok(None)` when *no* broker variable is present at all — the
    /// caller then keeps the existing local/direct behavior. A partial
    /// configuration is a hard error naming only the missing variable names.
    ///
    /// # Errors
    ///
    /// See [`Self::new`], plus errors for a partially-present environment or
    /// an unreadable/empty token file.
    pub fn from_env() -> Result<Option<Self>, StateBrokerError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// [`Self::from_env`] with an injectable variable lookup (for tests).
    ///
    /// # Errors
    ///
    /// See [`Self::from_env`].
    pub fn from_lookup<F>(lookup: F) -> Result<Option<Self>, StateBrokerError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let read = |key: &str| -> Option<String> {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let base_url = read(ENV_BROKER_URL);
        let token_env = read(ENV_BROKER_TOKEN);
        let token_file = read(ENV_BROKER_TOKEN_FILE);
        let project_uuid = read(ENV_PROJECT_UUID);
        let timeout_raw = read(ENV_TIMEOUT_MS);

        if base_url.is_none()
            && token_env.is_none()
            && token_file.is_none()
            && project_uuid.is_none()
        {
            return Ok(None);
        }

        let mut missing = Vec::new();
        if base_url.is_none() {
            missing.push(ENV_BROKER_URL);
        }
        if token_env.is_none() && token_file.is_none() {
            missing.push("CROSSLINK_STATE_BROKER_TOKEN or CROSSLINK_STATE_BROKER_TOKEN_FILE");
        }
        if project_uuid.is_none() {
            missing.push(ENV_PROJECT_UUID);
        }
        if !missing.is_empty() {
            return Err(configuration_error(format!(
                "incomplete broker configuration; missing {}",
                missing.join(", ")
            )));
        }

        if token_env.is_some() && token_file.is_some() {
            return Err(configuration_error(format!(
                "set only one of {ENV_BROKER_TOKEN} and {ENV_BROKER_TOKEN_FILE}"
            )));
        }

        let token = if let Some(token) = token_env {
            token
        } else {
            let path = token_file.expect("checked: token_file is present");
            read_token_file(&path)?
        };

        let timeout = match timeout_raw {
            None => Duration::from_millis(DEFAULT_TIMEOUT_MS),
            Some(raw) => {
                let ms: u64 = raw.parse().map_err(|_| {
                    configuration_error(format!("{ENV_TIMEOUT_MS} must be a positive integer"))
                })?;
                Duration::from_millis(ms)
            }
        };

        Self::new(
            base_url.expect("checked: base_url is present"),
            project_uuid.expect("checked: project_uuid is present"),
            token,
            timeout,
        )
        .map(Some)
    }

    /// Broker base URL with any trailing slashes removed.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Project UUID the token is bound to.
    #[must_use]
    pub fn project_uuid(&self) -> &str {
        &self.project_uuid
    }

    /// Per-request timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The durable state ref this project's namespace uses.
    #[must_use]
    pub fn state_ref(&self) -> String {
        format!("refs/heads/projects/{}/state", self.project_uuid)
    }

    /// The durable state branch this project's namespace uses.
    fn state_branch(&self) -> String {
        format!("projects/{}/state", self.project_uuid)
    }

    /// Whether `value` names the project's own state ref (any accepted form).
    #[must_use]
    pub fn is_state_ref(&self, value: &str) -> bool {
        value == self.state_ref() || value == self.state_branch() || value == "state"
    }

    /// Whether `value` is an acceptable blob `ref` query value: an exact commit
    /// sha or the project's own state ref.
    #[must_use]
    pub fn is_acceptable_ref(&self, value: &str) -> bool {
        is_commit_sha(value) || self.is_state_ref(value)
    }

    /// Borrow the token for the HTTP client.
    pub(crate) fn token(&self) -> &str {
        self.token.expose()
    }

    /// Redact the token from arbitrary text (defence in depth before any error
    /// or log line is constructed from broker output).
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        redact_secret(text, self.token.expose())
    }
}

/// Which durable-state backend Crosslink should use.
///
/// `Local` is the existing behavior (local git hub refs + tracker remote) and
/// is the default; this adapter never changes it implicitly.
#[derive(Clone)]
pub enum StateBackend {
    /// Existing local/direct behavior. This adapter does not intercept it.
    Local,
    /// Broker-backed durable state.
    Broker(StateBrokerConfig),
}

impl fmt::Debug for StateBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("StateBackend::Local"),
            Self::Broker(config) => f.debug_tuple("StateBackend::Broker").field(config).finish(),
        }
    }
}

impl StateBackend {
    /// Resolve the configured backend using the process environment and
    /// `.crosslink/hook-config.json`.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerErrorCode::Configuration`] when the selected backend is
    /// `broker` but its settings are missing/invalid, or when the configured
    /// backend name is unknown.
    pub fn resolve(crosslink_dir: &Path) -> Result<Self, StateBrokerError> {
        Self::resolve_with(crosslink_dir, |key| std::env::var(key).ok())
    }

    /// [`Self::resolve`] with an injectable environment lookup (for tests).
    ///
    /// # Errors
    ///
    /// See [`Self::resolve`].
    pub fn resolve_with<F>(crosslink_dir: &Path, lookup: F) -> Result<Self, StateBrokerError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let env_value = lookup(ENV_BACKEND)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let configured = match env_value {
            Some(value) => Some(value),
            None => read_hook_config_backend(crosslink_dir)?,
        };

        match configured.as_deref() {
            None | Some(BACKEND_LOCAL | "local" | "direct") => Ok(Self::Local),
            Some(BACKEND_BROKER) => StateBrokerConfig::from_lookup(&lookup)?.map_or_else(
                || {
                    Err(configuration_error(format!(
                        "state_backend=broker is selected but no broker settings are present; \
                         set {ENV_BROKER_URL}, {ENV_BROKER_TOKEN} (or {ENV_BROKER_TOKEN_FILE}), \
                         and {ENV_PROJECT_UUID}"
                    )))
                },
                |config| Ok(Self::Broker(config)),
            ),
            Some(other) => Err(configuration_error(format!(
                "unknown state backend {other:?}; expected \"{BACKEND_LOCAL}\" or \"{BACKEND_BROKER}\""
            ))),
        }
    }

    /// Stable label for logs and reports: `"local"` or `"broker"`.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Broker(_) => "broker",
        }
    }

    /// The broker settings when the broker backend is selected.
    #[must_use]
    pub const fn broker(&self) -> Option<&StateBrokerConfig> {
        match self {
            Self::Broker(config) => Some(config),
            Self::Local => None,
        }
    }
}

/// Default root for a disposable broker state projection inside a Crosslink
/// directory. The projection is a cache, never a source of truth: deleting it
/// loses nothing.
#[must_use]
pub fn default_projection_dir(crosslink_dir: &Path) -> PathBuf {
    crosslink_dir.join("state-projection")
}

fn configuration_error(message: impl Into<String>) -> StateBrokerError {
    // `configuration` is the client-side class; keep the constructor in one
    // place so every configuration failure has the same code and retryability.
    let error = StateBrokerError::configuration(message);
    debug_assert_eq!(error.code(), BrokerErrorCode::Configuration);
    error
}

/// Normalize and validate a broker base URL.
fn normalize_base_url(raw: &str) -> Result<String, StateBrokerError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(configuration_error("broker URL must not be empty"));
    }
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| configuration_error("broker URL must start with http:// or https://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return Err(configuration_error("broker URL must include a host"));
    }
    match scheme {
        "https" => Ok(trimmed.to_string()),
        "http" => {
            let host_only = host.rsplit('@').next().unwrap_or(host);
            let host_only = host_only.split(':').next().unwrap_or(host_only);
            let loopback = matches!(host_only, "localhost" | "127.0.0.1" | "[::1]" | "::1");
            if loopback {
                Ok(trimmed.to_string())
            } else {
                Err(configuration_error(
                    "refusing a plain-http broker URL for a non-loopback host: \
                     the bearer token would be sent in cleartext",
                ))
            }
        }
        _ => Err(configuration_error(
            "broker URL must start with http:// or https://",
        )),
    }
}

/// Read a token from `path`, trimming surrounding whitespace.
fn read_token_file(path: &str) -> Result<String, StateBrokerError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| configuration_error(format!("cannot read broker token file {path:?}: {e}")))?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        return Err(configuration_error(format!(
            "broker token file {path:?} is empty"
        )));
    }
    Ok(token)
}

/// Read the optional `state_backend` key from `.crosslink/hook-config.json`.
///
/// A **missing** file or key yields `None` (Local). A non-string value is a
/// configuration error. An unparsable file is a hard configuration error when
/// the raw text mentions the selection key — the file may have selected the
/// broker and falling back to Local would silently route durable state to the
/// wrong backend. When the raw text does not mention the key, the file cannot
/// have selected a backend, so the adapter warns and yields `None`.
///
/// An **existing but unreadable** file (permissions, I/O error, invalid UTF-8,
/// or a directory at that path) is always a hard configuration error: the file
/// could contain a broker selection and must never silently degrade to Local.
fn read_hook_config_backend(crosslink_dir: &Path) -> Result<Option<String>, StateBrokerError> {
    let path = crosslink_dir.join("hook-config.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(configuration_error(format!(
                "cannot read {} ({error}); refusing to fall back to Local because the file \
                 may select {HOOK_CONFIG_KEY}",
                path.display()
            )));
        }
    };
    let value = match serde_json::from_str::<Value>(&raw) {
        Ok(value) => value,
        Err(error) => {
            if raw.contains(HOOK_CONFIG_KEY) {
                return Err(configuration_error(format!(
                    "{} is not valid JSON and may select {HOOK_CONFIG_KEY}; refusing to \
                     silently fall back to Local: {error}",
                    path.display()
                )));
            }
            tracing::warn!(
                "{} is not valid JSON; ignoring {HOOK_CONFIG_KEY} (key not present)",
                path.display()
            );
            return Ok(None);
        }
    };
    match value.get(HOOK_CONFIG_KEY) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Some(other) => Err(configuration_error(format!(
            "{HOOK_CONFIG_KEY} in hook-config.json must be a string, found {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "1d440dcf-bcbf-4d1a-987c-d5334568a716";
    const TOKEN: &str = "broker-token-abcdefghijklmnop";

    fn lookup_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn complete_env_builds_config() {
        let config = StateBrokerConfig::from_lookup(lookup_from(&[
            (ENV_BROKER_URL, "https://broker.example.workers.dev/"),
            (ENV_BROKER_TOKEN, TOKEN),
            (ENV_PROJECT_UUID, UUID),
        ]))
        .unwrap()
        .expect("config present");
        assert_eq!(config.base_url(), "https://broker.example.workers.dev");
        assert_eq!(config.project_uuid(), UUID);
        assert_eq!(config.timeout(), Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(
            config.state_ref(),
            format!("refs/heads/projects/{UUID}/state")
        );
        assert_eq!(config.state_branch(), format!("projects/{UUID}/state"));
        assert!(config.is_state_ref("state"));
        assert!(config.is_state_ref(&config.state_ref()));
    }

    #[test]
    fn empty_env_yields_none() {
        let empty: &[(&str, &str)] = &[];
        assert!(StateBrokerConfig::from_lookup(lookup_from(empty))
            .unwrap()
            .is_none());
    }

    #[test]
    fn partial_env_is_a_hard_error_naming_variables() {
        let error = StateBrokerConfig::from_lookup(lookup_from(&[(
            ENV_BROKER_URL,
            "https://broker.example",
        )]))
        .unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::Configuration);
        assert!(error.message().contains(ENV_BROKER_TOKEN));
        assert!(error.message().contains(ENV_PROJECT_UUID));
    }

    #[test]
    fn token_file_is_supported_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, format!("  {TOKEN}\n")).unwrap();
        let config = StateBrokerConfig::from_lookup(lookup_from(&[
            (ENV_BROKER_URL, "https://broker.example"),
            (ENV_BROKER_TOKEN_FILE, token_path.to_str().unwrap()),
            (ENV_PROJECT_UUID, UUID),
        ]))
        .unwrap()
        .expect("config present");
        assert_eq!(config.token(), TOKEN);
    }

    #[test]
    fn token_env_and_file_together_is_rejected() {
        let error = StateBrokerConfig::from_lookup(lookup_from(&[
            (ENV_BROKER_URL, "https://broker.example"),
            (ENV_BROKER_TOKEN, TOKEN),
            (ENV_BROKER_TOKEN_FILE, "/nonexistent"),
            (ENV_PROJECT_UUID, UUID),
        ]))
        .unwrap_err();
        assert!(error.message().contains("only one"));
    }

    #[test]
    fn plain_http_non_loopback_is_rejected() {
        let error = StateBrokerConfig::new(
            "http://broker.example.com",
            UUID,
            TOKEN,
            Duration::from_secs(15),
        )
        .unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::Configuration);
        assert!(error.message().contains("cleartext"));
    }

    #[test]
    fn plain_http_loopback_is_allowed() {
        let config = StateBrokerConfig::new(
            "http://127.0.0.1:8787",
            UUID,
            TOKEN,
            Duration::from_secs(15),
        )
        .unwrap();
        assert_eq!(config.base_url(), "http://127.0.0.1:8787");
    }

    #[test]
    fn debug_and_redact_never_reveal_the_token() {
        let config = StateBrokerConfig::new(
            "https://broker.example",
            UUID,
            TOKEN,
            Duration::from_secs(15),
        )
        .unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains(TOKEN), "Debug must redact the token");
        assert!(debug.contains("redacted"));
        assert_eq!(
            config.redact(&format!("upstream said {TOKEN}")),
            "upstream said [redacted]"
        );
    }

    #[test]
    fn invalid_uuid_and_timeout_are_rejected() {
        assert!(StateBrokerConfig::new(
            "https://broker.example",
            "NOT-A-UUID",
            TOKEN,
            Duration::from_secs(15)
        )
        .is_err());
        assert!(
            StateBrokerConfig::new("https://broker.example", UUID, TOKEN, Duration::ZERO).is_err()
        );
        assert!(StateBrokerConfig::new(
            "https://broker.example",
            UUID,
            TOKEN,
            Duration::from_millis(MAX_TIMEOUT_MS + 1)
        )
        .is_err());
    }

    #[test]
    fn backend_default_is_local_even_with_broker_env_present() {
        // Preserve existing behavior: broker env alone must not switch backends.
        let backend = StateBackend::resolve_with(
            Path::new("/nonexistent-crosslink-dir"),
            lookup_from(&[
                (ENV_BROKER_URL, "https://broker.example"),
                (ENV_BROKER_TOKEN, TOKEN),
                (ENV_PROJECT_UUID, UUID),
            ]),
        )
        .unwrap();
        assert_eq!(backend.label(), "local");
        assert!(backend.broker().is_none());
    }

    #[test]
    fn backend_selection_from_env_and_hook_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            format!("{{\"{HOOK_CONFIG_KEY}\": \"broker\"}}"),
        )
        .unwrap();

        let backend = StateBackend::resolve_with(
            dir.path(),
            lookup_from(&[
                (ENV_BROKER_URL, "https://broker.example"),
                (ENV_BROKER_TOKEN, TOKEN),
                (ENV_PROJECT_UUID, UUID),
            ]),
        )
        .unwrap();
        assert_eq!(backend.label(), "broker");
        assert_eq!(backend.broker().unwrap().project_uuid(), UUID);

        // Environment overrides the hook-config value.
        let backend =
            StateBackend::resolve_with(dir.path(), lookup_from(&[(ENV_BACKEND, "git")])).unwrap();
        assert_eq!(backend.label(), "local");
    }

    #[test]
    fn broker_selected_without_settings_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = StateBackend::resolve_with(dir.path(), lookup_from(&[(ENV_BACKEND, "broker")]))
            .unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::Configuration);
        assert!(error.message().contains(ENV_BROKER_URL));
    }

    /// A corrupt shared config file that may have selected the broker must not
    /// silently revert to Local: that would route durable state to the wrong
    /// backend.
    #[test]
    fn corrupt_hook_config_mentioning_the_key_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            "{\"state_backend\": \"broker\", \"other\": ",
        )
        .unwrap();
        let error = StateBackend::resolve_with(dir.path(), lookup_from(&[])).unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::Configuration);
        assert!(
            error.message().contains("refusing"),
            "error must say it refuses to fall back: {}",
            error.message()
        );

        // An explicit environment selection still wins (the corrupt file is
        // never consulted), and it must not be blocked by the corrupt file.
        let backend =
            StateBackend::resolve_with(dir.path(), lookup_from(&[(ENV_BACKEND, "git")])).unwrap();
        assert_eq!(backend.label(), "local");
    }

    /// A corrupt file that cannot contain the selection key stays fail-safe to
    /// Local (the file is shared with unrelated configuration).
    #[test]
    fn corrupt_hook_config_without_the_key_stays_local() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hook-config.json"), "{ this is not json ").unwrap();
        let backend = StateBackend::resolve_with(dir.path(), lookup_from(&[])).unwrap();
        assert_eq!(backend.label(), "local");
    }

    #[test]
    fn non_string_selection_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            "{\"state_backend\": 7}",
        )
        .unwrap();
        let error = StateBackend::resolve_with(dir.path(), lookup_from(&[])).unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::Configuration);
        assert!(error.message().contains(HOOK_CONFIG_KEY));
    }

    /// A missing file means "no selection" and stays Local.
    #[test]
    fn missing_hook_config_stays_local() {
        let dir = tempfile::tempdir().unwrap();
        let backend = StateBackend::resolve_with(dir.path(), lookup_from(&[])).unwrap();
        assert_eq!(backend.label(), "local");
    }

    /// An existing file that cannot be decoded could have selected the broker:
    /// it must fail hard, never silently become Local.
    #[test]
    fn non_utf8_hook_config_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            [0xff, 0xfe, 0x00, 0x80, b'{'],
        )
        .unwrap();
        let error = StateBackend::resolve_with(dir.path(), lookup_from(&[])).unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::Configuration);
        assert!(
            error.message().contains("refusing"),
            "error must say it refuses to fall back: {}",
            error.message()
        );
    }

    #[test]
    fn directory_hook_config_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("hook-config.json")).unwrap();
        let error = StateBackend::resolve_with(dir.path(), lookup_from(&[])).unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::Configuration);
    }

    /// Permission-denied is the same class as invalid UTF-8: an existing config
    /// that cannot be read must not degrade to Local. Skipped when the process
    /// can read the file regardless of mode (e.g. running as root).
    #[cfg(unix)]
    #[test]
    fn unreadable_hook_config_is_a_hard_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook-config.json");
        std::fs::write(&path, format!("{{\"{HOOK_CONFIG_KEY}\": \"broker\"}}")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&path, permissions).unwrap();
        if std::fs::read_to_string(&path).is_ok() {
            // Privileges ignore file modes here; the unreadable path cannot be
            // exercised in this environment.
            return;
        }
        let error = StateBackend::resolve_with(dir.path(), lookup_from(&[])).unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::Configuration);
    }

    #[test]
    fn unknown_backend_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let error = StateBackend::resolve_with(dir.path(), lookup_from(&[(ENV_BACKEND, "sqlite")]))
            .unwrap_err();
        assert!(error.message().contains("unknown state backend"));
    }
}
