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
        StateBrokerCommands::PublishCheckpoint {
            dry_run,
            verify_full,
            publisher_id,
        } => publish_checkpoint(crosslink_dir, json, dry_run, verify_full, publisher_id),
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
                    if baseline_matches {
                        "matches"
                    } else {
                        "MISMATCH"
                    },
                    baseline_expected,
                    baseline_observed.as_deref().unwrap_or("(unreadable)")
                );
                println!(
                    "  registry:      {}",
                    if registry_present {
                        "present"
                    } else {
                        "absent"
                    }
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
    base_url.split_once("://").map_or_else(
        || base_url.to_string(),
        |(scheme, rest)| {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}")
        },
    )
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

/// `crosslink state-broker publish-checkpoint` — one CDP-1 derived publish.
///
/// The repository↔project-UUID binding is enforced before anything else. A
/// dry run plans and checks capacity with **zero** broker calls. A real run
/// performs at most the CAS attempts the protocol allows and never writes
/// without read-back verification.
fn publish_checkpoint(
    crosslink_dir: &Path,
    json: bool,
    dry_run: bool,
    verify_full: bool,
    publisher_id: Option<String>,
) -> Result<()> {
    use crate::state_broker::cdp1::{
        publish_checkpoint as cdp1_publish, AttemptStore, Cdp1Config, GitCheckpointSource,
        PublishOptions, PublishOutcome, PublisherIdentity, RepositoryBinding,
    };

    let backend =
        StateBackend::resolve(crosslink_dir).context("resolving the configured state backend")?;
    let StateBackend::Broker(config) = backend else {
        anyhow::bail!(
            "state backend is local; set CROSSLINK_STATE_BACKEND=broker (and the broker \
             variables) before publishing a derived checkpoint"
        );
    };

    // ADR-802 §16 / spec §12 item 9: repo↔project-UUID binding is mandatory.
    let binding = RepositoryBinding::require_for(crosslink_dir, &config)
        .context("enforcing the repository↔project binding")?;

    let client = StateBrokerClient::new(config.clone()).context("building the broker HTTP client")?;
    let identity = if dry_run {
        // Dry runs never touch the network; the scope is assumed, not checked.
        PublisherIdentity {
            project_uuid: config.project_uuid().to_string(),
            can_write: true,
        }
    } else {
        let who = client.whoami().context("broker whoami failed")?;
        PublisherIdentity {
            project_uuid: who.project_uuid,
            can_write: who.scopes.iter().any(|scope| scope == "state:write"),
        }
    };

    let publisher = publisher_id.unwrap_or_else(|| default_publisher_id(crosslink_dir, &identity));
    let cdp1 = Cdp1Config::from_broker(&config, publisher);
    let source = GitCheckpointSource::from_crosslink_dir(crosslink_dir)
        .context("building the git checkpoint source")?;
    let store = AttemptStore::new(crosslink_dir);
    let options = PublishOptions {
        dry_run,
        verify_full,
        max_retries: 1,
    };

    let outcome = cdp1_publish(&client, &source, &cdp1, &identity, &options, Some(&store))
        .context("CDP-1 publish failed before an outcome was reached")?;

    if json {
        println!("{}", outcome_json(&outcome, &binding)?);
    } else {
        print_outcome(&outcome);
    }

    match outcome {
        PublishOutcome::Landed { .. }
        | PublishOutcome::LandedSuperseded { .. }
        | PublishOutcome::AlreadyCurrent { .. }
        | PublishOutcome::DryRun { .. } => Ok(()),
        PublishOutcome::NotLanded { .. }
        | PublishOutcome::Refused { .. }
        | PublishOutcome::ReconcileRequired { .. }
        | PublishOutcome::Diverged { .. }
        | PublishOutcome::FailedClosed { .. } => {
            anyhow::bail!("derived publish did not land; see the outcome above")
        }
    }
}

/// Default publisher identity: the configured agent id, else the token id.
fn default_publisher_id(
    crosslink_dir: &Path,
    identity: &crate::state_broker::cdp1::PublisherIdentity,
) -> String {
    if let Ok(Some(agent)) = crate::identity::AgentConfig::load(crosslink_dir) {
        let id = agent.agent_id;
        if !id.is_empty()
            && id.len() <= 64
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return id;
        }
    }
    // The token id is not guaranteed to satisfy the agent-id charset.
    let sanitized: String = identity
        .project_uuid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("publisher-{}", &sanitized[..sanitized.len().min(8)])
}

