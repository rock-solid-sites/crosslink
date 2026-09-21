//! Live **read-only** probe against the deployed Crosslink State Broker.
//!
//! Ignored by default so the deterministic suite never depends on the network
//! or on production state. Run it explicitly after the Codex Cloud durability
//! experiment passes:
//!
//! ```text
//! CROSSLINK_STATE_BROKER_URL=https://<worker>.<subdomain>.workers.dev \
//! CROSSLINK_STATE_BROKER_TOKEN_FILE=/tmp/opencode/secrets/crosslink-state-broker.token \
//! CROSSLINK_STATE_PROJECT_UUID=<project-uuid> \
//! cargo test --test state_broker_live -- --ignored --nocapture
//! ```
//!
//! What it does: `health`, `whoami`, and `state` — all reads. It never commits,
//! never hydrates, and never prints the bearer token.
//!
//! Optional: set `CROSSLINK_STATE_EXPECTED_BASELINE` to a 40-hex commit sha to
//! also assert the broker's baseline observation matches.

use crosslink::state_broker::{ProjectStateTransport, StateBrokerClient, StateBrokerConfig};

#[test]
#[ignore = "live broker probe; run explicitly with --ignored (read-only)"]
fn live_broker_read_only_probe() {
    let config = StateBrokerConfig::from_env()
        .expect("broker configuration must be valid")
        .expect(
            "no broker configuration found: set CROSSLINK_STATE_BROKER_URL, \
             CROSSLINK_STATE_BROKER_TOKEN (or CROSSLINK_STATE_BROKER_TOKEN_FILE), and \
             CROSSLINK_STATE_PROJECT_UUID",
        );
    let expected_uuid = config.project_uuid().to_string();
    let state_ref = config.state_ref();
    let broker_host = config
        .base_url()
        .split_once("://")
        .map(|(scheme, rest)| {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}")
        })
        .unwrap_or_else(|| config.base_url().to_string());

    let client = StateBrokerClient::new(config).expect("client construction");

    let health = client.health().expect("broker health probe");
    assert_eq!(health.status, "ok", "broker is not healthy");

    let who = client.whoami().expect("broker whoami");
    assert_eq!(
        who.project_uuid, expected_uuid,
        "token is bound to a different project than CROSSLINK_STATE_PROJECT_UUID"
    );

    let state = client.read_state().expect("broker state read");
    assert_eq!(state.project.uuid, expected_uuid);

    if let Ok(expected) = std::env::var("CROSSLINK_STATE_EXPECTED_BASELINE") {
        let expected = expected.trim();
        assert_eq!(
            state.baseline.observed_commit.as_deref(),
            Some(expected),
            "baseline observation does not match CROSSLINK_STATE_EXPECTED_BASELINE"
        );
        assert!(state.baseline.matches, "broker reports a baseline mismatch");
    }

    let summary = serde_json::json!({
        "broker_host": broker_host,
        "broker_version": health.version,
        "token_id": who.token_id,
        "scopes": who.scopes,
        "project_uuid": state.project.uuid,
        "state_ref": state_ref,
        "state_exists": state.state.exists,
        "head_commit": state.state.head_commit(),
        "file_count": state.state.entries.len(),
        "baseline_matches": state.baseline.matches,
        "baseline_observed": state.baseline.observed_commit,
        "registry_present": state.registry.present,
        "writes_performed": 0,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).expect("summary")
    );
}

// ── CDP-1 pre-L1 evidence probes (read-only; ignored by default) ─────

/// Read-only probe: can the deployed broker serve blobs/verify at a
/// **non-head** commit? (Pre-L1 gate 2.)
///
/// Set `CROSSLINK_STATE_HISTORICAL_COMMIT` to a non-head commit sha and
/// `CROSSLINK_STATE_HISTORICAL_PATH` to a path that existed at that commit.
/// With no variables set, the probe reports the gap and does nothing.
#[test]
#[ignore = "live broker probe; run explicitly with --ignored (read-only)"]
fn live_historical_commit_read_probe() {
    let Ok(commit) = std::env::var("CROSSLINK_STATE_HISTORICAL_COMMIT") else {
        println!(
            "SKIP: set CROSSLINK_STATE_HISTORICAL_COMMIT (and \
             CROSSLINK_STATE_HISTORICAL_PATH) to probe non-head reads. \
             No write is performed; without a known non-head commit this gate \
             cannot be resolved from the client side."
        );
        return;
    };
    let path = std::env::var("CROSSLINK_STATE_HISTORICAL_PATH")
        .expect("CROSSLINK_STATE_HISTORICAL_PATH is required with the commit");
    let config = StateBrokerConfig::from_env()
        .expect("broker configuration must be valid")
        .expect("no broker configuration found");
    let client = StateBrokerClient::new(config).expect("client construction");
    let state = client.read_state().expect("state read");
    let head = state.state.head_commit().map(str::to_string);
    assert_ne!(
        head.as_deref(),
        Some(commit.as_str()),
        "the probe requires a NON-head commit"
    );
    let blob = client
        .read_blob(&path, Some(&commit))
        .expect("historical blob read");
    assert_eq!(blob.commit, commit);
    let bytes = blob.bytes().expect("blob digest");
    let verified = client
        .verify(&commit, &[path.clone()])
        .expect("historical verify");
    println!(
        "{}",
        serde_json::json!({
            "historical_commit": commit,
            "path": path,
            "bytes": bytes.len(),
            "sha256": blob.sha256,
            "verify_entries": verified.entries.len(),
            "head": head,
            "writes_performed": 0,
            "conclusion": "deployed broker serves non-head commits",
        })
    );
}

/// Read-only probe: corroborate the documented broker limits from the client
/// side, and report which parts remain unresolved without a write.
///
/// This sends only GETs: an over-limit `verify` path list and an over-long
/// `read_blob` path, both of which the broker rejects before any state access.
/// The decoded-vs-wire byte accounting **cannot** be resolved by reads; that
/// gap is reported explicitly.
#[test]
#[ignore = "live broker probe; run explicitly with --ignored (read-only)"]
fn live_limit_probe_reports_accounting_gap() {
    let config = StateBrokerConfig::from_env()
        .expect("broker configuration must be valid")
        .expect("no broker configuration found");
    let client = StateBrokerClient::new(config).expect("client construction");
    let state = client.read_state().expect("state read");
    let head = state
        .state
        .head_commit()
        .expect("the project must have a head for the probe")
        .to_string();

    // 33 distinct paths: the broker documents a 32-path verify cap.
    let paths: Vec<String> = (0..33)
        .map(|index| format!("probe/limit-{index}.json"))
        .collect();
    let verify_error = client
        .verify(&head, &paths)
        .expect_err("33 verify paths must be rejected");
    let over_long = format!("probe/{}", "x".repeat(300));
    let blob_error = client
        .read_blob(&over_long, Some(&head))
        .expect_err("an over-long path must be rejected");

    println!(
        "{}",
        serde_json::json!({
            "verify_path_cap_enforced": verify_error.code().as_str(),
            "verify_path_cap_message": verify_error.message(),
            "path_grammar_enforced": blob_error.code().as_str(),
            "path_grammar_message": blob_error.message(),
            "byte_accounting": "UNRESOLVED: decoded-vs-wire limit accounting cannot be \
                                determined by read-only requests; it requires the broker \
                                source or a reviewed live boundary probe (L2) on a \
                                synthetic project",
            "writes_performed": 0,
        })
    );
}
