//! CDP-1 tests (T01–T55). Shared fixtures plus the deterministic unit and
//! mock-broker scenarios from spec §10.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};

use super::attempt::AttemptStore;
use super::manifest::ManifestContext;
use super::publisher::{
    new_op_id, plan_publish, plan_publish_with_op_id, publish_checkpoint, Cdp1Config,
    PublishOptions, PublishOutcome, PublisherIdentity, RefusalReason,
};
use super::reader::{read_derived_checkpoint, Provenance, ReadExpectations, ReaderDefect};
use super::source::{CheckpointSource, JournalAnchor, PushedCheckpoint};
use super::{AccountingModel, MANIFEST_PATH, MAX_SLOTS};
use crate::checkpoint::CheckpointState;
use crate::events::OrderingKey;
use crate::state_broker::error::StateBrokerError;
use crate::state_broker::mock::MockStateTransport;
use crate::state_broker::transport::ProjectStateTransport;

const UUID: &str = "7f3c2a1e-9b4d-4c6a-8e2f-1d5b7a9c0e3f";
const PUBLISHER: &str = "test-publisher";

// ── Fixtures ─────────────────────────────────────────────────────────

/// A synthetic pushed checkpoint with a controllable watermark and state size.
pub(crate) struct MockCheckpointSource {
    pub checkpoint: PushedCheckpoint,
    pub fetched: Mutex<bool>,
    pub fetch_fails: bool,
}

impl MockCheckpointSource {
    pub(crate) fn new(agent_seq: u64) -> Self {
        Self::with_state(
            agent_seq,
            CheckpointState {
                next_display_id: 42,
                watermark: Some(OrderingKey {
                    timestamp: DateTime::<Utc>::UNIX_EPOCH
                        + chrono::Duration::seconds(agent_seq as i64),
                    agent_id: "driver".to_string(),
                    agent_seq,
                }),
                ..Default::default()
            },
        )
    }

    pub(crate) fn with_state(agent_seq: u64, state: CheckpointState) -> Self {
        let state_bytes = serde_json::to_vec_pretty(&state).expect("state json");
        let state_sha256 = crate::state_broker::digest::sha256_hex(&state_bytes);
        let watermark = state.watermark.clone().expect("watermark");
        let _ = agent_seq;
        let commit = format!("{:040x}", 0xa1b2_c3d4_e5f6_0718_u64);
        let state_blob_sha = format!("{:040x}", 0xb2c3_d4e5_f607_1829_u64);
        Self {
            checkpoint: PushedCheckpoint {
                commit,
                state_blob_sha,
                state_bytes,
                state,
                watermark,
                state_sha256,
            },
            fetched: Mutex::new(false),
            fetch_fails: false,
        }
    }

    pub(crate) fn failing_fetch() -> Self {
        let mut source = Self::new(1);
        source.fetch_fails = true;
        source
    }
}

impl CheckpointSource for MockCheckpointSource {
    fn resolve_pushed_checkpoint(&self) -> Result<PushedCheckpoint, StateBrokerError> {
        if self.fetch_fails {
            return Err(StateBrokerError::local_io(
                "mock fetch failed: cannot prove the checkpoint is pushed",
            ));
        }
        *self.fetched.lock().unwrap() = true;
        Ok(self.checkpoint.clone())
    }
}

impl JournalAnchor for MockCheckpointSource {
    fn verify_anchor(
        &self,
        source: &super::manifest::SourceCheckpoint,
        state_bytes: &[u8],
    ) -> Result<(), StateBrokerError> {
        if source.commit != self.checkpoint.commit
            || source.state_blob_sha != self.checkpoint.state_blob_sha
        {
            return Err(StateBrokerError::protocol(
                "mock journal anchor: source provenance mismatch",
            ));
        }
        if state_bytes != self.checkpoint.state_bytes {
            return Err(StateBrokerError::protocol(
                "mock journal anchor: state bytes mismatch",
            ));
        }
        Ok(())
    }
}

/// An anchor that always fails (for provenance-mismatch tests).
pub(crate) struct FailingAnchor;

impl JournalAnchor for FailingAnchor {
    fn verify_anchor(
        &self,
        _source: &super::manifest::SourceCheckpoint,
        _state_bytes: &[u8],
    ) -> Result<(), StateBrokerError> {
        Err(StateBrokerError::protocol("forced anchor mismatch"))
    }
}

pub(crate) fn cfg() -> Cdp1Config {
    Cdp1Config {
        project_uuid: UUID.to_string(),
        state_ref: format!("refs/heads/projects/{UUID}/state"),
        publisher_id: PUBLISHER.to_string(),
        accounting: AccountingModel::Decoded,
    }
}

pub(crate) fn writer() -> PublisherIdentity {
    PublisherIdentity::writer(UUID)
}

fn seeded_transport() -> MockStateTransport {
    MockStateTransport::new(UUID)
}

/// Publish a checkpoint through the mock broker and return the outcome.
fn publish(
    transport: &MockStateTransport,
    source: &MockCheckpointSource,
    opts: PublishOptions,
) -> PublishOutcome {
    publish_checkpoint(transport, source, &cfg(), &writer(), &opts, None).unwrap()
}

/// Publish and assert it landed.
fn publish_landed(transport: &MockStateTransport, source: &MockCheckpointSource) -> String {
    match publish(transport, source, PublishOptions::default()) {
        PublishOutcome::Landed { commit, .. } => commit,
        other => panic!("expected Landed, got {other:?}"),
    }
}

/// High-entropy text so gzip cannot shrink it (multi-chunk / oversized tests).
fn noise(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(ALPHABET[(state % 64) as usize] as char);
    }
    out
}

