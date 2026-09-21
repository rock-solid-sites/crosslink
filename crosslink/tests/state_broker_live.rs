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
    println!("{}", serde_json::to_string_pretty(&summary).expect("summary"));
}
