//! Cross-cutting unit tests for the broker transport: disposable projections,
//! reconciled CAS, and feeding the existing hydration path from a broker
//! projection.

use std::path::Path;

use chrono::Utc;
use uuid::Uuid;

use super::client::CommitRequest;
use super::config::StateBrokerConfig;
use super::error::{BrokerErrorCode, StateBrokerError};
use super::mock::MockStateTransport;
use super::transport::ProjectStateTransport;

const UUID: &str = "1d440dcf-bcbf-4d1a-987c-d5334568a716";

fn bootstrap_mock() -> MockStateTransport {
    MockStateTransport::with_files(
        UUID,
        [
            (
                "meta/counters.json",
                br#"{"next_display_id":2,"next_comment_id":1}"#.to_vec(),
            ),
            ("checkpoints/first.json", br#"{"ok":true}"#.to_vec()),
        ],
    )
}

#[test]
fn hydrate_into_writes_a_disposable_projection() {
    let mock = bootstrap_mock();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state-projection");

    let report = mock.hydrate_into(&root, None).expect("hydrate");
    assert_eq!(report.commit.as_deref(), mock.head().as_deref());
    assert_eq!(report.files.len(), 2);
    assert!(report.bytes > 0);
    assert_eq!(
        std::fs::read(root.join("meta/counters.json")).unwrap(),
        br#"{"next_display_id":2,"next_comment_id":1}"#
    );

    // Disposable: deleting the projection loses nothing; the durable head is
    // still readable from the transport.
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(mock.current_head().unwrap(), mock.head());
}

#[test]
fn hydrate_into_without_durable_state_writes_nothing() {
    let mock = MockStateTransport::new(UUID);
    let dir = tempfile::tempdir().unwrap();
    let report = mock.hydrate_into(dir.path(), None).unwrap();
    assert!(report.commit.is_none());
    assert!(report.files.is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn hydrate_into_rejects_invalid_selection() {
    let mock = bootstrap_mock();
    let dir = tempfile::tempdir().unwrap();
    let empty: Vec<String> = Vec::new();
    let error = mock.hydrate_into(dir.path(), Some(&empty)).unwrap_err();
    assert_eq!(error.code(), BrokerErrorCode::InvalidInput);

    let unsafe_paths = vec!["../escape".to_string()];
    let error = mock
        .hydrate_into(dir.path(), Some(&unsafe_paths))
        .unwrap_err();
    assert_eq!(error.code(), BrokerErrorCode::InvalidInput);
}

#[test]
fn commit_cas_rebases_after_a_competing_writer() {
    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();

    // A competing writer lands between our read and our CAS attempt.
    let competing = mock.inject_competing_commit(
        [("checkpoints/other.json", b"{}".to_vec())],
        "checkpoint: other writer",
        None,
    );

    let request = CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(base_head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&request, 1).expect("reconciled commit");
    assert!(!resolution.already_applied);
    assert_eq!(resolution.attempts, 2, "one conflict, one rebased retry");
    assert!(resolution.outcome.verified);
    assert_ne!(resolution.outcome.commit, competing);
    assert_eq!(
        mock.file_bytes("checkpoints/ours.json").unwrap(),
        br#"{"ours":true}"#
    );
    // The competing writer's file survives the retry (whole-file upsert).
    assert!(mock.file_bytes("checkpoints/other.json").is_some());
}

#[test]
fn commit_cas_detects_our_own_already_landed_write() {
    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();

    // Our first attempt lands but the response is lost, so the caller still
    // believes `base_head` is current.
    let landed = mock
        .commit(&CommitRequest::single(
            "checkpoints/ours.json",
            br#"{"ours":true}"#.to_vec(),
            Some(base_head.clone()),
            "checkpoint: ours",
            Some("op-ours".to_string()),
        ))
        .expect("direct commit");
    assert_eq!(mock.commit_count(), 1);

    // Force the conflict path by pretending the caller saw the old head.
    let stale_call = CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(base_head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&stale_call, 1).expect("reconcile");
    assert!(resolution.already_applied);
    assert_eq!(resolution.attempts, 1);
    assert_eq!(resolution.outcome.commit, landed.commit);
    assert!(resolution.outcome.verified, "our paths verify at the head");
    assert_eq!(mock.commit_count(), 1, "no second write was issued");
}

/// Reconciliation must not vouch for content it did not write: if the head
/// records our op id but carries a different payload (a reused op id or a later
/// overwrite), `already_applied` must report `verified: false`.
#[test]
fn commit_cas_does_not_vouch_for_content_it_did_not_write() {
    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();

    // Our operation lands first...
    mock.commit(&CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"v":1}"#.to_vec(),
        Some(base_head.clone()),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    ))
    .expect("direct commit");
    // ...then a later writer reuses the same op id and overwrites the payload.
    mock.inject_competing_commit(
        vec![("checkpoints/ours.json", br#"{"v":2}"#.to_vec())],
        "reuse",
        Some("op-ours"),
    );

    let stale_call = CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"v":1}"#.to_vec(),
        Some(base_head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&stale_call, 1).expect("reconcile");
    assert!(resolution.already_applied);
    assert!(
        !resolution.outcome.verified,
        "a content mismatch must not be reported as a verified write"
    );
    assert!(resolution.outcome.files.iter().all(|file| !file.verified));
    assert_eq!(mock.commit_count(), 1, "no second write was issued");
    assert_eq!(
        mock.file_bytes("checkpoints/ours.json").unwrap(),
        br#"{"v":2}"#,
        "the other writer's payload is untouched"
    );
}

#[test]
fn commit_cas_exhausts_retries_with_a_typed_stale_error() {
    let mock = bootstrap_mock();
    let stale = || {
        StateBrokerError::from_envelope(
            BrokerErrorCode::StaleState,
            "expected state head does not match the observed state head".to_string(),
            true,
            None,
            Some(409),
            None,
            Some("state.commit".to_string()),
        )
    };
    mock.fail_next_commit(stale());
    mock.fail_next_commit(stale());

    let request = CommitRequest::single(
        "checkpoints/ours.json",
        b"{}".to_vec(),
        mock.head(),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let error = mock.commit_cas(&request, 1).unwrap_err();
    assert!(error.is_stale_state());
    assert_eq!(mock.commit_count(), 0);
}

#[test]
fn commit_cas_requires_an_op_id() {
    let mock = bootstrap_mock();
    let request = CommitRequest::single(
        "checkpoints/ours.json",
        b"{}".to_vec(),
        mock.head(),
        "checkpoint: ours",
        None,
    );
    let error = mock.commit_cas(&request, 1).unwrap_err();
    assert_eq!(error.code(), BrokerErrorCode::InvalidInput);
    assert!(error.message().contains("op_id"));
}

#[test]
fn readback_mismatch_is_typed_for_reconciliation() {
    let mock = bootstrap_mock();
    let commit = "a".repeat(40);
    mock.fail_next_commit(StateBrokerError::from_envelope(
        BrokerErrorCode::UpstreamError,
        "state commit landed but read-back verification failed".to_string(),
        false,
        Some(serde_json::json!({
            "ref": format!("refs/heads/projects/{UUID}/state"),
            "commit": commit,
            "failed_paths": ["checkpoints/ours.json"],
        })),
        Some(502),
        None,
        Some("state.commit".to_string()),
    ));
    let request = CommitRequest::single(
        "checkpoints/ours.json",
        b"{}".to_vec(),
        mock.head(),
        "checkpoint: ours",
        None,
    );
    let error = mock.commit(&request).unwrap_err();
    assert_eq!(error.code(), BrokerErrorCode::UpstreamError);
    assert!(error.is_readback_mismatch(), "commit sha is recoverable");
    assert!(!error.retryable(), "never blind-retry a read-back mismatch");
}

/// The projection a broker transport writes must be consumable by the existing
/// state-hydration path unchanged: this is the "local SQLite/files are a
/// disposable projection" contract in executable form.
///
/// The v3 checkpoint path is used deliberately: `hydrate_from_state` is what
/// Crosslink's write path hydrates through, and it is not an audit-guarded
/// destructive v2 entry point.
#[test]
fn broker_projection_feeds_existing_state_hydration() {
    let uuid = Uuid::new_v4();
    let mut state = crate::checkpoint::CheckpointState {
        next_display_id: 2,
        next_comment_id: 1,
        ..crate::checkpoint::CheckpointState::default()
    };
    state.display_id_map.insert(uuid, 1);
    state.issues.insert(
        uuid,
        crate::checkpoint::CompactIssue {
            uuid,
            display_id: Some(1),
            title: "hydrated from broker state".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: "agent-broker".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: std::collections::BTreeSet::new(),
            blockers: std::collections::BTreeSet::new(),
            related: std::collections::BTreeSet::new(),
            milestone_uuid: None,
            comments: std::collections::BTreeMap::new(),
            time_entries: std::collections::BTreeMap::new(),
        },
    );

    // Serialize the state with the existing writer, then serve those exact bytes
    // as a broker blob (v3 layout).
    let scratch = tempfile::tempdir().unwrap();
    crate::checkpoint::write_checkpoint(scratch.path(), &state).unwrap();
    let checkpoint_bytes = std::fs::read(scratch.path().join("checkpoint/state.json")).unwrap();
    let mock = MockStateTransport::with_files(
        UUID,
        vec![
            ("checkpoint/state.json", checkpoint_bytes),
            ("meta/hub.json", br#"{"hub_version":3}"#.to_vec()),
        ],
    );

    // Durable state -> disposable projection -> existing reader -> SQLite.
    let dir = tempfile::tempdir().unwrap();
    let projection = dir.path().join("state-projection");
    mock.hydrate_into(&projection, None).expect("hydrate");

    let read_back = crate::checkpoint::read_checkpoint(&projection).expect("read checkpoint");
    let db = crate::db::Database::open(Path::new(":memory:")).unwrap();
    let stats = crate::hydration::hydrate_from_state(&read_back, &db).unwrap();
    assert_eq!(stats.issues, 1);
    let hydrated = db.get_issue(1).unwrap().expect("issue hydrated");
    assert_eq!(hydrated.title, "hydrated from broker state");
}

#[test]
fn broker_config_never_serializes_or_prints_the_token() {
    let config = StateBrokerConfig::new(
        "https://broker.example",
        UUID,
        "broker-token-abcdefghijklmnop",
        std::time::Duration::from_secs(15),
    )
    .unwrap();
    let debug = format!("{config:?}");
    assert!(!debug.contains("broker-token"));
    assert!(debug.contains("redacted"));
}