/// A checkpoint whose JSON is dominated by high-entropy text.
fn noisy_checkpoint(agent_seq: u64, noise_len: usize) -> MockCheckpointSource {
    let mut state = CheckpointState::default();
    let uuid = uuid::Uuid::new_v4();
    state.issues.insert(
        uuid,
        crate::checkpoint::CompactIssue {
            uuid,
            display_id: Some(1),
            title: noise(noise_len),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: "driver".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: BTreeSet::default(),
            blockers: BTreeSet::default(),
            related: BTreeSet::default(),
            milestone_uuid: None,
            comments: BTreeMap::default(),
            time_entries: BTreeMap::default(),
        },
    );
    state.watermark = Some(OrderingKey {
        timestamp: Utc::now(),
        agent_id: "driver".to_string(),
        agent_seq,
    });
    MockCheckpointSource::with_state(agent_seq, state)
}

fn defect_of(error: &StateBrokerError) -> String {
    error
        .details()
        .and_then(|d| d.get("defect"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ── T01–T09, T12–T16, T20–T23, T25, T27–T34, T36–T55 (mock/unit) ─────

#[test]
fn t01_t02_one_and_multiple_chunks_publish_and_read_back() {
    // T01: a small checkpoint is one chunk.
    let small = MockCheckpointSource::new(1);
    let transport = seeded_transport();
    let commit = publish_landed(&transport, &small);
    let read = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&small),
    )
    .unwrap();
    assert_eq!(read.commit, commit);
    assert_eq!(read.manifest.chunk_count, 1);
    assert_eq!(read.provenance, Provenance::Advisory);
    assert_eq!(read.state.watermark, small.checkpoint.state.watermark);

    // T02: a large, incompressible state needs multiple chunks.
    let big = noisy_checkpoint(2, 900_000);
    let transport = seeded_transport();
    let commit = publish_landed(&transport, &big);
    let read =
        read_derived_checkpoint(&transport, &cfg(), &ReadExpectations::default(), Some(&big))
            .unwrap();
    assert!(read.manifest.chunk_count > 1, "expected multiple chunks");
    assert_eq!(read.state_bytes, big.checkpoint.state_bytes);
    assert_eq!(read.commit, commit);
}

#[test]
fn t03_t04_shrinking_publish_leaves_old_slots_inert() {
    let big = noisy_checkpoint(10, 900_000);
    let transport = seeded_transport();
    publish_landed(&transport, &big);
    let big_slots = transport
        .file_paths()
        .into_iter()
        .filter(|p| p.starts_with("checkpoint/chunks/"))
        .count();
    assert!(big_slots > 1);

    // Shrink to one chunk with a strictly higher watermark (timestamp first).
    let small_state = CheckpointState {
        next_display_id: 11,
        watermark: Some(OrderingKey {
            timestamp: Utc::now() + chrono::Duration::hours(1),
            agent_id: "driver".to_string(),
            agent_seq: 11,
        }),
        ..CheckpointState::default()
    };
    let small = MockCheckpointSource::with_state(11, small_state);
    match publish(&transport, &small, PublishOptions::default()) {
        PublishOutcome::Landed { .. } => {}
        other => panic!("expected Landed, got {other:?}"),
    }
    let manifest = transport
        .file_bytes(MANIFEST_PATH)
        .expect("manifest present");
    let parsed = super::manifest::CheckpointManifestV1::from_slice(&manifest).unwrap();
    assert_eq!(parsed.chunk_count, 1);
    // Old slots remain in the tree (no delete) but are not listed.
    let paths = transport.file_paths();
    assert!(paths.contains(&"checkpoint/chunks/0001".to_string()));
    assert_eq!(parsed.chunks.len(), 1);
    // The reader reconstructs from the active set only.
    let read = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&small),
    )
    .unwrap();
    assert_eq!(read.state_bytes, small.checkpoint.state_bytes);
    assert_eq!(read.manifest.chunk_count, 1);
}

#[test]
fn t05_corrupt_chunk_is_refused() {
    let source = MockCheckpointSource::new(1);
    let transport = seeded_transport();
    publish_landed(&transport, &source);
    transport.corrupt_file("checkpoint/chunks/0000", b"corrupted");
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::CorruptChunk.label());
}

#[test]
fn t06_corrupt_manifest_is_refused() {
    let source = MockCheckpointSource::new(1);
    let transport = seeded_transport();
    publish_landed(&transport, &source);
    transport.corrupt_file(MANIFEST_PATH, b"{not json");
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::MalformedManifest.label());
}

#[test]
fn t07_wrong_chunk_ordering_is_refused() {
    use super::manifest::CheckpointManifestV1;
    let source = MockCheckpointSource::new(1);
    let transport = seeded_transport();
    publish_landed(&transport, &source);
    let mut manifest =
        CheckpointManifestV1::from_slice(&transport.file_bytes(MANIFEST_PATH).unwrap()).unwrap();
    // Declare two chunks while listing one (ordering/count defect).
    manifest.chunk_count = 2;
    let bytes = serde_json::to_vec(&manifest).unwrap();
    transport.corrupt_file(MANIFEST_PATH, &bytes);
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::MalformedManifest.label());
}

#[test]
fn t08_wrong_digest_layers_are_distinguished() {
    let source = MockCheckpointSource::new(1);
    let transport = seeded_transport();
    publish_landed(&transport, &source);

    // Manifest-level payload digest corruption is caught by the payload check.
    let mut manifest = super::manifest::CheckpointManifestV1::from_slice(
        &transport.file_bytes(MANIFEST_PATH).unwrap(),
    )
    .unwrap();
    manifest.payload_sha256 = "0".repeat(64);
    transport.corrupt_file(MANIFEST_PATH, serde_json::to_vec(&manifest).unwrap());
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::CorruptState.label());
}

