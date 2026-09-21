//! Client-side mirror of the broker's input validation.
//!
//! Rejecting invalid input locally keeps malformed requests off the network and
//! gives deterministic unit tests without a broker. The rules here MUST stay in
//! sync with the broker's `src/paths.ts` (`assertLogicalPath`) and
//! `src/state.ts` (`LIMITS`) — the broker remains authoritative; a mismatch
//! only means the broker rejects something this module accepted (never the
//! reverse).

use super::error::StateBrokerError;

/// Maximum files in one commit (broker `LIMITS.maxFilesPerCommit`).
pub const MAX_FILES_PER_COMMIT: usize = 32;
/// Maximum bytes per state file (broker `LIMITS.maxFileBytes`).
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// Maximum total bytes per commit (broker `LIMITS.maxTotalBytes`).
pub const MAX_TOTAL_BYTES: usize = 1024 * 1024;
/// Maximum characters in a caller commit message (broker `LIMITS.maxMessageLength`).
pub const MAX_MESSAGE_LENGTH: usize = 512;
/// Maximum paths per verify request (broker `LIMITS.maxVerifyPaths`).
pub const MAX_VERIFY_PATHS: usize = 32;
/// Maximum logical path length (broker `MAX_PATH_LENGTH`).
pub const MAX_PATH_LENGTH: usize = 256;
/// Maximum path segments (broker `MAX_SEGMENTS`).
pub const MAX_PATH_SEGMENTS: usize = 16;
/// Maximum length of a single path segment (broker `MAX_SEGMENT_LENGTH`).
pub const MAX_SEGMENT_LENGTH: usize = 64;
/// Maximum op id length (broker op id rule).
pub const MAX_OP_ID_LENGTH: usize = 128;

fn invalid(message: impl Into<String>) -> StateBrokerError {
    StateBrokerError::invalid_input(message)
}

