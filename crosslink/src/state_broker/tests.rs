//! Cross-cutting unit tests for the broker transport: disposable projections
//! with identity/freshness markers, reconciled CAS verdicts (including refused
//! same-path rebases and ambiguous writes), and feeding the existing hydration
//! path from a broker projection.

use std::path::Path;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use super::client::CommitRequest;
use super::config::{default_projection_dir, StateBrokerConfig};
use super::error::{BrokerErrorCode, StateBrokerError};
use super::mock::MockStateTransport;
use super::projection::{read_projection_marker, PROJECTION_MARKER_FILE};
use super::transport::{CasResolution, OpReconciliation, ProjectStateTransport, ReconcileReason};

const UUID: &str = "1d440dcf-bcbf-4d1a-987c-d5334568a716";
const OTHER_UUID: &str = "2a551ed0-cdc0-4e2b-a98d-e6445679b827";

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

fn stale_error() -> StateBrokerError {
    StateBrokerError::from_envelope(
        BrokerErrorCode::StaleState,
        "expected state head does not match the observed state head".to_string(),
        true,
        Some(json!({})),
        Some(409),
        None,
        Some("state.commit".to_string()),
    )
}

fn ambiguous_error() -> StateBrokerError {
    StateBrokerError::reconcile_required(
        "write outcome is unknown (transport_ambiguous); reconcile by op_id",
        Some(json!({"reason": "transport_ambiguous", "op_id": "op-ours"})),
    )
}

fn expect_reconcile_required(resolution: &CasResolution) -> &ReconcileReason {
    match resolution {
        CasResolution::ReconcileRequired { reason, .. } => reason,
        other => panic!("expected ReconcileRequired, got {other:?}"),
    }
}

// ── Projection identity and freshness ────────────────────────────────