#[test]
fn t09_missing_active_chunk_is_truncation() {
    let source = MockCheckpointSource::new(1);
    let transport = seeded_transport();
    publish_landed(&transport, &source);
    transport.remove_file("checkpoint/chunks/0000");
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::Truncated.label());
}

#[test]
fn t10_stale_broker_head_reclassifies_and_supersedes() {
    let transport = seeded_transport();
    let first = MockCheckpointSource::new(1);
    publish_landed(&transport, &first);

    // A competing publish advances the head while our candidate is older.
    let other = MockCheckpointSource::new(5);
    publish_landed(&transport, &other);

    // Our older candidate is refused, not clobbered.
    let stale = MockCheckpointSource::new(2);
    match publish(&transport, &stale, PublishOptions::default()) {
        PublishOutcome::Refused {
            reason: RefusalReason::CandidateStale,
            ..
        } => {}
        other => panic!("expected CandidateStale, got {other:?}"),
    }
}

#[test]
fn t11_newer_watermark_candidate_supersedes_lower_head() {
    let transport = seeded_transport();
    let older = MockCheckpointSource::new(1);
    publish_landed(&transport, &older);
    let newer = MockCheckpointSource::new(2);
    match publish(&transport, &newer, PublishOptions::default()) {
        PublishOutcome::Landed { .. } => {}
        other => panic!("expected Landed, got {other:?}"),
    }
}

#[test]
fn t12_same_watermark_identical_replay_is_already_current() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(3);
    publish_landed(&transport, &source);
    let commits = transport.commit_count();
    // A second plan of the same source (new op id) must be a no-op.
    match publish(&transport, &source, PublishOptions::default()) {
        PublishOutcome::AlreadyCurrent { .. } => {}
        other => panic!("expected AlreadyCurrent, got {other:?}"),
    }
    assert_eq!(transport.commit_count(), commits, "no new commit");
}

#[test]
#[allow(clippy::redundant_clone)] // clone from a Drop-containing fixture
fn t13_same_watermark_different_state_is_diverged() {
    let transport = seeded_transport();
    let first = MockCheckpointSource::new(3);
    publish_landed(&transport, &first);
    // Same watermark (agent_seq), different state content.
    let watermark = first.checkpoint.watermark.clone();
    let state = CheckpointState {
        next_display_id: 999,
        watermark: Some(watermark),
        ..CheckpointState::default()
    };
    let different = MockCheckpointSource::with_state(3, state);
    match publish(&transport, &different, PublishOptions::default()) {
        PublishOutcome::Diverged { .. } => {}
        other => panic!("expected Diverged, got {other:?}"),
    }
}

#[test]
#[allow(clippy::redundant_clone)] // clone from a Drop-containing fixture
fn t14_same_op_id_different_content_is_diverged() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(4);
    let original =
        plan_publish_with_op_id(&source, &cfg(), "ckpt-aaaaaaaaaaaa-0123456789abcdef").unwrap();

    // A different plan reuses the op id and lands at an equal watermark.
    let watermark = source.checkpoint.watermark.clone();
    let state = CheckpointState {
        next_display_id: 1234,
        watermark: Some(watermark),
        ..CheckpointState::default()
    };
    let different = MockCheckpointSource::with_state(4, state);
    let different_plan =
        plan_publish_with_op_id(&different, &cfg(), original.op_id.clone()).unwrap();
    let request = crate::state_broker::client::CommitRequest {
        expected_head: None,
        message: "crosslink checkpoint publish reuse".to_string(),
        op_id: Some(different_plan.op_id.clone()),
        files: std::iter::once(crate::state_broker::client::CommitFile {
            path: MANIFEST_PATH.to_string(),
            content: different_plan.manifest_bytes.clone(),
        })
        .chain(
            different_plan
                .chunks
                .iter()
                .enumerate()
                .map(|(slot, chunk)| crate::state_broker::client::CommitFile {
                    path: super::chunk_path(slot as u32),
                    content: chunk.clone(),
                }),
        )
        .collect(),
    };
    assert!(transport.commit(&request).unwrap().verified);

    // A local attempt record still describes the ORIGINAL content under the
    // same op id: recovery must report divergence, never a verified landing.
    let store_dir = tempfile::tempdir().unwrap();
    let store = AttemptStore::new(store_dir.path());
    let mut record = super::attempt::AttemptRecord::prepared(
        UUID,
        PUBLISHER,
        original.op_id.clone(),
        super::attempt::AttemptSource {
            commit: original.source.commit.clone(),
            state_path: "state.json".to_string(),
            state_blob_sha: original.source.state_blob_sha.clone(),
            state_sha256: original.source.state_sha256.clone(),
            watermark: original.source.watermark.clone(),
        },
        original.payload.len() as u64,
        original.manifest.payload_sha256.clone(),
        original.manifest_sha256.clone(),
        original.chunks.len() as u32,
        None,
    );
    record.phase = super::attempt::AttemptPhase::InFlight.label().to_string();
    store.save(&record).unwrap();
    let outcome = publish_checkpoint(
        &transport,
        &different,
        &cfg(),
        &writer(),
        &PublishOptions::default(),
        Some(&store),
    )
    .unwrap();
    assert!(
        matches!(outcome, PublishOutcome::Diverged { .. }),
        "expected Diverged, got {outcome:?}"
    );
}

