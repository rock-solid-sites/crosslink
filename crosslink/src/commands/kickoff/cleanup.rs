// E-ana tablet — kickoff cleanup: remove stale agent artifacts
use anyhow::{bail, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use super::helpers::*;
use super::monitor::discover_agents;
use super::types::*;

/// Environment variable that must be set to `1` for the operator-only
/// blanket `--force` sweep. Agent launches never set this variable, so an
/// agent-invoked `cleanup --force` fails structurally instead of relying on
/// the agent "being careful" (ASES #349/#350 — the #227 incident class).
pub(super) const OPERATOR_ENV: &str = "CROSSLINK_OPERATOR";

/// What a cleanup invocation would remove, before any filesystem mutation.
///
/// Pure selection logic so the safety properties (exact `--only` matching,
/// active-agent refusal, blanket DONE-only partitioning) are unit-testable.
#[derive(Debug, Default)]
pub(super) struct CleanupPlan {
    /// Agents that will be removed (confirmed-DONE in the blanket path, or
    /// exactly the named agents in `--only` mode).
    pub to_clean: Vec<(AgentInfo, CleanupClass)>,
    /// STALE agents skipped by the non-force blanket path. Empty in
    /// `--only` mode (a named agent is either removed or refused).
    pub skipped_stale: Vec<(AgentInfo, CleanupClass)>,
    /// Agents that are still active and were never considered for removal
    /// (in `--only` mode: named agents that were refused for being active).
    pub active: Vec<(AgentInfo, CleanupClass)>,
}

/// `crosslink kickoff cleanup`
///
/// Discover and remove stale kickoff agent artifacts: completed tmux sessions,
/// worktrees with DONE sentinels, and orphaned worktrees whose sessions no
/// longer exist.
///
/// Removal is always explicit:
/// * `--only <id1,id2,...>` removes exactly the named agents (worktrees +
///   tmux + containers) and nothing else; it refuses to touch active agents
///   and errors on unknown IDs. Mutually exclusive with `--force`/`--keep`.
/// * The blanket path removes confirmed-DONE agents only, unless the
///   operator-only `--force` is used (requires `CROSSLINK_OPERATOR=1`).
/// * Every non-dry-run removal prints the full blast radius and requires
///   explicit confirmation: `--yes`, or an interactive [y/N] prompt.
pub fn cleanup(
    crosslink_dir: &Path,
    dry_run: bool,
    force: bool,
    keep: usize,
    json_output: bool,
    only: &[String],
    yes: bool,
) -> Result<()> {
    let agents = discover_agents(crosslink_dir)?;
    cleanup_with_agents(
        crosslink_dir,
        dry_run,
        force,
        keep,
        json_output,
        only,
        yes,
        agents,
        &mut default_confirmation,
    )?;
    Ok(())
}

/// Shared implementation, parameterised over agent discovery and the
/// confirmation source so the safety gates are testable without real
/// worktrees, tmux sessions, or stdin.
#[allow(clippy::too_many_arguments)]
pub(super) fn cleanup_with_agents(
    crosslink_dir: &Path,
    dry_run: bool,
    force: bool,
    keep: usize,
    json_output: bool,
    only: &[String],
    yes: bool,
    agents: Vec<AgentInfo>,
    confirmer: &mut dyn FnMut() -> bool,
) -> Result<Vec<CleanupResult>> {
    // The blanket --force sweep is operator-only (ASES #349/#350): without
    // the operator environment variable, refuse before even planning so an
    // agent invocation can never reach the STALE sweep.
    if force && !operator_gate_ok() {
        bail!(
            "cleanup --force is operator-only: set {OPERATOR_ENV}=1 to run a \
             blanket STALE sweep. Agent invocations cannot bulk-delete stale \
             agents (ASES #349/#350)."
        );
    }

    let CleanupPlan {
        to_clean,
        skipped_stale,
        active,
    } = plan_cleanup(agents, only, force, keep)?;

    // --- Dry-run / JSON output ---
    if json_output {
        #[derive(Serialize)]
        struct CleanupPlanJson {
            to_clean: Vec<CleanupPlanEntry>,
            skipped_stale: Vec<CleanupPlanEntry>,
            active: Vec<CleanupPlanEntry>,
            dry_run: bool,
        }
        #[derive(Serialize)]
        struct CleanupPlanEntry {
            id: String,
            status: String,
            class: CleanupClass,
            worktree: String,
            session: Option<String>,
            docker: Option<String>,
        }
        let to_entry = |items: &[(AgentInfo, CleanupClass)]| -> Vec<CleanupPlanEntry> {
            items
                .iter()
                .map(|(a, c)| CleanupPlanEntry {
                    id: a.id.clone(),
                    status: a.status.clone(),
                    class: c.clone(),
                    worktree: a.worktree.clone(),
                    session: a.session.clone(),
                    docker: a.docker.clone(),
                })
                .collect()
        };
        let plan_json = CleanupPlanJson {
            to_clean: to_entry(&to_clean),
            skipped_stale: to_entry(&skipped_stale),
            active: to_entry(&active),
            dry_run,
        };
        println!("{}", serde_json::to_string_pretty(&plan_json)?);
        if dry_run {
            return Ok(Vec::new());
        }
    }

    if to_clean.is_empty() && skipped_stale.is_empty() {
        if !json_output {
            println!("No agents to clean up.");
        }
        return Ok(Vec::new());
    }

    if dry_run || !json_output {
        // Print the plan
        if !to_clean.is_empty() {
            println!("Cleanup candidates:\n");
            for (agent, class) in &to_clean {
                let class_label = match class {
                    CleanupClass::Done => "DONE  ",
                    CleanupClass::Stale => "STALE ",
                    CleanupClass::Active => "      ",
                };
                let wt_display = if agent.worktree.is_empty() {
                    "-".to_string()
                } else {
                    std::path::Path::new(&agent.worktree)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&agent.worktree)
                        .to_string()
                };
                let session_info = agent
                    .session
                    .as_deref()
                    .map_or_else(|| "tmux: exited".to_string(), |s| format!("tmux: {s}"));
                let docker_info = agent
                    .docker
                    .as_deref()
                    .map(|d| format!("  docker: {d}"))
                    .unwrap_or_default();
                println!(
                    "  {}  {:<40} worktree: {:<30} {}{}",
                    class_label, agent.id, wt_display, session_info, docker_info
                );
            }
        }

        if !skipped_stale.is_empty() {
            println!(
                "\n{} stale agent(s) skipped (operator-only sweep: {OPERATOR_ENV}=1 with --force):",
                skipped_stale.len()
            );
            for (agent, _) in &skipped_stale {
                let wt_display = if agent.worktree.is_empty() {
                    "-".to_string()
                } else {
                    std::path::Path::new(&agent.worktree)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&agent.worktree)
                        .to_string()
                };
                println!("  STALE  {:<40} worktree: {}", agent.id, wt_display);
            }
        }

        if dry_run {
            let wt_count = to_clean
                .iter()
                .filter(|(a, _)| !a.worktree.is_empty())
                .count();
            let tmux_count = to_clean.iter().filter(|(a, _)| a.session.is_some()).count();
            let docker_count = to_clean.iter().filter(|(a, _)| a.docker.is_some()).count();
            println!();
            print!("Would remove {wt_count} worktree(s)");
            if tmux_count > 0 {
                print!(", kill {tmux_count} tmux session(s)");
            }
            if docker_count > 0 {
                print!(", remove {docker_count} container(s)");
            }
            println!(".");
            println!("Run without --dry-run to proceed.");
            return Ok(Vec::new());
        }

        println!();
    }

    // --- Confirmation: the full blast radius was printed above ---
    if !yes {
        if json_output {
            bail!(
                "cleanup: --json requires --yes to confirm the blast radius \
                 (refusing non-interactive removal)"
            );
        }
        if !confirmer() {
            println!("Cleanup cancelled.");
            return Ok(Vec::new());
        }
    }

    // --- Execute cleanup ---
    let mut results: Vec<CleanupResult> = Vec::new();

    for (agent, class) in &to_clean {
        let mut result = CleanupResult {
            id: agent.id.clone(),
            class: class.clone(),
            worktree_removed: false,
            tmux_killed: false,
            container_removed: false,
            error: None,
        };

        // 1. Kill tmux session if it still exists
        if let Some(ref session_name) = agent.session {
            match Command::new("tmux")
                .args(["kill-session", "-t", session_name])
                .output()
            {
                Ok(o) if o.status.success() => {
                    result.tmux_killed = true;
                    if !json_output {
                        println!("  Killed tmux session: {session_name}");
                    }
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    tracing::warn!(
                        "failed to kill tmux session {}: {}",
                        session_name,
                        stderr.trim()
                    );
                }
                Err(e) => {
                    tracing::warn!("tmux error for {}: {}", session_name, e);
                }
            }
        }

        // 2. Remove Docker/Podman container if present
        if let Some(ref container_name) = agent.docker {
            for runtime in &["docker", "podman"] {
                if command_available(runtime) {
                    if let Ok(o) = Command::new(runtime)
                        .args(["rm", "-f", container_name])
                        .output()
                    {
                        if o.status.success() {
                            result.container_removed = true;
                            if !json_output {
                                println!("  Removed {runtime} container: {container_name}");
                            }
                            break;
                        }
                    }
                }
            }
        }

        // 3. Reconcile the matching pipeline run row before the worktree
        //    disappears (GH#614): once removed, lazy display reconcile can only
        //    ever see it as "aborted". Capture the truth now from the agent's
        //    terminal status — DONE → completed, failed → failed, anything else
        //    (stale/timed-out/stopped) → aborted.
        if !agent.worktree.is_empty() {
            if let Some(root) = crosslink_dir.parent() {
                let pipeline_status = match agent.status.as_str() {
                    "done" => "completed",
                    "failed" => "failed",
                    _ => "aborted",
                };
                let _ = super::pipeline::reconcile_completion_by_worktree(
                    root,
                    &agent.worktree,
                    pipeline_status,
                );
            }
        }

        // 4. Remove the git worktree
        if !agent.worktree.is_empty() && std::path::Path::new(&agent.worktree).exists() {
            match Command::new("git")
                .args(["worktree", "remove", "--force", &agent.worktree])
                .output()
            {
                Ok(o) if o.status.success() => {
                    result.worktree_removed = true;
                    let wt_display = std::path::Path::new(&agent.worktree)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&agent.worktree);
                    if !json_output {
                        println!("  Removed worktree: {wt_display}");
                    }
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    let msg = format!("git worktree remove failed: {}", stderr.trim());
                    tracing::warn!("{}", msg);
                    result.error = Some(msg);
                }
                Err(e) => {
                    let msg = format!("git worktree remove error: {e}");
                    tracing::warn!("{}", msg);
                    result.error = Some(msg);
                }
            }
        }

        results.push(result);
    }

    // --- Summary ---
    if json_output {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        let wt_removed = results.iter().filter(|r| r.worktree_removed).count();
        let tmux_killed = results.iter().filter(|r| r.tmux_killed).count();
        let containers_removed = results.iter().filter(|r| r.container_removed).count();
        let errors = results.iter().filter(|r| r.error.is_some()).count();

        println!();
        print!("Cleaned up {} agent(s)", results.len());
        if wt_removed > 0 {
            print!(": {wt_removed} worktree(s)");
        }
        if tmux_killed > 0 {
            print!(", {tmux_killed} tmux session(s)");
        }
        if containers_removed > 0 {
            print!(", {containers_removed} container(s)");
        }
        if errors > 0 {
            print!(" ({errors} error(s))");
        }
        println!(".");
    }

    Ok(results)
}

