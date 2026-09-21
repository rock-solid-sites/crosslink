//! Typed failures for the Crosslink State Broker client.
//!
//! The broker's error contract is documented in
//! `crosslink-state-broker/docs`/`README.md` (`error.code` values are part of
//! the public contract). This module mirrors that contract and adds the three
//! client-side failure classes Crosslink can hit before a broker envelope
//! exists: `transport`, `protocol`, and `configuration`.
//!
//! # Secret safety
//!
//! Every constructor takes already-redacted text. Callers building errors from
//! broker or network responses MUST pass the text through
//! [`redact_secret`] (or [`crate::state_broker::config::StateBrokerConfig::redact`])
//! first. The bearer token is never stored on an error, never formatted into
//! [`std::fmt::Display`], and never included in the `Debug` output.

use serde_json::Value;
use std::fmt;

/// Minimum secret length considered for redaction. Shorter values are not
/// substituted (they would corrupt unrelated text) — real broker tokens are
/// 32+ characters.
const MIN_REDACTABLE_SECRET_LEN: usize = 8;

/// Replacement marker for a redacted secret.
pub const REDACTED: &str = "[redacted]";

/// Broker contract error code, plus the client-side failure classes.
///
/// The broker codes (`unauthorized` … `internal_error`) round-trip through
/// [`Self::as_str`] / [`Self::from_contract_str`] and MUST NOT be renamed
/// without a broker contract version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrokerErrorCode {
    /// HTTP 401 — missing or invalid broker credential.
    Unauthorized,
    /// HTTP 403 — token not bound to this project, or missing scope.
    ScopeViolation,
    /// HTTP 400/413 — request rejected by the broker's input validation.
    InvalidInput,
    /// HTTP 404 — operation or state file not found.
    NotFound,
    /// HTTP 409 — compare-and-swap conflict; nothing was written.
    StaleState,
    /// HTTP 405 — wrong HTTP method for the operation.
    MethodNotAllowed,
    /// HTTP 502 — broker's GitHub backend failed, or read-back disagreed.
    UpstreamError,
    /// HTTP 500 — internal broker error.
    InternalError,
    /// Client-side: the request never produced a broker envelope (DNS,
    /// connect, TLS, timeout, connection reset).
    Transport,
    /// Client-side: a response arrived but was not a valid broker envelope.
    Protocol,
    /// Client-side: the local configuration is incomplete or invalid.
    Configuration,
    /// Client-side: local filesystem work failed (e.g. writing a projection).
    LocalIo,
}

impl BrokerErrorCode {
    /// Every variant.
    pub const ALL: &'static [Self] = &[
        Self::Unauthorized,
        Self::ScopeViolation,
        Self::InvalidInput,
        Self::NotFound,
        Self::StaleState,
        Self::MethodNotAllowed,
        Self::UpstreamError,
        Self::InternalError,
        Self::Transport,
        Self::Protocol,
        Self::Configuration,
        Self::LocalIo,
    ];

    /// The subset that is part of the broker's public wire contract.
    pub const CONTRACT_CODES: &'static [Self] = &[
        Self::Unauthorized,
        Self::ScopeViolation,
        Self::InvalidInput,
        Self::NotFound,
        Self::StaleState,
        Self::MethodNotAllowed,
        Self::UpstreamError,
        Self::InternalError,
    ];

    /// The wire/contract spelling of this code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::ScopeViolation => "scope_violation",
            Self::InvalidInput => "invalid_input",
            Self::NotFound => "not_found",
            Self::StaleState => "stale_state",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::UpstreamError => "upstream_error",
            Self::InternalError => "internal_error",
            Self::Transport => "transport",
            Self::Protocol => "protocol",
            Self::Configuration => "configuration",
            Self::LocalIo => "local_io",
        }
    }

    /// Parse a broker contract error code. Unknown codes return `None` so the
    /// caller can surface them as a protocol violation instead of guessing.
    #[must_use]
    pub fn from_contract_str(value: &str) -> Option<Self> {
        match value {
            "unauthorized" => Some(Self::Unauthorized),
            "scope_violation" => Some(Self::ScopeViolation),
            "invalid_input" => Some(Self::InvalidInput),
            "not_found" => Some(Self::NotFound),
            "stale_state" => Some(Self::StaleState),
            "method_not_allowed" => Some(Self::MethodNotAllowed),
            "upstream_error" => Some(Self::UpstreamError),
            "internal_error" => Some(Self::InternalError),
            _ => None,
        }
    }

    /// Default retryability for a code when no envelope `retryable` field is
    /// available. `stale_state` is retryable *only* through the reconciled
    /// CAS path ([`crate::state_broker::ProjectStateTransport::commit_cas`]);
    /// a blind retry of a write is never allowed.
    #[must_use]
    pub const fn default_retryable(self) -> bool {
        matches!(self, Self::StaleState | Self::Transport | Self::UpstreamError)
    }
}

impl fmt::Display for BrokerErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failure from the state broker transport, carrying the typed contract code.
#[derive(Debug, Clone)]
pub struct StateBrokerError {
    code: BrokerErrorCode,
    message: String,
    retryable: bool,
    details: Option<Value>,
    http_status: Option<u16>,
    request_id: Option<String>,
    operation: Option<String>,
}

impl StateBrokerError {
    /// Typed error from a broker failure envelope (already redacted).
    pub(crate) fn from_envelope(
        code: BrokerErrorCode,
        message: String,
        retryable: bool,
        details: Option<Value>,
        http_status: Option<u16>,
        request_id: Option<String>,
        operation: Option<String>,
    ) -> Self {
        Self {
            code,
            message,
            retryable,
            details,
            http_status,
            request_id,
            operation,
        }
    }