#[test]
fn t15_lost_response_after_landed_commit_reconciles() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(6);
    // The commit lands, but the client sees an ambiguous transport error.
    transport.fail_next_commit_after_landing(StateBrokerError::transport("connection reset", true));
    let outcome = publish(&transport, &source, PublishOptions::default());
    assert!(
        matches!(outcome, PublishOutcome::Landed { .. }),
        "expected Landed via reconciliation, got {outcome:?}"
    );
    assert_eq!(transport.commit_count(), 1, "exactly one commit");
}

#[test]
fn t16_lost_response_not_landed_retries_once() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(7);
    transport.fail_next_commit(StateBrokerError::transport("timeout", true));
    let outcome = publish(&transport, &source, PublishOptions::default());
    assert!(
        matches!(outcome, PublishOutcome::Landed { .. }),
        "expected Landed after one retry, got {outcome:?}"
    );
    assert_eq!(transport.commit_attempts(), 2, "one retry");
    assert_eq!(transport.commit_count(), 1, "one landed commit");
}

#[test]
fn t19_unknown_read_during_reconcile_blocks() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(8);
    let plan = plan_publish(&source, &cfg()).unwrap();
    let intent = super::publisher::ReconcileIntent::from_plan(&plan, None);
    // Reconciliation itself cannot read state: the verdict must be unknown.
    transport.fail_next_read_state(StateBrokerError::transport("timeout", true));
    let verdict = super::publisher::reconcile(&transport, &cfg(), &intent, None);
    match verdict {
        super::publisher::ReconcileVerdict::Unknown { detail } => {
            assert!(
                detail.contains("cannot read durable state"),
                "detail: {detail}"
            );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn t20_oversized_checkpoint_fails_closed_with_zero_commits() {
    // A state whose gzip payload exceeds the Wire fallback cap fails before
    // any broker call, in both accounting models.
    let huge = noisy_checkpoint(9, 1_600_000);
    let transport = seeded_transport();
    let wire = Cdp1Config {
        accounting: AccountingModel::Wire,
        ..cfg()
    };
    let result = publish_checkpoint(
        &transport,
        &huge,
        &wire,
        &writer(),
        &PublishOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "oversized must fail before any broker call"
    );
    assert_eq!(transport.commit_count(), 0);
    assert_eq!(transport.read_calls(), 0);
}

#[test]
fn t21_exact_capacity_boundaries() {
    // Payload at the cap is accepted; one byte over is refused.
    let model = AccountingModel::Decoded;
    let cap = model.max_payload_bytes() as usize;
    let at_cap = vec![0u8; cap];
    let chunks = super::payload::split_payload(&at_cap, model).unwrap();
    assert!(super::capacity::check_capacity(&[1u8], &at_cap, &chunks, &[b' '; 16], model).is_ok());
    let over = vec![0u8; cap + 1];
    let over_chunks: Vec<Vec<u8>> = over
        .chunks(model.slot_bytes() as usize)
        .map(<[u8]>::to_vec)
        .collect();
    assert!(
        super::capacity::check_capacity(&[1u8], &over, &over_chunks, &[b' '; 16], model).is_err()
    );
    assert_eq!(MAX_SLOTS, 4);
}

#[test]
fn t22_project_identity_mismatch_is_refused() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    // A reader configured for another project refuses the manifest.
    let other = Cdp1Config {
        project_uuid: "0f0e0d0c-0b0a-4908-8706-050403020100".to_string(),
        state_ref: "refs/heads/projects/0f0e0d0c-0b0a-4908-8706-050403020100/state".to_string(),
        ..cfg()
    };
    let error = read_derived_checkpoint(
        &transport,
        &other,
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::WrongCheckpoint.label());
}

#[test]
fn t23_source_checkpoint_mismatch_is_refused() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    let expect = ReadExpectations {
        source_commit: Some(format!("{:040x}", 0xdead_beef_u64)),
        ..Default::default()
    };
    let error = read_derived_checkpoint(&transport, &cfg(), &expect, Some(&source)).unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::WrongCheckpoint.label());
}

#[test]
fn t23b_unpushed_checkpoint_source_fails_closed() {
    // A source whose fetch cannot prove durability must fail before any
    // broker call.
    let transport = seeded_transport();
    let source = MockCheckpointSource::failing_fetch();
    let result = publish_checkpoint(
        &transport,
        &source,
        &cfg(),
        &writer(),
        &PublishOptions::default(),
        None,
    );
    assert!(result.is_err(), "an unprovable source must fail closed");
    assert_eq!(transport.read_calls(), 0);
    assert_eq!(transport.commit_count(), 0);
}

#[test]
fn t24_stale_projection_and_min_watermark() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    let min = OrderingKey {
        timestamp: Utc::now(),
        agent_id: "driver".to_string(),
        agent_seq: 99,
    };
    let expect = ReadExpectations {
        min_watermark: Some(min),
        ..Default::default()
    };
    let error = read_derived_checkpoint(&transport, &cfg(), &expect, Some(&source)).unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::Stale.label());
}

#[test]
fn t25_unused_old_slots_do_not_affect_reads() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    // Add an inert extra slot that no manifest lists.
    transport.upsert_file("checkpoint/chunks/0003", b"inert");
    let read = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap();
    assert_eq!(read.state_bytes, source.checkpoint.state_bytes);
}

#[test]
fn t27_reader_pins_one_commit_across_a_concurrent_publish() {
    let transport = seeded_transport();
    let first = MockCheckpointSource::new(1);
    publish_landed(&transport, &first);
    let pinned = transport.head().unwrap();
    // A competing publish moves the head after the pin.
    let second = MockCheckpointSource::new(2);
    publish_landed(&transport, &second);
    // A read pinned to the old commit still reconstructs the old state.
    let blob = transport.read_blob(MANIFEST_PATH, Some(&pinned)).unwrap();
    assert_eq!(blob.commit, pinned);
    let manifest =
        super::manifest::CheckpointManifestV1::from_slice(&blob.bytes().unwrap()).unwrap();
    assert_eq!(manifest.source.watermark.agent_seq, 1);
}