fn outcome_json(
    outcome: &crate::state_broker::cdp1::PublishOutcome,
    binding: &crate::state_broker::cdp1::RepositoryBinding,
) -> Result<String> {
    use crate::state_broker::cdp1::PublishOutcome;
    let value = match outcome {
        PublishOutcome::DryRun { plan } => serde_json::json!({
            "outcome": "dry_run",
            "broker_calls": 0,
            "plan": {
                "op_id": plan.op_id,
                "source_commit": plan.source_commit,
                "watermark": plan.watermark,
                "state_bytes": plan.state_bytes,
                "payload_bytes": plan.payload_bytes,
                "manifest_bytes": plan.manifest_bytes,
                "chunk_count": plan.chunk_count,
                "files": plan.files,
                "accounting": plan.accounting,
            },
        }),
        PublishOutcome::Landed { commit, op_id, attempts, manifest_sha256, chunk_count } => {
            serde_json::json!({
                "outcome": "landed",
                "commit": commit,
                "op_id": op_id,
                "attempts": attempts,
                "manifest_sha256": manifest_sha256,
                "chunk_count": chunk_count,
            })
        }
        PublishOutcome::LandedSuperseded { commit, head, op_id } => serde_json::json!({
            "outcome": "landed_superseded",
            "commit": commit,
            "head": head,
            "op_id": op_id,
        }),
        PublishOutcome::AlreadyCurrent { commit, op_id } => serde_json::json!({
            "outcome": "already_current",
            "commit": commit,
            "op_id": op_id,
        }),
        PublishOutcome::NotLanded { observed_head, op_id } => serde_json::json!({
            "outcome": "not_landed",
            "observed_head": observed_head,
            "op_id": op_id,
        }),
        PublishOutcome::Refused { reason, observed_head } => serde_json::json!({
            "outcome": "refused",
            "reason": reason.label(),
            "observed_head": observed_head,
        }),
        PublishOutcome::ReconcileRequired { reason, observed_head, detail } => serde_json::json!({
            "outcome": "reconcile_required",
            "reason": reason.label(),
            "observed_head": observed_head,
            "detail": detail,
        }),
        PublishOutcome::Diverged { reason, observed_head, detail } => serde_json::json!({
            "outcome": "diverged",
            "reason": reason.label(),
            "observed_head": observed_head,
            "detail": detail,
        }),
        PublishOutcome::FailedClosed { reason } => serde_json::json!({
            "outcome": "failed_closed",
            "reason": reason,
        }),
    };
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "project_uuid".to_string(),
            serde_json::Value::String(binding.project_uuid.clone()),
        );
        object.insert(
            "repository".to_string(),
            serde_json::Value::String(binding.repository.clone()),
        );
    }
    Ok(serde_json::to_string_pretty(&value)?)
}

fn print_outcome(outcome: &crate::state_broker::cdp1::PublishOutcome) {
    use crate::state_broker::cdp1::PublishOutcome;
    match outcome {
        PublishOutcome::DryRun { plan } => {
            println!("dry run: no broker calls were made");
            println!("  op_id:          {}", plan.op_id);
            println!("  source commit:  {}", plan.source_commit);
            println!(
                "  watermark:      {}/{}",
                plan.watermark.agent_id, plan.watermark.agent_seq
            );
            println!("  state bytes:    {}", plan.state_bytes);
            println!("  payload bytes:  {}", plan.payload_bytes);
            println!("  manifest bytes: {}", plan.manifest_bytes);
            println!("  chunks:         {} ({})", plan.chunk_count, plan.accounting);
            println!("  files:          {}", plan.files);
        }
        PublishOutcome::Landed { commit, op_id, attempts, chunk_count, .. } => {
            println!("landed {commit} (op {op_id}, {attempts} attempt(s), {chunk_count} chunk(s))");
        }
        PublishOutcome::LandedSuperseded { commit, head, .. } => {
            println!("landed {commit}, superseded by head {head}");
        }
        PublishOutcome::AlreadyCurrent { commit, op_id } => {
            println!("already current at {commit} (op {op_id})");
        }
        PublishOutcome::NotLanded { observed_head, .. } => {
            println!(
                "not landed; observed head {}",
                observed_head.as_deref().unwrap_or("(none)")
            );
        }
        PublishOutcome::Refused { reason, observed_head } => {
            println!(
                "refused: {} (head {})",
                reason.label(),
                observed_head.as_deref().unwrap_or("(none)")
            );
        }
        PublishOutcome::ReconcileRequired { reason, detail, .. } => {
            println!("reconcile required: {} — {detail}", reason.label());
        }
        PublishOutcome::Diverged { reason, detail, .. } => {
            println!("diverged: {} — {detail}", reason.label());
        }
        PublishOutcome::FailedClosed { reason } => println!("failed closed: {reason}"),
    }
}