    /// Client-side configuration failure (missing/invalid broker settings).
    #[must_use]
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::client(BrokerErrorCode::Configuration, message.into())
    }

    /// Client-side transport failure (no broker envelope was produced).
    #[must_use]
    pub fn transport(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            retryable,
            ..Self::client(BrokerErrorCode::Transport, message.into())
        }
    }

    /// Client-side protocol failure (response was not a valid broker envelope).
    #[must_use]
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::client(BrokerErrorCode::Protocol, message.into())
    }

    /// Client-side input rejection mirroring the broker's limits.
    #[must_use]
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::client(BrokerErrorCode::InvalidInput, message.into())
    }

    /// Client-side "not found" without contacting the broker.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::client(BrokerErrorCode::NotFound, message.into())
    }

    /// Client-side local filesystem failure.
    #[must_use]
    pub fn local_io(message: impl Into<String>) -> Self {
        Self::client(BrokerErrorCode::LocalIo, message.into())
    }

    fn client(code: BrokerErrorCode, message: String) -> Self {
        Self {
            code,
            message,
            retryable: code.default_retryable(),
            details: None,
            http_status: None,
            request_id: None,
            operation: None,
        }
    }

    /// The typed contract code.
    #[must_use]
    pub const fn code(&self) -> BrokerErrorCode {
        self.code
    }

    /// Human-readable, already-redacted failure description.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the broker marked (or the client classifies) this failure as
    /// retryable. `stale_state` implies "retry through `commit_cas`", never a
    /// blind write retry.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// Broker-supplied `details` object, if any (redacted).
    #[must_use]
    pub const fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }

    /// HTTP status, when a response was received.
    #[must_use]
    pub const fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    /// Broker `request_id`, when a response envelope was received.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Broker operation label from the envelope, when present.
    #[must_use]
    pub fn operation(&self) -> Option<&str> {
        self.operation.as_deref()
    }

    /// True for HTTP 409 `stale_state` (CAS conflict; nothing was written).
    #[must_use]
    pub const fn is_stale_state(&self) -> bool {
        matches!(self.code, BrokerErrorCode::StaleState)
    }

    /// True when the broker's read-back disagreed with the write. The write
    /// may have landed — reconcile before retrying.
    #[must_use]
    pub fn is_readback_mismatch(&self) -> bool {
        self.code == BrokerErrorCode::UpstreamError
            && self
                .details
                .as_ref()
                .and_then(|d| d.get("failed_paths"))
                .is_some()
    }
}

impl fmt::Display for StateBrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "state broker {}: {}", self.code, self.message)?;
        if let Some(status) = self.http_status {
            write!(f, " (http {status})")?;
        }
        if let Some(request_id) = &self.request_id {
            write!(f, " [request {request_id}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for StateBrokerError {}

/// Replace every occurrence of `secret` in `text` with [`REDACTED`].
///
/// Secrets shorter than [`MIN_REDACTABLE_SECRET_LEN`] are left untouched —
/// substituting them would corrupt unrelated text, and no real broker token is
/// that short. This is defence in depth: correct call sites never put the
/// token in an error in the first place.
#[must_use]
pub fn redact_secret(text: &str, secret: &str) -> String {
    if secret.len() < MIN_REDACTABLE_SECRET_LEN || text.is_empty() {
        return text.to_string();
    }
    text.replace(secret, REDACTED)
}

/// Recursively redact every string in a JSON value.
#[must_use]
pub(crate) fn redact_value(value: &Value, secret: &str) -> Value {
    match value {
        Value::String(s) => Value::String(redact_secret(s, secret)),
        Value::Array(items) => Value::Array(items.iter().map(|v| redact_value(v, secret)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_value(v, secret)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Convenience alias used across the broker client surface.
pub type BrokerResult<T> = Result<T, StateBrokerError>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn contract_codes_round_trip() {
        for code in BrokerErrorCode::CONTRACT_CODES {
            assert_eq!(
                BrokerErrorCode::from_contract_str(code.as_str()),
                Some(*code),
                "code {code} must round-trip"
            );
        }
        assert_eq!(BrokerErrorCode::from_contract_str("nonsense"), None);
    }

    #[test]
    fn stale_state_is_typed_and_retryable() {
        let error = StateBrokerError::from_envelope(
            BrokerErrorCode::StaleState,
            "head moved".to_string(),
            true,
            Some(json!({"observed_head": "abc"})),
            Some(409),
            Some("req-1".to_string()),
            Some("state.commit".to_string()),
        );
        assert!(error.is_stale_state());
        assert!(error.retryable());
        assert_eq!(error.http_status(), Some(409));
        assert_eq!(error.request_id(), Some("req-1"));
    }

    #[test]
    fn redaction_replaces_long_secrets_only() {
        let token = "broker-token-abcdefghijklmnop";
        let text = format!("failed with {token} present");
        assert_eq!(
            redact_secret(&text, token),
            format!("failed with {REDACTED} present")
        );
        assert_eq!(redact_secret("short", "abc"), "short");
    }

    #[test]
    fn redaction_walks_json_values() {
        let token = "broker-token-abcdefghijklmnop";
        let value = json!({"message": format!("leak {token}"), "nested": [token, 1]});
        let redacted = redact_value(&value, token);
        assert!(!redacted.to_string().contains(token));
        assert!(redacted.to_string().contains(REDACTED));
    }

    #[test]
    fn display_never_contains_credentials() {
        let error = StateBrokerError::configuration(
            "set CROSSLINK_STATE_BROKER_TOKEN (value not shown)",
        );
        let rendered = format!("{error} {error:?}");
        assert!(rendered.contains("configuration"));
        assert!(!rendered.contains("Bearer"));
    }
}