#[test]
fn t30_watermark_comparison_is_by_value_not_string() {
    let a = OrderingKey {
        timestamp: DateTime::parse_from_rfc3339("2026-09-21T04:40:12.611359099Z")
            .unwrap()
            .with_timezone(&Utc),
        agent_id: "driver".to_string(),
        agent_seq: 76,
    };
    let b = OrderingKey {
        timestamp: DateTime::parse_from_rfc3339("2026-09-21T04:40:12.611359099+00:00")
            .unwrap()
            .with_timezone(&Utc),
        agent_id: "driver".to_string(),
        agent_seq: 76,
    };
    assert_eq!(a, b);
    let json_a = serde_json::to_string(&a).unwrap();
    let parsed: OrderingKey = serde_json::from_str(&json_a).unwrap();
    assert_eq!(parsed, a);
}

#[test]
fn t31_publisher_never_deletes_and_reader_never_globs() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    assert!(transport.delete_calls() == 0, "no delete support is used");
    // The reader's request paths are exactly the manifest's active set.
    let read = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap();
    assert_eq!(read.manifest.chunks.len(), 1);
    let listed: Vec<&str> = read
        .manifest
        .chunks
        .iter()
        .map(|c| c.path.as_str())
        .collect();
    assert!(listed.contains(&"checkpoint/chunks/0000"));
    assert!(!listed.contains(&"checkpoint/chunks/0003"));
}

#[test]
fn t32_attempt_record_recovery_reconciles_landed_write() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    let store_dir = tempfile::tempdir().unwrap();
    let store = AttemptStore::new(store_dir.path());
    // Land a publish directly (simulating a crash after the commit).
    let plan = plan_publish(&source, &cfg()).unwrap();
    let request = crate::state_broker::client::CommitRequest {
        expected_head: None,
        message: "crosslink checkpoint publish crash-test slots=1 wm=driver/1".to_string(),
        op_id: Some(plan.op_id.clone()),
        files: std::iter::once(crate::state_broker::client::CommitFile {
            path: MANIFEST_PATH.to_string(),
            content: plan.manifest_bytes.clone(),
        })
        .chain(plan.chunks.iter().enumerate().map(|(slot, chunk)| {
            crate::state_broker::client::CommitFile {
                path: super::chunk_path(slot as u32),
                content: chunk.clone(),
            }
        }))
        .collect(),
    };
    transport.commit(&request).unwrap();
    // A prepared record from before the crash.
    let record = super::attempt::AttemptRecord::prepared(
        UUID,
        PUBLISHER,
        plan.op_id.clone(),
        super::attempt::AttemptSource {
            commit: plan.source.commit.clone(),
            state_path: "state.json".to_string(),
            state_blob_sha: plan.source.state_blob_sha.clone(),
            state_sha256: plan.source.state_sha256.clone(),
            watermark: plan.source.watermark.clone(),
        },
        plan.payload.len() as u64,
        plan.manifest.payload_sha256.clone(),
        plan.manifest_sha256.clone(),
        plan.chunks.len() as u32,
        None,
    );
    store.save(&record).unwrap();
    let outcome = publish_checkpoint(
        &transport,
        &source,
        &cfg(),
        &writer(),
        &PublishOptions::default(),
        Some(&store),
    )
    .unwrap();
    assert!(matches!(outcome, PublishOutcome::Landed { .. }));
    let loaded = store.load().unwrap().unwrap();
    assert!(!loaded.is_unresolved());
    assert_eq!(loaded.resolution.unwrap().outcome, "landed");
}

#[test]
fn t34_foreign_checkpoint_paths_without_manifest_refuse() {
    // A head commit containing checkpoint/ paths but no manifest is a foreign
    // namespace: refuse, never guess.
    let transport =
        MockStateTransport::with_files(UUID, [("checkpoint/chunks/0000", b"foreign".as_slice())]);
    let source = MockCheckpointSource::new(1);
    match publish(&transport, &source, PublishOptions::default()) {
        PublishOutcome::Refused {
            reason: RefusalReason::OverlapUnprovable,
            ..
        } => {}
        other => panic!("expected OverlapUnprovable, got {other:?}"),
    }
    assert_eq!(transport.commit_count(), 0);
}

#[test]
fn t36_journal_anchored_and_advisory_profiles() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);

    let advisory =
        read_derived_checkpoint(&transport, &cfg(), &ReadExpectations::default(), None).unwrap();
    assert_eq!(advisory.provenance, Provenance::Advisory);

    let anchored = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations {
            journal_anchored: true,
            ..Default::default()
        },
        Some(&source),
    )
    .unwrap();
    assert_eq!(anchored.provenance, Provenance::JournalAnchored);

    // Anchoring required without an anchor is refused.
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations {
            journal_anchored: true,
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::ProvenanceRequired.label());

    // A failing anchor is a provenance mismatch.
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations {
            journal_anchored: true,
            ..Default::default()
        },
        Some(&FailingAnchor),
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::ProvenanceMismatch.label());
}

#[test]
fn t38_dry_run_makes_zero_broker_calls() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    let outcome = publish(
        &transport,
        &source,
        PublishOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    match outcome {
        PublishOutcome::DryRun { plan } => {
            assert!(plan.payload_bytes > 0);
            assert_eq!(plan.files, 1 + plan.chunk_count as usize);
        }
        other => panic!("expected DryRun, got {other:?}"),
    }
    assert_eq!(transport.read_calls(), 0);
    assert_eq!(transport.commit_count(), 0);
}