/// Whether `segment` matches the broker's `SEGMENT_RE`
/// (`^[A-Za-z0-9._][A-Za-z0-9._-]*$`).
fn segment_is_valid(segment: &str) -> bool {
    if segment.is_empty() || segment == "." || segment == ".." {
        return false;
    }
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphanumeric() || first == '.' || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Validate a project-relative logical state path against the broker rules.
///
/// # Errors
///
/// Returns [`BrokerErrorCode::InvalidInput`] when the path is empty, too long,
/// too deeply nested, absolute, contains backslashes/NUL, or has an invalid
/// segment.
pub fn validate_logical_path(path: &str) -> Result<(), StateBrokerError> {
    if path.is_empty() || path.len() > MAX_PATH_LENGTH {
        return Err(invalid(format!(
            "logical path must be a non-empty string of at most {MAX_PATH_LENGTH} characters"
        )));
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err(invalid("logical path must not start or end with '/'"));
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(invalid(
            "logical path must not contain backslashes or NUL bytes",
        ));
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() > MAX_PATH_SEGMENTS {
        return Err(invalid(format!(
            "logical path must have at most {MAX_PATH_SEGMENTS} segments"
        )));
    }
    for segment in &segments {
        if segment.len() > MAX_SEGMENT_LENGTH || !segment_is_valid(segment) {
            return Err(invalid(format!(
                "logical path segment {segment:?} must be 1-{MAX_SEGMENT_LENGTH} characters of \
                 [A-Za-z0-9._-] and not '.' or '..'"
            )));
        }
    }
    Ok(())
}

/// Validate a caller commit message against the broker rules.
///
/// The broker requires a single non-empty line of at most
/// [`MAX_MESSAGE_LENGTH`] characters with no control characters, and forbids a
/// message that begins with a broker trailer key (`Project-UUID:`, `Broker:`,
/// `Broker-Op:`).
///
/// # Errors
///
/// Returns [`BrokerErrorCode::InvalidInput`] on any violation.
pub fn validate_message(message: &str) -> Result<(), StateBrokerError> {
    if message.trim().is_empty() {
        return Err(invalid("commit message must not be empty"));
    }
    if message.chars().count() > MAX_MESSAGE_LENGTH {
        return Err(invalid(format!(
            "commit message must be at most {MAX_MESSAGE_LENGTH} characters"
        )));
    }
    // Exact broker rule: reject C0 controls and DEL only (`state.ts`:
    // `/[\u0000-\u001F\u007F]/`). C1 controls are accepted by the broker and
    // must not be rejected locally.
    if message
        .chars()
        .any(|c| c == '\u{7f}' || ('\u{0}'..='\u{1f}').contains(&c))
    {
        return Err(invalid(
            "commit message must be a single line with no control characters",
        ));
    }
    let trimmed = message.trim();
    for trailer in ["Project-UUID:", "Broker:", "Broker-Op:"] {
        if trimmed.len() >= trailer.len() && trimmed[..trailer.len()].eq_ignore_ascii_case(trailer)
        {
            return Err(invalid(
                "commit message must not begin with a broker trailer key",
            ));
        }
    }
    Ok(())
}

/// Validate an optional op id against the broker rule
/// (`[A-Za-z0-9._:-]{1,128}`).
///
/// # Errors
///
/// Returns [`BrokerErrorCode::InvalidInput`] on violation.
pub fn validate_op_id(op_id: &str) -> Result<(), StateBrokerError> {
    if op_id.is_empty() || op_id.len() > MAX_OP_ID_LENGTH {
        return Err(invalid(format!(
            "op_id must be 1-{MAX_OP_ID_LENGTH} characters"
        )));
    }
    if !op_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
    {
        return Err(invalid("op_id may only contain [A-Za-z0-9._:-] characters"));
    }
    Ok(())
}

/// Whether `value` is an exact 40-character lowercase hex commit sha.
#[must_use]
pub fn is_commit_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Validate a commit sha.
///
/// # Errors
///
/// Returns [`BrokerErrorCode::InvalidInput`] unless `value` is an exact
/// 40-character lowercase hex commit sha.
pub fn validate_commit_sha(value: &str) -> Result<(), StateBrokerError> {
    if is_commit_sha(value) {
        Ok(())
    } else {
        Err(invalid(
            "commit must be an exact 40-character lowercase hex commit sha",
        ))
    }
}

/// Whether `value` is a lowercase canonical RFC 4122 UUID.
#[must_use]
pub fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

/// Validate that `value` is a lowercase canonical RFC 4122 UUID.
///
/// # Errors
///
/// Returns [`BrokerErrorCode::InvalidInput`] otherwise.
pub fn validate_project_uuid(value: &str) -> Result<(), StateBrokerError> {
    if is_canonical_uuid(value) {
        Ok(())
    } else {
        Err(invalid("project uuid must be a lowercase RFC 4122 uuid"))
    }
}

/// Map an already-validated logical path onto a path relative to a projection
/// root, refusing anything that could escape the root.
///
/// The broker path rules already exclude `..`, absolute paths, backslashes,
/// and NUL; this function re-checks them and additionally rejects
/// Windows-reserved file names so a projection is portable.
///
/// # Errors
///
/// Returns [`BrokerErrorCode::InvalidInput`] for an unsafe path.
pub fn projection_relative_path(path: &str) -> Result<std::path::PathBuf, StateBrokerError> {
    validate_logical_path(path)?;
    let mut out = std::path::PathBuf::new();
    for segment in path.split('/') {
        if crate::utils::is_windows_reserved_name(segment) {
            return Err(invalid(format!(
                "logical path segment {segment:?} is a reserved file name"
            )));
        }
        out.push(segment);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_broker::error::BrokerErrorCode;

    #[test]
    fn accepts_typical_broker_paths() {
        for path in [
            "issues/1d440dcf-bcbf-4d1a-987c-d5334568a716.json",
            "meta/counters.json",
            "agents/agent-1/events.log",
            "checkpoints/2026-09-21.json",
            "a.b/c_d-e/f",
        ] {
            validate_logical_path(path).unwrap_or_else(|e| panic!("{path} should be valid: {e}"));
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "",
            "/absolute",
            "trailing/",
            "a//b",
            "..",
            "../escape",
            "a/../../escape",
            "a\\b",
            "a/./b",
            "a/ b",
            "curly/{}",
            &"x".repeat(MAX_PATH_LENGTH + 1),
            &std::iter::repeat_n("segment", MAX_PATH_SEGMENTS + 1)
                .collect::<Vec<_>>()
                .join("/"),
            &format!("{}/{}", "a".repeat(MAX_SEGMENT_LENGTH + 1), "b"),
        ] {
            assert!(
                validate_logical_path(path).is_err(),
                "path {path:?} must be rejected"
            );
        }
    }

    #[test]
    fn projection_paths_stay_relative() {
        let path = projection_relative_path("issues/x.json").unwrap();
        assert_eq!(path, std::path::Path::new("issues").join("x.json"));
        assert!(projection_relative_path("../escape").is_err());
        assert!(projection_relative_path("aux/name.json").is_err());
    }

    #[test]
    fn message_rules_mirror_the_contract() {
        validate_message("checkpoint: durable state probe").unwrap();
        assert!(validate_message("").is_err());
        assert!(validate_message("   ").is_err());
        assert!(validate_message("two\nlines").is_err());
        assert!(validate_message("tab\there").is_err());
        // Exact broker rule: C1 controls are accepted (rejected only by
        // `char::is_control`, which is why this test exists).
        validate_message("c1 \u{85} accepted").unwrap();
        assert!(validate_message("Project-UUID: spoof").is_err());
        assert!(validate_message("broker-op: spoof").is_err());
        assert!(validate_message(&"x".repeat(MAX_MESSAGE_LENGTH + 1)).is_err());
        validate_message(&"x".repeat(MAX_MESSAGE_LENGTH)).unwrap();
    }

    #[test]
    fn commit_sha_and_uuid_rules_are_exact() {
        let sha = "94fa0e38ae68c95e13d91226e74e6d4f6f1524dd";
        validate_commit_sha(sha).unwrap();
        assert!(validate_commit_sha(&sha.to_uppercase()).is_err());
        assert!(validate_commit_sha("deadbeef").is_err());

        let uuid = "1d440dcf-bcbf-4d1a-987c-d5334568a716";
        validate_project_uuid(uuid).unwrap();
        assert!(validate_project_uuid(&uuid.to_uppercase()).is_err());
        assert!(validate_project_uuid("not-a-uuid").is_err());
    }

    #[test]
    fn op_id_rules_mirror_the_contract() {
        validate_op_id("2026-09-21T01:40Z-1").unwrap();
        assert!(validate_op_id("").is_err());
        assert!(validate_op_id("has space").is_err());
        assert!(validate_op_id(&"x".repeat(MAX_OP_ID_LENGTH + 1)).is_err());
    }

    #[test]
    fn error_code_of_rejections_is_invalid_input() {
        let error = validate_logical_path("../escape").unwrap_err();
        assert_eq!(error.code(), BrokerErrorCode::InvalidInput);
        assert_eq!(error.code().as_str(), "invalid_input");
    }
}
