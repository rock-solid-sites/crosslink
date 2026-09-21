//! `crosslink state-broker` — read-only inspection of the configured
//! durable-state backend.
//!
//! This is the operator entry point for the broker transport introduced by
//! `.design/state-broker-transport.md`. `status` performs broker **read**
//! operations only (`whoami`, `state`); it never commits, never hydrates, and
//! never prints the bearer token.

use anyhow::{Context, Result};
use std::path::Path;

use crate::state_broker::{ProjectStateTransport, StateBackend, StateBrokerClient};
use crate::StateBrokerCommands;

/// Dispatch `crosslink state-broker <subcommand>`.
///
/// # Errors
///
/// Returns an error when backend resolution fails or a broker read fails.
pub fn run(command: StateBrokerCommands, crosslink_dir: &Path, json: bool) -> Result<()> {
    match command {
        StateBrokerCommands::Status => status(crosslink_dir, json),
    }
}

fn status(crosslink_dir: &Path, json: bool) -> Result<()> {
    let backend =
        StateBackend::resolve(crosslink_dir).context("resolving the configured state backend")?;

    match backend {
        StateBackend::Local => report_local(json),
        StateBackend::Broker(config) => {
            let broker_host = host_of(config.base_url());
            let project_uuid = config.project_uuid().to_string();
            let state_ref = config.state_ref();
            let client =
                StateBrokerClient::new(config).context("building the broker HTTP client")?;

            // Read-only operations only: no commit, no hydration.
            let who = client.whoami().context("broker whoami failed")?;
            let state = client.read_state().context("broker state read failed")?;

            anyhow::ensure!(
                who.project_uuid == state.project.uuid,
                "broker identity mismatch: whoami reports project {} but state reports {}",
                who.project_uuid,
                state.project.uuid
            );

            let head_commit = state.state.head_commit().map(str::to_string);
            let head_subject = state
                .state
                .head
                .as_ref()
                .map(|head| head.message.lines().next().unwrap_or_default().to_string());
            let baseline_matches = state.baseline.matches;
            let baseline_observed = state.baseline.observed_commit.clone();
            let baseline_expected = state.baseline.expected_commit.clone();
            let file_count = state.state.entries.len();
            let registry_present = state.registry.present;

            if json {
                let summary = serde_json::json!({
                    "backend": "broker",
                    "broker_host": broker_host,
                    "project_uuid": project_uuid,
                    "token_id": who.token_id,
                    "scopes": who.scopes,
                    "state_ref": state_ref,
                    "head_commit": head_commit,
                    "head_subject": head_subject,
                    "file_count": file_count,
                    "baseline_matches": baseline_matches,
                    "baseline_expected": baseline_expected,
                    "baseline_observed": baseline_observed,
                    "registry_present": registry_present,
                });
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!("state backend: broker");
                println!("  broker host:   {broker_host}");
                println!("  project uuid:  {project_uuid}");
                println!("  token id:      {}", who.token_id);
                println!("  scopes:        {}", who.scopes.join(", "));
                println!("  state ref:     {state_ref}");
                match &head_commit {
                    Some(commit) => println!("  head:          {commit}"),
                    None => println!("  head:          (none — no durable state yet)"),
                }
                if let Some(subject) = &head_subject {
                    println!("  head subject:  {subject}");
                }
                println!("  files:         {file_count}");
                println!(
                    "  baseline:      {} (expected {}, observed {})",
                    if baseline_matches { "matches" } else { "MISMATCH" },
                    baseline_expected,
                    baseline_observed.as_deref().unwrap_or("(unreadable)")
                );
                println!(
                    "  registry:      {}",
                    if registry_present { "present" } else { "absent" }
                );
            }
            Ok(())
        }
    }
}

fn report_local(json: bool) -> Result<()> {
    if json {
        let summary = serde_json::json!({
            "backend": "local",
            "broker_configured": false,
            "note": "existing local/direct behavior is active; broker selection requires \
                     CROSSLINK_STATE_BACKEND=broker or \"state_backend\": \"broker\" in hook-config.json",
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("state backend: local (default)");
        println!("  Crosslink is using its existing local/direct git behavior.");
        println!("  To select the broker backend, set CROSSLINK_STATE_BACKEND=broker plus");
        println!("  CROSSLINK_STATE_BROKER_URL, CROSSLINK_STATE_BROKER_TOKEN (or");
        println!("  CROSSLINK_STATE_BROKER_TOKEN_FILE), and CROSSLINK_STATE_PROJECT_UUID.");
    }
    Ok(())
}

/// Host-only label for the broker (path segments are never secret, but there
/// is no reason to print them).
fn host_of(base_url: &str) -> String {
    base_url
        .split_once("://")
        .map(|(scheme, rest)| {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}")
        })
        .unwrap_or_else(|| base_url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_label_never_includes_paths() {
        assert_eq!(
            host_of("https://broker.example.workers.dev/v1/x"),
            "https://broker.example.workers.dev"
        );
    }
}