#[test]
fn t40_content_identical_recommit_is_already_current() {
    // Same state bytes, different source commit sha.
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);

    let mut recommitted = MockCheckpointSource::new(1);
    recommitted.checkpoint.commit = format!("{:040x}", 0xfeed_face_u64);
    match publish(&transport, &recommitted, PublishOptions::default()) {
        PublishOutcome::AlreadyCurrent { .. } => {}
        other => panic!("expected AlreadyCurrent, got {other:?}"),
    }
    assert_eq!(transport.commit_count(), 1, "no second commit");
}

#[test]
fn t42_reconcile_verifies_active_paths_only() {
    // Land a one-chunk publish, then supersede it with a higher watermark.
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    let plan =
        plan_publish_with_op_id(&source, &cfg(), "ckpt-aaaaaaaaaaaa-0000000000000001").unwrap();
    let request = crate::state_broker::client::CommitRequest {
        expected_head: None,
        message: "crosslink checkpoint publish slots=1".to_string(),
        op_id: Some(plan.op_id.clone()),
        files: std::iter::once(crate::state_broker::client::CommitFile {
            path: MANIFEST_PATH.to_string(),
            content: plan.manifest_bytes.clone(),
        })
        .chain(plan.chunks.iter().enumerate().map(|(slot, chunk)| {
            crate::state_broker::client::CommitFile {
                path: super::chunk_path(slot as u32),
                content: chunk.clone(),
            }
        }))
        .collect(),
    };
    let landed = transport.commit(&request).unwrap();
    let newer = MockCheckpointSource::new(2);
    publish_landed(&transport, &newer);

    // Reconcile with the carried (superseded) commit: only the one active path
    // is verified, so the verdict is LandedSuperseded, never NotLanded.
    let intent = super::publisher::ReconcileIntent::from_plan(&plan, None);
    let verdict = super::publisher::reconcile(&transport, &cfg(), &intent, Some(&landed.commit));
    match verdict {
        super::publisher::ReconcileVerdict::LandedSuperseded { commit, head } => {
            assert_eq!(commit, landed.commit);
            assert_eq!(head, transport.head().unwrap());
        }
        other => panic!("expected LandedSuperseded, got {other:?}"),
    }
    // The one-chunk publish wrote exactly one chunk path.
    assert_eq!(intent.paths.len(), 2, "manifest + one active chunk");
}