/// Decide what a cleanup invocation would remove, without touching the
/// filesystem.
pub(super) fn plan_cleanup(
    agents: Vec<AgentInfo>,
    only: &[String],
    force: bool,
    keep: usize,
) -> Result<CleanupPlan> {
    // --- Selective mode: --only <id1,id2,...> ---
    if !only.is_empty() {
        if force {
            bail!("cleanup: --only cannot be combined with --force");
        }
        if keep > 0 {
            bail!("cleanup: --only cannot be combined with --keep");
        }
        let resolved = resolve_only_agents(&agents, only)?;
        let mut to_clean = Vec::new();
        let mut active = Vec::new();
        for agent in resolved {
            let class = classify_agent(agent);
            if class == CleanupClass::Active {
                active.push((agent.clone(), class));
            } else {
                to_clean.push((agent.clone(), class));
            }
        }
        if !active.is_empty() {
            bail!(
                "cleanup --only: refusing to remove active agent(s): {} (stop them first)",
                active
                    .iter()
                    .map(|(a, _)| a.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        return Ok(CleanupPlan {
            to_clean,
            skipped_stale: Vec::new(),
            active,
        });
    }

    // --- Blanket path ---
    let (active, removable): (Vec<_>, Vec<_>) = agents
        .into_iter()
        .map(|a| {
            let class = classify_agent(&a);
            (a, class)
        })
        .partition(|(_, class)| *class == CleanupClass::Active);

    // Without --force, only clean Done agents (not Stale)
    let (mut to_clean, skipped_stale): (Vec<_>, Vec<_>) = if force {
        (removable, vec![])
    } else {
        removable
            .into_iter()
            .partition(|(_, class)| *class == CleanupClass::Done)
    };

    // Sort by worktree path (as a proxy for creation order) so --keep works predictably
    to_clean.sort_by(|a, b| a.0.worktree.cmp(&b.0.worktree));

    // Apply --keep: keep the N most recent (last N items after sorting)
    let to_clean = if keep > 0 && to_clean.len() > keep {
        to_clean[..to_clean.len() - keep].to_vec()
    } else if keep > 0 && to_clean.len() <= keep {
        vec![] // keep all
    } else {
        to_clean
    };

    Ok(CleanupPlan {
        to_clean,
        skipped_stale,
        active,
    })
}

/// Resolve `--only` IDs against the discovered agents.
///
/// Accepts the agent ID exactly as stored, the worktree directory name, or a
/// `feature/...` / `feat-...` branch-style slug (mirroring `kickoff status`).
/// Fails closed: any named ID that does not resolve to an agent is an error,
/// so `--only` can never silently drop part of the requested removal.
pub(super) fn resolve_only_agents<'a>(
    agents: &'a [AgentInfo],
    only: &[String],
) -> Result<Vec<&'a AgentInfo>> {
    let mut resolved: Vec<&'a AgentInfo> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for raw in only {
        let id = raw.trim();
        if id.is_empty() {
            continue;
        }
        let slug = id
            .strip_prefix("feature/")
            .or_else(|| id.strip_prefix("feat-"))
            .unwrap_or(id);
        let wt_slug = slug.rsplit("--").next().unwrap_or(slug);
        let matched = agents.iter().find(|a| {
            a.id == id
                || a.id == format!("driver--{wt_slug}")
                || Path::new(&a.worktree)
                    .file_name()
                    .map_or(false, |n| n.to_string_lossy() == wt_slug)
        });
        match matched {
            Some(agent) => {
                if !resolved.iter().any(|r| r.id == agent.id) {
                    resolved.push(agent);
                }
            }
            None => missing.push(id.to_string()),
        }
    }
    if !missing.is_empty() {
        bail!("cleanup --only: agent(s) not found: {}", missing.join(", "));
    }
    Ok(resolved)
}

/// The blanket `--force` sweep is operator-only. Agents never have
/// `CROSSLINK_OPERATOR=1` in their launch environment, so an agent-invoked
/// `cleanup --force` fails in [`cleanup_with_agents`] before planning or
/// printing anything. The operator runs:
/// `CROSSLINK_OPERATOR=1 crosslink kickoff cleanup --force [--yes]`.
pub(super) fn operator_gate_ok() -> bool {
    std::env::var(OPERATOR_ENV).map_or(false, |v| v.trim() == "1")
}

/// Interactive [y/N] confirmation used by the public CLI entry point.
/// Fail-closed: any read error or non-"y" answer cancels the cleanup.
fn default_confirmation() -> bool {
    eprint!("Proceed with removal? [y/N] ");
    let _ = io::stdout().flush();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(_) => input.trim().eq_ignore_ascii_case("y"),
        Err(_) => false,
    }
}