#[test]
fn hydrate_into_writes_a_disposable_projection_with_a_marker() {
    let mock = bootstrap_mock();
    let dir = tempfile::tempdir().unwrap();
    let root = default_projection_dir(dir.path());

    let report = mock.hydrate_into(&root, None).expect("hydrate");
    assert_eq!(report.commit.as_deref(), mock.head().as_deref());
    assert_eq!(report.files.len(), 2);
    assert!(report.bytes > 0);
    assert_eq!(
        std::fs::read(root.join("meta/counters.json")).unwrap(),
        br#"{"next_display_id":2,"next_comment_id":1}"#
    );

    // The marker binds the projection to the project and head it came from.
    let marker = read_projection_marker(&root).unwrap().expect("marker");
    assert!(marker.complete);
    assert_eq!(marker.project_uuid, UUID);
    assert_eq!(marker.backend_host.as_deref(), Some("mock.invalid"));
    assert_eq!(marker.state_ref, mock.state_ref());
    assert_eq!(marker.head_commit, mock.head().unwrap());
    assert_eq!(marker.files.len(), 2);
    assert_eq!(marker.bytes, report.bytes);
    assert_eq!(report.marker.as_ref(), Some(&marker));

    // The freshness gate accepts a projection of the current head.
    assert_eq!(mock.verify_projection(&root).unwrap(), marker);

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
    assert!(report.marker.is_none());
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
fn stale_projection_cannot_masquerade_as_current() {
    let mock = bootstrap_mock();
    let dir = tempfile::tempdir().unwrap();
    mock.hydrate_into(dir.path(), None).unwrap();

    // A competing writer moves the head: the projection is now stale.
    mock.inject_competing_commit(
        [("checkpoints/other.json", b"{}".to_vec())],
        "checkpoint: other writer",
        None,
    );
    let error = mock.verify_projection(dir.path()).unwrap_err();
    assert_eq!(error.code(), BrokerErrorCode::LocalIo);
    assert!(error.message().contains("stale"), "{}", error.message());

    // Re-hydrating restores freshness.
    mock.hydrate_into(dir.path(), None).unwrap();
    mock.verify_projection(dir.path()).unwrap();
}

#[test]
fn projection_of_another_project_is_refused() {
    let first = bootstrap_mock();
    let second = MockStateTransport::with_files(OTHER_UUID, [("a.json", b"{}".to_vec())]);
    let dir = tempfile::tempdir().unwrap();
    first.hydrate_into(dir.path(), None).unwrap();

    let error = second.hydrate_into(dir.path(), None).unwrap_err();
    assert!(error.is_identity_mismatch(), "{error:?}");

    // And the freshness gate refuses it too.
    let error = second.verify_projection(dir.path()).unwrap_err();
    assert!(error.is_identity_mismatch(), "{error:?}");
}

/// A projection must not silently move between backend instances that share a
/// project UUID: the marker records the backend host.
#[test]
fn projection_of_another_backend_is_refused() {
    let mock = bootstrap_mock();
    let dir = tempfile::tempdir().unwrap();
    mock.hydrate_into(dir.path(), None).unwrap();

    let mut marker = read_projection_marker(dir.path()).unwrap().expect("marker");
    marker.backend_host = Some("other.invalid".to_string());
    let body = serde_json::to_vec_pretty(&marker).unwrap();
    std::fs::write(dir.path().join(PROJECTION_MARKER_FILE), body).unwrap();

    let error = mock.verify_projection(dir.path()).unwrap_err();
    assert!(error.is_identity_mismatch(), "{error:?}");
    let error = mock.hydrate_into(dir.path(), None).unwrap_err();
    assert!(error.is_identity_mismatch(), "{error:?}");
}

#[test]
fn interrupted_hydration_leaves_an_incomplete_marker() {
    let mock = bootstrap_mock();
    let dir = tempfile::tempdir().unwrap();
    mock.fail_next_read_blob(StateBrokerError::transport("connection reset", true));

    let error = mock.hydrate_into(dir.path(), None).unwrap_err();
    assert_eq!(error.code(), BrokerErrorCode::Transport);

    let marker = read_projection_marker(dir.path()).unwrap().expect("marker");
    assert!(
        !marker.complete,
        "an interrupted hydration must not look complete"
    );
    let error = mock.verify_projection(dir.path()).unwrap_err();
    assert!(
        error.message().contains("incomplete"),
        "{}",
        error.message()
    );
}

#[test]
fn corrupt_marker_fails_closed() {
    let mock = bootstrap_mock();
    let dir = tempfile::tempdir().unwrap();
    mock.hydrate_into(dir.path(), None).unwrap();
    std::fs::write(dir.path().join(PROJECTION_MARKER_FILE), b"{ not json").unwrap();

    let error = mock.verify_projection(dir.path()).unwrap_err();
    assert_eq!(error.code(), BrokerErrorCode::LocalIo);
    assert!(
        error.message().contains("not valid JSON"),
        "{}",
        error.message()
    );
}

#[test]
fn rehydration_removes_files_outside_the_new_selection() {
    let mock = bootstrap_mock();
    let dir = tempfile::tempdir().unwrap();
    mock.hydrate_into(dir.path(), None).unwrap();
    assert!(dir.path().join("checkpoints/first.json").exists());

    // Upsert with a subset: the unselected file is removed so the directory
    // matches the marker manifest exactly.
    let selection = vec!["meta/counters.json".to_string()];
    mock.hydrate_into(dir.path(), Some(&selection)).unwrap();
    assert!(dir.path().join("meta/counters.json").exists());
    assert!(!dir.path().join("checkpoints/first.json").exists());
    let marker = mock.verify_projection(dir.path()).unwrap();
    assert_eq!(marker.files.len(), 1);
}

// ── CAS reconciliation ───────────────────────────────────────────────

#[test]
fn commit_cas_rebases_after_a_non_overlapping_competing_writer() {
    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();

    // A competing writer lands a commit touching a *different* path.
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
    let outcome = resolution.applied_outcome().expect("applied");
    assert!(resolution.is_verified());
    assert_eq!(resolution.attempts(), 2, "one conflict, one rebased retry");
    assert!(outcome.verified);
    assert_ne!(outcome.commit, competing);
    assert_eq!(
        mock.file_bytes("checkpoints/ours.json").unwrap(),
        br#"{"ours":true}"#
    );
    // The competing writer's file survives the retry.
    assert!(mock.file_bytes("checkpoints/other.json").is_some());
}

/// The same path changed by a competing writer must not be clobbered by the
/// automatic rebase: without a proof of non-overlap the call refuses.
#[test]
fn commit_cas_refuses_a_same_path_rebase() {
    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();
    let ours = "checkpoints/first.json";

    // A competing writer puts newer content on the very path we would upsert.
    mock.inject_competing_commit(
        [(ours, br#"{"theirs":true}"#.to_vec())],
        "checkpoint: theirs",
        None,
    );

    let request = CommitRequest::single(
        ours,
        br#"{"ours":true}"#.to_vec(),
        Some(base_head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&request, 1).expect("verdict");
    assert!(!resolution.is_verified());
    match expect_reconcile_required(&resolution) {
        ReconcileReason::OverlappingPaths { paths } => assert_eq!(paths, &[ours.to_string()]),
        other => panic!("expected OverlappingPaths, got {other:?}"),
    }
    assert_eq!(
        resolution.attempts(),
        1,
        "no write was attempted after the proof failed"
    );
    assert_eq!(
        mock.file_bytes(ours).unwrap(),
        br#"{"theirs":true}"#,
        "the competing writer's bytes must survive"
    );
    assert!(mock
        .commit_message(&mock.head().unwrap())
        .unwrap()
        .contains("checkpoint: theirs"));
}

/// Equivalent content is a valid proof: if the observed payload already equals
/// the intended payload, re-issuing cannot lose data.
#[test]
fn commit_cas_allows_a_same_path_rebase_when_content_is_equivalent() {
    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();
    let ours = "checkpoints/first.json";
    let intended = br#"{"ok":true}"#.to_vec();

    // The competing writer landed exactly the bytes we intend to write.
    mock.inject_competing_commit([(ours, intended.clone())], "checkpoint: same bytes", None);

    let request = CommitRequest::single(
        ours,
        intended.clone(),
        Some(base_head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&request, 1).expect("reconciled commit");
    assert!(resolution.is_verified());
    assert_eq!(resolution.attempts(), 2);
    assert_eq!(mock.file_bytes(ours).unwrap(), intended);
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
    match &resolution {
        CasResolution::AlreadyApplied { commit, files, .. } => {
            assert_eq!(commit, &landed.commit);
            assert!(files.iter().all(|file| file.verified));
        }
        other => panic!("expected AlreadyApplied, got {other:?}"),
    }
    assert!(resolution.is_verified());
    assert_eq!(resolution.attempts(), 1);
    assert_eq!(mock.commit_count(), 1, "no second write was issued");
}

/// Reconciliation must not vouch for content it did not write: if the head
/// records our op id but carries a different payload (a reused op id or a later
/// overwrite), the verdict is an explicit reconcile-required, never success.
#[test]
fn commit_cas_same_op_id_different_content_requires_reconciliation() {
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
    let resolution = mock.commit_cas(&stale_call, 1).expect("verdict");
    assert!(!resolution.is_verified());
    match expect_reconcile_required(&resolution) {
        ReconcileReason::OpIdReusedWithDifferentContent => {}
        other => panic!("expected OpIdReusedWithDifferentContent, got {other:?}"),
    }
    assert_eq!(mock.commit_count(), 1, "no second write was issued");
    assert_eq!(
        mock.file_bytes("checkpoints/ours.json").unwrap(),
        br#"{"v":2}"#,
        "the other writer's payload is untouched"
    );
}

#[test]
fn commit_cas_reconciles_an_ambiguous_write_that_landed() {
    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();

    let landed = mock
        .commit(&CommitRequest::single(
            "checkpoints/ours.json",
            br#"{"ours":true}"#.to_vec(),
            Some(base_head.clone()),
            "checkpoint: ours",
            Some("op-ours".to_string()),
        ))
        .expect("direct commit");

    // The next commit call fails ambiguously (e.g. the response was lost).
    mock.fail_next_commit(ambiguous_error());
    let request = CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(base_head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&request, 1).expect("verdict");
    match &resolution {
        CasResolution::AlreadyApplied { commit, .. } => assert_eq!(commit, &landed.commit),
        other => panic!("expected AlreadyApplied, got {other:?}"),
    }
    assert_eq!(
        mock.commit_count(),
        1,
        "reconciliation must not write again"
    );
}

#[test]
fn commit_cas_reports_an_ambiguous_write_that_did_not_land() {
    let mock = bootstrap_mock();
    let head = mock.head().unwrap();
    mock.fail_next_commit(ambiguous_error());

    let request = CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(head.clone()),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&request, 0).expect("verdict");
    assert!(!resolution.is_verified());
    match expect_reconcile_required(&resolution) {
        ReconcileReason::WriteNotLanded => {}
        other => panic!("expected WriteNotLanded, got {other:?}"),
    }
    assert_eq!(resolution.attempts(), 1);
    assert_eq!(mock.head(), Some(head), "no write was issued");
}

#[test]
fn commit_cas_reconcile_read_failure_is_ambiguous() {
    let mock = bootstrap_mock();
    let head = mock.head().unwrap();
    mock.fail_next_commit(ambiguous_error());
    mock.fail_next_read_state(StateBrokerError::transport("connection reset", true));

    let request = CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&request, 1).expect("verdict");
    match expect_reconcile_required(&resolution) {
        ReconcileReason::AmbiguousWrite { detail } => {
            assert!(detail.contains("could not read"), "{detail}");
        }
        other => panic!("expected AmbiguousWrite, got {other:?}"),
    }
}

/// A ref that disappeared between the conflict and the re-read must not be
/// rebased by bootstrapping a fresh history over the deleted one.
#[test]
fn commit_cas_refuses_to_bootstrap_over_a_deleted_ref() {
    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();
    mock.fail_next_commit(stale_error());
    mock.inject_ref_deletion();

    let request = CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(base_head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = mock.commit_cas(&request, 1).expect("verdict");
    match expect_reconcile_required(&resolution) {
        ReconcileReason::OverlapUnprovable { detail } => {
            assert!(detail.contains("disappeared"), "{detail}");
        }
        other => panic!("expected OverlapUnprovable, got {other:?}"),
    }
    assert!(mock.head().is_none(), "no bootstrap write was issued");
}

#[test]
fn commit_cas_reports_a_head_that_moves_during_reconciliation() {
    use std::cell::Cell;

    use super::client::{ProjectState, StateBlob, VerifiedEntry};

    /// Wraps the mock and simulates a head move between the reconciliation read
    /// and the verdict re-check (the verify-after-read TOCTOU window).
    struct MoveHeadOnRecheck {
        inner: MockStateTransport,
        reads: Cell<u8>,
        moved: Cell<bool>,
    }

    impl ProjectStateTransport for MoveHeadOnRecheck {
        fn read_state(&self) -> Result<ProjectState, StateBrokerError> {
            let reads = self.reads.get() + 1;
            self.reads.set(reads);
            if reads == 2 && !self.moved.get() {
                self.moved.set(true);
                self.inner.inject_competing_commit(
                    [("checkpoints/moved.json", b"{}".to_vec())],
                    "moved during reconcile",
                    None,
                );
            }
            self.inner.read_state()
        }

        fn read_blob(&self, path: &str, at: Option<&str>) -> Result<StateBlob, StateBrokerError> {
            self.inner.read_blob(path, at)
        }

        fn verify(
            &self,
            commit: &str,
            paths: &[String],
        ) -> Result<Vec<VerifiedEntry>, StateBrokerError> {
            self.inner.verify(commit, paths)
        }

        fn commit(
            &self,
            request: &CommitRequest,
        ) -> Result<super::client::CommitOutcome, StateBrokerError> {
            self.inner.commit(request)
        }
    }

    let mock = bootstrap_mock();
    let base_head = mock.head().unwrap();
    mock.commit(&CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(base_head.clone()),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    ))
    .expect("direct commit");

    let transport = MoveHeadOnRecheck {
        inner: mock,
        reads: Cell::new(0),
        moved: Cell::new(false),
    };
    let stale_call = CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(base_head),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    );
    let resolution = transport.commit_cas(&stale_call, 1).expect("verdict");
    assert!(!resolution.is_verified());
    match expect_reconcile_required(&resolution) {
        ReconcileReason::HeadMovedDuringReconcile => {}
        other => panic!("expected HeadMovedDuringReconcile, got {other:?}"),
    }
}

#[test]
fn commit_cas_exhausts_retries_with_a_typed_stale_error() {
    let mock = bootstrap_mock();
    mock.fail_next_commit(stale_error());
    mock.fail_next_commit(stale_error());

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
fn reconcile_is_available_to_raw_commit_callers() {
    let mock = bootstrap_mock();
    let head = mock.head().unwrap();
    mock.commit(&CommitRequest::single(
        "checkpoints/ours.json",
        br#"{"ours":true}"#.to_vec(),
        Some(head.clone()),
        "checkpoint: ours",
        Some("op-ours".to_string()),
    ))
    .expect("direct commit");

    let landed = mock
        .reconcile(&CommitRequest::single(
            "checkpoints/ours.json",
            br#"{"ours":true}"#.to_vec(),
            Some(head),
            "checkpoint: ours",
            Some("op-ours".to_string()),
        ))
        .expect("reconcile");
    match landed {
        OpReconciliation::Landed { files, .. } => {
            assert!(files.iter().all(|file| file.verified));
        }
        other => panic!("expected Landed, got {other:?}"),
    }
}

// ── Integration with the existing hydration path ─────────────────────

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
    // The projection is only trustworthy after the freshness gate passes.
    mock.verify_projection(&projection)
        .expect("fresh projection");

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