#[test]
fn t43_vanished_ref_never_bootstraps() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    let plan = plan_publish(&source, &cfg()).unwrap();
    let intent = super::publisher::ReconcileIntent::from_plan(&plan, Some("b".repeat(40)));
    // The ref vanishes while an expected head existed: reconcile must report
    // unknown rather than bootstrap a fresh single-commit history.
    transport.inject_ref_deletion();
    let verdict = super::publisher::reconcile(&transport, &cfg(), &intent, None);
    match verdict {
        super::publisher::ReconcileVerdict::Unknown { detail } => {
            assert!(detail.contains("vanished"), "detail: {detail}");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
    assert_eq!(transport.commit_count(), 0, "no bootstrap write");
}

#[test]
#[allow(clippy::redundant_clone)] // clone from a Drop-containing fixture
fn t46_concurrent_bootstrap_race_is_handled() {
    let transport = seeded_transport();
    let first = MockCheckpointSource::new(1);
    publish_landed(&transport, &first);
    // A second publisher at an equal watermark with identical content is a
    // no-op; with different content it is Diverged (never a clobber).
    let same = MockCheckpointSource::new(1);
    assert!(matches!(
        publish(&transport, &same, PublishOptions::default()),
        PublishOutcome::AlreadyCurrent { .. }
    ));
    let watermark = first.checkpoint.watermark.clone();
    let state = CheckpointState {
        next_display_id: 77,
        watermark: Some(watermark),
        ..CheckpointState::default()
    };
    let different = MockCheckpointSource::with_state(1, state);
    assert!(matches!(
        publish(&transport, &different, PublishOptions::default()),
        PublishOutcome::Diverged { .. }
    ));
}

#[test]
fn t47_wire_accounting_boundaries_are_exact() {
    let model = AccountingModel::Wire;
    assert_eq!(model.slot_bytes(), 196_608);
    assert_eq!(model.max_payload_bytes(), 782_336);
    let cap = model.max_payload_bytes() as usize;
    let at_cap = vec![1u8; cap];
    let chunks = super::payload::split_payload(&at_cap, model).unwrap();
    assert_eq!(chunks.len(), 4);
    assert!(super::capacity::check_capacity(&[1u8], &at_cap, &chunks, &[b' '; 16], model).is_ok());
}

#[test]
fn t48_decompression_bomb_is_refused() {
    let payload = super::payload::gzip_encode(&vec![0u8; 200_000]).unwrap();
    assert!(super::payload::gzip_decode_bounded(&payload, 1_000).is_err());
    // The hard reader cap also applies.
    assert!(super::payload::gzip_decode_bounded(&payload, u64::MAX).is_ok());
}

#[test]
fn t49_projection_path_is_not_publishable_as_source() {
    // The source trait only resolves a pushed git checkpoint; a projection
    // path cannot be supplied. This asserts the fixture's contract: the
    // resolved source always names a git commit and blob, never a projection.
    let source = MockCheckpointSource::new(1);
    let checkpoint = source.resolve_pushed_checkpoint().unwrap();
    assert_eq!(checkpoint.commit.len(), 40);
    assert_eq!(checkpoint.state_blob_sha.len(), 40);
    assert!(!checkpoint.commit.contains("state-projection"));
}

#[test]
fn t50_forged_self_consistent_manifest_is_advisory_only() {
    // A self-consistent manifest can be read broker-only, but cannot satisfy
    // the journal-anchored requirement without the git blob.
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    let advisory =
        read_derived_checkpoint(&transport, &cfg(), &ReadExpectations::default(), None).unwrap();
    assert_eq!(advisory.provenance, Provenance::Advisory);
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations {
            journal_anchored: true,
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::ProvenanceRequired.label());
}

#[test]
fn t51_watermark_ordering_across_agents() {
    // Timestamp dominates, then agent_id, then seq — the same Ord as reduce.
    let base = OrderingKey {
        timestamp: DateTime::<Utc>::UNIX_EPOCH,
        agent_id: "a".to_string(),
        agent_seq: 1,
    };
    let later = OrderingKey {
        timestamp: DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(1),
        agent_id: "a".to_string(),
        agent_seq: 1,
    };
    let other_agent = OrderingKey {
        timestamp: DateTime::<Utc>::UNIX_EPOCH,
        agent_id: "b".to_string(),
        agent_seq: 1,
    };
    assert!(base < later);
    assert!(base < other_agent);
}

#[test]
fn t53_op_id_grammar_boundary() {
    let ok = format!("ckpt-{}-{}", "a".repeat(12), "0".repeat(16));
    crate::state_broker::validate::validate_op_id(&ok).unwrap();
    let too_long = "x".repeat(129);
    assert!(crate::state_broker::validate::validate_op_id(&too_long).is_err());
    assert!(crate::state_broker::validate::validate_op_id("has space").is_err());
}

#[test]
fn t54_commit_message_boundaries() {
    let source = MockCheckpointSource::new(1);
    let plan = plan_publish(&source, &cfg()).unwrap();
    let message = super::publisher::commit_message(&plan);
    crate::state_broker::validate::validate_message(&message).unwrap();
    assert!(message.len() <= 512);
    assert!(!message.contains('\n'));
    let _ = new_op_id(&source.checkpoint.commit);
}

#[test]
fn t55_definitive_refusals_are_not_reconcile_required() {
    let transport = seeded_transport();
    let newer = MockCheckpointSource::new(5);
    publish_landed(&transport, &newer);
    let older = MockCheckpointSource::new(1);
    match publish(&transport, &older, PublishOptions::default()) {
        PublishOutcome::Refused { .. } => {}
        other => panic!("expected Refused, got {other:?}"),
    }
    // A wrong-project token is FailedClosed, not a blocked state.
    let other_identity = PublisherIdentity::writer("0f0e0d0c-0b0a-4908-8706-050403020100");
    let outcome = publish_checkpoint(
        &transport,
        &older,
        &cfg(),
        &other_identity,
        &PublishOptions::default(),
        None,
    )
    .unwrap();
    assert!(matches!(outcome, PublishOutcome::FailedClosed { .. }));
}

#[test]
fn manifest_context_round_trip_for_mock_state() {
    // Guards the fixture itself: the mock's state.json parses and validates.
    let source = MockCheckpointSource::new(1);
    let checkpoint = source.resolve_pushed_checkpoint().unwrap();
    let parsed = CheckpointState::from_slice(&checkpoint.state_bytes).unwrap();
    assert_eq!(parsed.watermark, Some(checkpoint.watermark));
    let plan = plan_publish(&source, &cfg()).unwrap();
    plan.manifest
        .validate(&ManifestContext {
            project_uuid: UUID,
            state_ref: &cfg().state_ref,
            expected_source_commit: Some(&plan.source.commit),
            accounting: AccountingModel::Decoded,
        })
        .unwrap();
}

#[test]
fn t41_same_op_id_equal_identity_differing_payload_is_landed() {
    // A different compressor build produces different payload bytes for the
    // same semantic identity. The head records our op id; reconcile must
    // report Landed (provenance differs, identity matches), never Diverged.
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(6);
    let plan =
        plan_publish_with_op_id(&source, &cfg(), "ckpt-aaaaaaaaaaaa-0000000000000041").unwrap();
    let request = crate::state_broker::client::CommitRequest {
        expected_head: None,
        message: "crosslink checkpoint publish identity".to_string(),
        op_id: Some(plan.op_id.clone()),
        files: std::iter::once(crate::state_broker::client::CommitFile {
            path: MANIFEST_PATH.to_string(),
            content: plan.manifest_bytes.clone(),
        })
        .chain(plan.chunks.iter().enumerate().map(|(slot, chunk)| {
            crate::state_broker::client::CommitFile {
                path: super::chunk_path(slot as u32),
                content: chunk.clone(),
            }
        }))
        .collect(),
    };
    transport.commit(&request).unwrap();
    // Forge the payload digest only: semantic identity is unchanged.
    let mut manifest = super::manifest::CheckpointManifestV1::from_slice(
        &transport.file_bytes(MANIFEST_PATH).unwrap(),
    )
    .unwrap();
    manifest.payload_sha256 = "0".repeat(64);
    transport.corrupt_file(MANIFEST_PATH, serde_json::to_vec(&manifest).unwrap());

    let intent = super::publisher::ReconcileIntent::from_plan(&plan, None);
    match super::publisher::reconcile(&transport, &cfg(), &intent, None) {
        super::publisher::ReconcileVerdict::Landed { commit } => {
            assert_eq!(commit, transport.head().unwrap());
        }
        other => panic!("expected Landed, got {other:?}"),
    }
}

#[test]
fn t45_head_manifest_null_watermark_is_refused() {
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    let mut value: serde_json::Value =
        serde_json::from_slice(&transport.file_bytes(MANIFEST_PATH).unwrap()).unwrap();
    value["source"]["watermark"] = serde_json::Value::Null;
    transport.corrupt_file(MANIFEST_PATH, serde_json::to_vec(&value).unwrap());
    let plan = plan_publish(&source, &cfg()).unwrap();
    match super::publisher::classify_head(&transport, &cfg(), &plan).unwrap() {
        super::publisher::HeadVerdict::Refuse {
            reason: RefusalReason::HeadManifestUnreadable,
            ..
        } => {}
        other => panic!("expected HeadManifestUnreadable, got {other:?}"),
    }
}

// ── Projection (T24/T26/T37/T44) ─────────────────────────────────────

#[test]
fn t37_projection_write_verify_and_staleness() {
    use super::projection::{
        read_marker, verify_derived_projection, write_derived_projection, PROJECTED_STATE_PATH,
    };
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    let dir = tempfile::tempdir().unwrap();

    let marker = write_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap();
    assert!(marker.complete);
    assert_eq!(marker.provenance, "journal_anchored");
    assert_eq!(marker.state_sha256, source.checkpoint.state_sha256);
    assert_eq!(marker.files.len(), 1);
    assert_eq!(marker.files[0].path, PROJECTED_STATE_PATH);
    let projected = std::fs::read(dir.path().join(PROJECTED_STATE_PATH)).unwrap();
    assert_eq!(projected, source.checkpoint.state_bytes);

    // Verify at the same head with the anchor: passes.
    let verified = verify_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap();
    assert_eq!(verified.head_commit, marker.head_commit);

    // A new publish makes the projection stale.
    let newer = MockCheckpointSource::new(2);
    publish_landed(&transport, &newer);
    let error = verify_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert!(error.message().contains("stale"), "{}", error.message());

    // A tampered projected file is refused.
    let head_after = transport.head().unwrap();
    assert_eq!(
        read_marker(dir.path()).unwrap().head_commit,
        marker.head_commit
    );
    assert_ne!(head_after, marker.head_commit);
    std::fs::write(dir.path().join(PROJECTED_STATE_PATH), b"tampered").unwrap();
    // Re-write the marker to point at the new head, then verify the digest.
    let mut forged = read_marker(dir.path()).unwrap();
    forged.head_commit = head_after;
    forged.complete = true;
    std::fs::write(
        dir.path().join(super::PROJECTION_MARKER_FILE),
        serde_json::to_vec_pretty(&forged).unwrap(),
    )
    .unwrap();
    let error = verify_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert!(
        error.message().contains("manifest") || error.message().contains("digest"),
        "{}",
        error.message()
    );
}

#[test]
fn t26_incomplete_projection_marker_is_refused() {
    use super::projection::{read_marker, verify_derived_projection, write_derived_projection};
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    let dir = tempfile::tempdir().unwrap();
    write_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap();
    let mut marker = read_marker(dir.path()).unwrap();
    marker.complete = false;
    std::fs::write(
        dir.path().join(super::PROJECTION_MARKER_FILE),
        serde_json::to_vec_pretty(&marker).unwrap(),
    )
    .unwrap();
    let error = verify_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert!(
        error.message().contains("incomplete"),
        "{}",
        error.message()
    );
}

#[test]
fn t44_advisory_projection_cannot_hydrate() {
    use super::projection::{read_marker, verify_derived_projection, write_derived_projection};
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    let dir = tempfile::tempdir().unwrap();
    write_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap();
    // Forge the marker's provenance to advisory (as a broker-only writer would).
    let mut marker = read_marker(dir.path()).unwrap();
    marker.provenance = "advisory".to_string();
    std::fs::write(
        dir.path().join(super::PROJECTION_MARKER_FILE),
        serde_json::to_vec_pretty(&marker).unwrap(),
    )
    .unwrap();
    // Authoritative hydration refuses it.
    let error = verify_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations {
            journal_anchored: true,
            ..Default::default()
        },
        Some(&source),
    )
    .unwrap_err();
    assert!(
        error.message().contains("journal_anchored") || error.message().contains("advisory"),
        "{}",
        error.message()
    );
    // An explicit advisory consumer may still read it.
    verify_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        None,
    )
    .unwrap();
}

#[test]
fn t24_projection_min_watermark_is_enforced() {
    use super::projection::{verify_derived_projection, write_derived_projection};
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    let dir = tempfile::tempdir().unwrap();
    write_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap();
    let error = verify_derived_projection(
        &transport,
        &cfg(),
        dir.path(),
        &ReadExpectations {
            min_watermark: Some(OrderingKey {
                timestamp: Utc::now() + chrono::Duration::days(1),
                agent_id: "driver".to_string(),
                agent_seq: 99,
            }),
            ..Default::default()
        },
        Some(&source),
    )
    .unwrap_err();
    assert!(
        error.message().contains("older than the required minimum"),
        "{}",
        error.message()
    );
}

#[test]
fn t39_inventory_blob_mismatch_is_refused() {
    // The live map disagrees with the head commit snapshot: the inventory
    // cross-check must catch it before any parse.
    let transport = seeded_transport();
    let source = MockCheckpointSource::new(1);
    publish_landed(&transport, &source);
    // Mutate only the live map (the head snapshot keeps the committed bytes).
    transport.upsert_file(MANIFEST_PATH, b"{\"schema\":\"forged\"}");
    let error = read_derived_checkpoint(
        &transport,
        &cfg(),
        &ReadExpectations::default(),
        Some(&source),
    )
    .unwrap_err();
    assert_eq!(defect_of(&error), ReaderDefect::InventoryMismatch.label());
}
