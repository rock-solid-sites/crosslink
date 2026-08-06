// E-ana tablet — kickoff launch: agent launch infrastructure
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::identity::AgentConfig;

use super::helpers::*;
use super::types::*;

/// Resolve the correct `timeout` command for the current platform.
///
/// On macOS, `timeout` is not available by default. The GNU coreutils
/// package (via Homebrew) installs it as `gtimeout`.
/// Returns the command name to use, or an error with install instructions.
fn resolve_timeout_command(platform: &Platform) -> Result<&'static str> {
    if command_available("timeout") {
        return Ok("timeout");
    }
    if command_available("gtimeout") {
        return Ok("gtimeout");
    }
    bail!(
        "Neither `timeout` nor `gtimeout` found.\n{}",
        install_hint("timeout", platform)
    );
}

/// Generous backstop for the GNU timeout wrapper. The `--timeout` value is a
/// GUIDE (expected task duration, stored in .kickoff-metadata.json and shown
/// by `kickoff status`); the wrapper must never kill a healthy agent in normal
/// operation. Backstop = max(timeout * 24, 24h) so even a short guide gets a
/// 24-hour floor, far above any task ceiling (ASES #192).
pub(super) fn timeout_backstop_secs(timeout_secs: u64) -> u64 {
    std::cmp::max(timeout_secs.saturating_mul(24), 86_400)
}

/// Read an optional `kickoff.timeout_backstop_secs` override from
/// hook-config.json.
///
/// When set, it replaces the computed backstop for the GNU timeout wrapper
/// (which otherwise defaults to `max(timeout * 24, 24h)`). The `--timeout`
/// guide value is never changed — it stays in `.kickoff-metadata.json` for
/// `kickoff status` display. ASES #192.
pub(super) fn read_backstop_override(crosslink_dir: &Path) -> Option<u64> {
    let config_path = crosslink_dir.join("hook-config.json");
    let content = std::fs::read_to_string(&config_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed
        .get("kickoff")
        .and_then(|k| k.get("timeout_backstop_secs"))
        .and_then(serde_json::Value::as_u64)
        .filter(|v| *v > 0)
}

/// Read the `sandbox.command` setting from hook-config.json, if configured.
pub(super) fn read_sandbox_command(crosslink_dir: &Path) -> Option<String> {
    let config_path = crosslink_dir.join("hook-config.json");
    let content = std::fs::read_to_string(&config_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed
        .get("sandbox")
        .and_then(|s| s.get("command"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// Read the `agent.binary` setting from hook-config.json, if configured.
///
/// Returns the configured binary name, or `"claude"` when the key is absent,
/// empty, or the file cannot be parsed. This lets projects point kickoff at a
/// different agent CLI (e.g. `opencode`, `codex`) without code changes.

pub(super) fn read_watchdog_config(crosslink_dir: &Path) -> WatchdogConfig {
    let config_path = crosslink_dir.join("hook-config.json");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return WatchdogConfig::default();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
        return WatchdogConfig::default();
    };

    let Some(wd) = parsed.get("watchdog") else {
        return WatchdogConfig::default();
    };

    let mut cfg = WatchdogConfig::default();
    if let Some(v) = wd.get("enabled").and_then(serde_json::Value::as_bool) {
        cfg.enabled = v;
    }
    if let Some(v) = wd.get("staleness_secs").and_then(serde_json::Value::as_u64) {
        cfg.staleness_secs = v;
    }
    if let Some(v) = wd.get("max_nudges").and_then(serde_json::Value::as_u64) {
        // Deprecated (ASES #192): the nudge path is gone; read tolerantly for
        // config-file backward compatibility. The generated script never
        // uses this value.
        cfg.max_nudges = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = wd
        .get("check_interval_secs")
        .and_then(serde_json::Value::as_u64)
    {
        cfg.check_interval_secs = v;
    }
    if let Some(v) = wd
        .get("grace_period_secs")
        .and_then(serde_json::Value::as_u64)
    {
        cfg.grace_period_secs = v;
    }
    if let Some(v) = wd
        .get("stall_marker")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
    {
        cfg.stall_marker = Some(v.to_string());
    }
    cfg
}

/// Resolve the worktree-relative stall-evidence marker path the watchdog
/// will write, honoring `watchdog.stall_marker` from hook-config.json.
///
/// ASES #192: the watchdog writes the configured marker (default
/// `.kickoff-stalled`); the readers (`kickoff status`, `kickoff list`) must
/// resolve the same path or a custom marker's evidence is written to a
/// location `status`/`list` never read and silently disappears.
pub(super) fn stall_marker_path(worktree_dir: &Path, crosslink_dir: &Path) -> PathBuf {
    let marker = read_watchdog_config(crosslink_dir)
        .stall_marker
        .unwrap_or_else(|| ".kickoff-stalled".to_string());
    worktree_dir.join(marker)
}

/// Build the watchdog shell script that monitors heartbeat staleness and
/// records stall evidence.
///
/// The script is purely an evidence recorder — it NEVER kills or nudges the
/// agent (ASES #192). Kill/relaunch belongs to the future #146 watcher or to
/// manual `kickoff stop`. Its jobs are:
///
/// 1. Disarm on TERMINAL status: exit 0 when the `.kickoff-status` file's
///    CONTENT starts with `DONE`, `FAILED`, `CI_FAILED`, or `TIMEOUT` — not
///    when the file merely exists. Checking existence alone is wrong because
///    `launch` writes `LAUNCHING`/`RUNNING` into the file before the watchdog
///    even starts, which would make the watchdog exit on its first check and
///    leave the stall-evidence logic dead (fork bug #138).
/// 2. Disarm when the tmux session disappears.
/// 3. On stale heartbeat, write a stall-evidence marker at
///    `{worktree}/.kickoff-stalled` (or the configured `stall_marker`)
///    recording when the agent stalled. The marker is surfaced by
///    `kickoff status` / `kickoff list` and never pollutes agent PRs (the
///    marker is in `KICKOFF_EXCLUDE_PATTERNS`).
///
/// The script loops forever until a terminal condition — it never kills.
pub(super) fn build_watchdog_script(
    session_name: &str,
    worktree_dir: &Path,
    cfg: &WatchdogConfig,
) -> String {
    let stall_marker = cfg.stall_marker.as_deref().unwrap_or(".kickoff-stalled");
    // Use portable stat command — try GNU stat first, fall back to BSD
    format!(
        r#"sleep {grace}
while true; do
    sleep {interval}
    if [ -f "{worktree}/.kickoff-status" ]; then
        STATUS=$(cat "{worktree}/.kickoff-status" 2>/dev/null)
        case "$STATUS" in
            DONE*|FAILED*|CI_FAILED*|TIMEOUT*) exit 0 ;;
        esac
    fi
    if ! tmux has-session -t "{session}" 2>/dev/null; then exit 0; fi
    HB="{worktree}/.crosslink/.cache/last-heartbeat"
    if [ -f "$HB" ]; then
        LAST=$(stat -c %Y "$HB" 2>/dev/null || stat -f %m "$HB" 2>/dev/null)
        NOW=$(date +%s)
        AGE=$((NOW - LAST))
        if [ "$AGE" -gt {staleness} ]; then
            echo "stalled since $(date -u +%FT%TZ)" > "{worktree}/{stall_marker}"
        fi
    fi
done
"#,
        grace = cfg.grace_period_secs,
        interval = cfg.check_interval_secs,
        worktree = worktree_dir.display(),
        session = session_name,
        staleness = cfg.staleness_secs,
        stall_marker = stall_marker,
    )
}

/// Spawn a background watchdog process that monitors the agent's heartbeat
/// and records stall evidence (it never kills or nudges — ASES #192).
pub(super) fn spawn_watchdog(
    session_name: &str,
    worktree_dir: &Path,
    cfg: &WatchdogConfig,
) -> Result<()> {
    let script = build_watchdog_script(session_name, worktree_dir, cfg);

    Command::new("bash")
        .args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn watchdog process")?;

    Ok(())
}

/// Build the shell command string for launching a claude agent.
///
/// `claude_config_dir` is a caller-side environment variable that must be
/// propagated into the tmux session. When a tmux server is already running
/// on the host, `tmux new-session` inherits env from the tmux server's
/// frozen-at-startup environment rather than the caller's current shell —
/// so any `CLAUDE_CONFIG_DIR` set by the caller is silently lost (#555).
/// Baking it into the command string bypasses tmux env handling entirely.
///
/// The `CLAUDE_CONFIG_DIR=val` assignment is folded into the existing `env`
/// argv (between `-u CLAUDECODE` and `claude`) rather than placed as a shell
/// prefix. Shell prefix assignments (`VAR=val cmd`) are positional — they
/// only take effect when `VAR=val` precedes the *leading* command name. With
/// `timeout` (or any other wrapper) at column zero, a shell-prefix assignment
/// degenerates into a literal positional arg and `timeout` tries to exec it
/// as a binary path (`ENOENT`). Folding the assignment into `env`'s argv is
/// robust to additional wrappers prepended later (nice, chrt, bwrap, etc.).
/// See GH#587.
///
/// Timeout semantics (ASES #192): `timeout_secs` is the GUIDE — the expected
/// task duration recorded in `.kickoff-metadata.json` and shown by `kickoff
/// status`. The GNU timeout wrapper must NEVER kill a healthy agent in normal
/// operation, so the wrapper duration is a generous BACKSTOP computed by
/// [`timeout_backstop_secs`] (`max(timeout * 24, 24h)`), far above any task
/// ceiling. `backstop_override`, when `Some`, raises the wrapper duration
/// further (hook-config.json `kickoff.timeout_backstop_secs`).
///
/// When `sandbox_command` is set, the claude invocation is wrapped:
/// ```text
/// timeout 86400s my-sandbox --project-dir /path -- env -u CLAUDECODE CLAUDE_CONFIG_DIR='/p' claude ...
/// ```
/// Without sandbox:
/// ```text
/// timeout 86400s env -u CLAUDECODE CLAUDE_CONFIG_DIR='/p' claude ...
/// ```
/// (`86400s` is the backstop for a 1h guide.) When `claude_config_dir` is
/// `None`, the assignment is omitted.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_agent_command(
    agent_binary: &str,
    agent_type: &str,
    timeout_cmd: &str,
    timeout_secs: u64,
    model: &str,
    allowed_tools: &str,
    kickoff_file: &str,
    sandbox_command: Option<&str>,
    worktree_dir: &Path,
    skip_permissions: bool,
    claude_config_dir: Option<&str>,
    permission_mode: Option<&str>,
    backstop_override: Option<u64>,
) -> String {
    use crate::utils::shell_escape_arg;

    // Resolve permission posture for the spawned agent session:
    // Only Claude supports these permission flags — non-Claude agents
    // receive an empty skip_flag. The claude wrapper drops these anyway,
    // but gating here avoids passing unrecognized flags.
    let permission_flag_owned: String = if agent_binary == "claude" {
        match (permission_mode, skip_permissions) {
            (Some(mode), _) if !mode.is_empty() => {
                format!(" --permission-mode {}", shell_escape_arg(mode))
            }
            (_, true) => " --dangerously-skip-permissions".to_string(),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    let skip_flag = permission_flag_owned.as_str();
    // Fold `CLAUDE_CONFIG_DIR=val` into env(1)'s argv so the assignment takes
    // effect regardless of what wraps the resulting command (timeout, sandbox
    // wrappers, etc.). `env` accepts `KEY=val` between options and the
    // command, mutating only the environment passed to the exec'd process.
    // This is claude-specific (it is claude's config dir), so only emit it
    // when the configured agent binary is `claude` — other agents have no use
    // for it and would receive a meaningless env var.
    let env_assignment = if agent_binary == "claude" {
        claude_config_dir
            .filter(|v| !v.is_empty())
            .map(|v| format!("CLAUDE_CONFIG_DIR={} ", shell_escape_arg(v)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let escaped_model = shell_escape_arg(model);
    let escaped_tools = shell_escape_arg(allowed_tools);
    let escaped_kickoff = shell_escape_arg(kickoff_file);
    let escaped_agent = shell_escape_arg(agent_type);
    let claude_cmd = if agent_binary == "claude" {
        format!(
            "env -u CLAUDECODE {env_assignment}{agent_binary}{skip_flag} --model {escaped_model} --agent {escaped_agent} --allowedTools {escaped_tools} -- \"$(cat {escaped_kickoff})\""
        )
    } else {
        // Non-Claude agents: pipe the prompt via stdin, no --model/--allowedTools flags
        format!(
            "env -u CLAUDECODE {env_assignment}{agent_binary}{skip_flag} < {escaped_kickoff}"
        )
    };
    // The wrapper duration is the BACKSTOP, not the guide (ASES #192). A
    // healthy agent must never be killed by `--timeout`; the backstop only
    // exists as a destroyer guard against a wedged process, and even then it
    // sits far above any task ceiling.
    let backstop_secs = backstop_override.unwrap_or_else(|| timeout_backstop_secs(timeout_secs));
    sandbox_command.map_or_else(
        || format!("{timeout_cmd} {backstop_secs}s {claude_cmd}"),
        |cmd| {
            let escaped_worktree = shell_escape_arg(&worktree_dir.to_string_lossy());
            let expanded = cmd.replace("{{worktree}}", &escaped_worktree);
            format!("{timeout_cmd} {backstop_secs}s {expanded} {claude_cmd}")
        },
    )
}

/// Pre-flight check: verify all required external commands are present before
/// creating worktrees, branches, or sessions. Emits clear errors with install
/// instructions for any missing command.
pub(super) fn preflight_check(
    container: &ContainerMode,
    verify: &VerifyLevel,
    crosslink_dir: &Path,
) -> Result<PreflightResult> {
    let platform = detect_platform();
    let agent_binary = crate::utils::read_agent_binary(crosslink_dir);
    let mut missing: Vec<String> = Vec::new();

    // timeout (or gtimeout on macOS) — always required for agent timeout
    let timeout_cmd = match resolve_timeout_command(&platform) {
        Ok(cmd) => cmd,
        Err(e) => {
            missing.push(format!("{e}"));
            "timeout" // placeholder, won't be used since we'll bail
        }
    };

    // tmux — required for local (non-container) mode
    // On Windows, tmux is not available at all — bail early with a clear message.
    if *container == ContainerMode::None {
        if cfg!(target_os = "windows") {
            bail!(
                "Local kickoff mode requires tmux, which is not available on Windows.\n\
                 Use `--container docker` for agent kickoff on Windows."
            );
        }
        if !command_available("tmux") {
            missing.push(install_hint("tmux", &platform));
        }
    }

    // Agent CLI — required for local mode. The binary name comes from
    // hook-config.json's `agent.binary` (default "claude").
    if *container == ContainerMode::None && !command_available(&agent_binary) {
        missing.push(install_hint(&agent_binary, &platform));
    }

    // gh — required for CI/thorough verification
    if (*verify == VerifyLevel::Ci || *verify == VerifyLevel::Thorough) && !command_available("gh")
    {
        missing.push(install_hint("gh", &platform));
    }

    // docker/podman — required when using container mode
    match container {
        ContainerMode::Docker if !command_available("docker") => {
            missing.push(install_hint("docker", &platform));
        }
        ContainerMode::Podman if !command_available("podman") => {
            missing.push(install_hint("podman", &platform));
        }
        _ => {}
    }

    // sandbox command — validate the binary exists when configured
    let sandbox_command = read_sandbox_command(crosslink_dir);
    if let Some(ref cmd) = sandbox_command {
        // Extract the binary name (first word before any flags/templates)
        let binary = cmd.split_whitespace().next().unwrap_or(cmd);
        if !command_available(binary) {
            missing.push(format!(
                "`{binary}` (configured in hook-config.json sandbox.command) not found on PATH"
            ));
        }
    }

    if !missing.is_empty() {
        let header = format!(
            "Pre-flight check failed — {} missing command{}:\n",
            missing.len(),
            if missing.len() == 1 { "" } else { "s" }
        );
        let body = missing
            .iter()
            .enumerate()
            .map(|(i, msg)| format!("{}. {}", i + 1, msg))
            .collect::<Vec<_>>()
            .join("\n\n");
        bail!("{header}{body}");
    }

    Ok(PreflightResult {
        timeout_cmd,
        sandbox_command,
    })
}

/// Get the main git repository root, resolving through worktrees.
///
/// Uses `git rev-parse --show-toplevel` to find the current repo, then
/// `resolve_main_repo_root()` to follow worktree links back to the main
/// repository. This ensures worktrees are always created relative to the
/// main repo, not inside internal directories like `.crosslink/` (#425).
pub(super) fn repo_root() -> Result<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("Not inside a git repository");
    }
    let toplevel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let toplevel_path = std::path::PathBuf::from(&toplevel);

    // Resolve through worktrees to the main repo root (#425)
    Ok(crate::utils::resolve_main_repo_root(&toplevel_path).unwrap_or(toplevel_path))
}

/// Create a feature branch and worktree for the agent.
///
/// The worktree is created at `<repo_root>/.worktrees/<slug>`. A safety
/// guard prevents worktrees from landing inside internal directories
/// like `.crosslink/` or `.git/` (#425).
pub(super) fn create_worktree(
    repo_root: &Path,
    slug: &str,
    base_branch: Option<&str>,
) -> Result<(std::path::PathBuf, String)> {
    let branch_name = format!("feature/{slug}");
    let worktree_dir = repo_root.join(".worktrees").join(slug);

    // Safety guard: reject worktree paths that land inside internal directories (#425)
    let canonical_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    for forbidden in [".crosslink", ".git"] {
        let forbidden_dir = canonical_root.join(forbidden);
        if let Ok(canonical_wt) = worktree_dir.canonicalize() {
            if canonical_wt.starts_with(&forbidden_dir) {
                bail!(
                    "Worktree path {} would land inside {}/. \
                     This usually means repo_root resolved to an internal directory. \
                     Please run this command from the main repository root.",
                    worktree_dir.display(),
                    forbidden
                );
            }
        }
    }

    if worktree_dir.exists() {
        bail!(
            "Worktree already exists at {}. Remove it first or use --branch to target an existing branch.",
            worktree_dir.display()
        );
    }

    // Determine base ref
    let base = base_branch.unwrap_or("HEAD");

    // Handle existing branch refs from prior phases (#481).
    // A branch may exist from a previous kickoff/swarm phase that was
    // already merged. Rather than failing, clean it up automatically.
    let branch_exists = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--verify", &branch_name])
        .output()
        .is_ok_and(|o| o.status.success());

    if branch_exists {
        // Check if the branch has an active worktree
        let wt_output = Command::new("git")
            .current_dir(repo_root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .context("Failed to list worktrees")?;
        let wt_list = String::from_utf8_lossy(&wt_output.stdout);
        let has_active_worktree = wt_list
            .lines()
            .any(|line| line.starts_with("branch ") && line.ends_with(&branch_name));

        if has_active_worktree {
            bail!(
                "Branch '{branch_name}' already exists and has an active worktree. \
                 Clean up the worktree first with: git worktree remove <path>"
            );
        }

        // Check if the branch is fully merged into the base
        let is_merged = Command::new("git")
            .current_dir(repo_root)
            .args(["merge-base", "--is-ancestor", &branch_name, base])
            .output()
            .is_ok_and(|o| o.status.success());

        if is_merged {
            // Branch is fully merged — safe to delete and recreate
            tracing::info!(
                "branch '{}' exists from a prior phase and is fully merged, recreating",
                branch_name
            );
            let delete_output = Command::new("git")
                .current_dir(repo_root)
                .args(["branch", "-d", &branch_name])
                .output()
                .context("Failed to delete merged branch")?;
            if !delete_output.status.success() {
                let stderr = String::from_utf8_lossy(&delete_output.stderr);
                bail!(
                    "Branch '{}' is merged but could not be deleted: {}",
                    branch_name,
                    stderr.trim()
                );
            }
        } else {
            bail!(
                "Branch '{branch_name}' already exists and has unmerged changes. \
                 Either merge it first, delete it manually with \
                 `git branch -D {branch_name}`, or use a different slug."
            );
        }
    }

    // Create the worktree with a new branch
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["worktree", "add", "-b", &branch_name])
        .arg(&worktree_dir)
        .arg(base)
        .output()
        .context("Failed to create git worktree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create worktree: {}", stderr.trim());
    }

    Ok((worktree_dir, branch_name))
}

/// Ensure the worktree has the heartbeat hook — and its shared-config
/// dependency — so liveness evidence actually flows.
///
/// `crosslink init` writes both, but init short-circuits when `.crosslink/`
/// and `.claude/` already exist — the common case for a worktree freshly
/// checked out from a branch that has both committed — and `.claude/hooks/`
/// is gitignored, so a fresh worktree may lack `heartbeat.py` **and**
/// `crosslink_config.py`. `heartbeat.py` imports
/// `from crosslink_config import find_crosslink_binary` at module top, so
/// writing the hook without its sibling leaves the hook crashing with
/// `ModuleNotFoundError` on every `PostToolUse`: `.crosslink/.cache/
/// last-heartbeat` is never written and the watchdog has no liveness
/// evidence to record (ASES #192 / #135 Phase 2).
///
/// This pass writes both files when missing, from the same bundled resources
/// `crosslink init` uses. Best-effort: if either cannot be written, kickoff
/// proceeds — liveness evidence is degraded, not fatal.
pub(super) fn ensure_worktree_hooks(worktree_dir: &Path) {
    let wt_hooks = worktree_dir.join(".claude").join("hooks");
    if !wt_hooks.join("crosslink_config.py").exists()
        || !wt_hooks.join("heartbeat.py").exists()
    {
        let _ = std::fs::create_dir_all(&wt_hooks);
        if !wt_hooks.join("crosslink_config.py").exists() {
            let _ = std::fs::write(
                wt_hooks.join("crosslink_config.py"),
                crate::commands::init::CROSSLINK_CONFIG_PY,
            );
        }
        if !wt_hooks.join("heartbeat.py").exists() {
            let _ = std::fs::write(
                wt_hooks.join("heartbeat.py"),
                crate::commands::init::HEARTBEAT_PY,
            );
        }
    }
}

/// Initialize crosslink and agent identity in the worktree.
pub(super) fn init_worktree_agent(
    worktree_dir: &Path,
    crosslink_dir: &Path,
    compact_name: &str,
) -> Result<String> {
    // Run `crosslink init` in the worktree. Plain init (no --force) is
    // idempotent: it short-circuits when `.crosslink/` and `.claude/` already
    // exist, which is the common case for a worktree freshly checked out
    // from a branch that has both committed. We keep `--skip-signing` and
    // `--defaults` to suppress the TUI walkthrough on the rare path where
    // init actually has work to do. Dropping `--force` here prevents the
    // worktree's `hook-config.json` from being re-templated and leaking a
    // spurious diff into every agent-produced PR. See GH#583.
    let output = Command::new("crosslink")
        .current_dir(worktree_dir)
        .args(["init", "--skip-signing", "--defaults"])
        .output()
        .context("Failed to run crosslink init in worktree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("crosslink init in worktree: {}", stderr.trim());
    }

    // ASES #192 / #135 Phase 2: ensure the heartbeat hook (and its
    // crosslink_config.py dependency) exists in the worktree even when init
    // short-circuited. Without it, no heartbeat mtime flows and liveness
    // evidence is empty.
    ensure_worktree_hooks(worktree_dir);

    // Use the compact name as the agent ID directly
    let agent_id = compact_name.to_string();

    // Initialize agent identity with its own signing key (#505).
    // Previous approach inherited the parent's key with no_key=true, but
    // that failed when no parent agent config existed (e.g. driver-invoked
    // kickoff). Now each subagent gets a dedicated keypair, and is
    // auto-approved since the driver explicitly launched it.
    let wt_crosslink = worktree_dir.join(".crosslink");
    if wt_crosslink.exists() {
        // Only init if not already configured
        if AgentConfig::load(&wt_crosslink)?.is_none() {
            // Kickoff subagent worktree → `AgentRole::Agent` so hub
            // commits from this worktree sign with the agent's own
            // key and attribute distinctly. See #718.
            if let Err(e) = super::super::agent::init(
                &wt_crosslink,
                &agent_id,
                Some(&format!("Kickoff agent for: {compact_name}")),
                false, // generate dedicated signing key
                false,
                crate::identity::AgentRole::Agent,
            ) {
                tracing::warn!("could not initialize agent identity in worktree: {e} — agent will work without its own identity");
            }

            // Auto-approve: the driver explicitly invoked kickoff, so trust
            // is implicit. This eliminates the manual sync → pending → approve
            // workflow that blocked autonomous agent operation.
            if let Err(e) = super::super::trust::approve(crosslink_dir, &agent_id) {
                tracing::warn!(
                    "could not auto-approve agent '{}': {e} — run `crosslink trust approve {}` manually",
                    agent_id, agent_id
                );
            }
        }
    }

    // Sync coordination state
    let output = Command::new("crosslink")
        .current_dir(worktree_dir)
        .args(["sync"])
        .output();

    if let Ok(o) = output {
        if !o.status.success() {
            tracing::warn!("crosslink sync in worktree returned non-zero");
        }
    }

    Ok(agent_id)
}

/// Exclude kickoff files from git tracking.
pub(super) fn exclude_kickoff_files(worktree_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .current_dir(worktree_dir)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .context("Failed to get git common dir")?;

    let common_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let exclude_path = std::path::PathBuf::from(&common_dir).join("info/exclude");

    // Ensure parent directory exists
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    let additions = missing_exclude_patterns(&existing);

    if !additions.is_empty() {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude_path)
            .context("Failed to open git exclude file")?;
        for pattern in additions {
            writeln!(file, "{pattern}")?;
        }
    }

    Ok(())
}

/// Launch the agent as a local tmux process.
#[allow(clippy::too_many_arguments)]
pub(super) fn launch_local(
    agent_binary: &str,
    agent_type: &str,
    worktree_dir: &Path,
    session_name: &str,
    model: &str,
    allowed_tools: &str,
    timeout: Duration,
    timeout_cmd: &str,
    sandbox_command: Option<&str>,
    crosslink_dir: &Path,
    skip_permissions: bool,
    permission_mode: Option<&str>,
) -> Result<()> {
    // Create the tmux session
    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            session_name,
            "-c",
            &worktree_dir.to_string_lossy(),
        ])
        .output()
        .context("Failed to create tmux session")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create tmux session: {}", stderr.trim());
    }

    // Propagate the caller's CLAUDE_CONFIG_DIR into the tmux session by
    // baking it into the command string. `tmux new-session` would otherwise
    // inherit env from the tmux server's frozen-at-startup environment
    // rather than the caller's shell (#555).
    let claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();

    // Build the claude command (with optional sandbox wrapping)
    let cmd = build_agent_command(
        agent_binary,
        agent_type,
        timeout_cmd,
        timeout.as_secs(),
        model,
        allowed_tools,
        "KICKOFF.md",
        sandbox_command,
        worktree_dir,
        skip_permissions,
        claude_config_dir.as_deref(),
        permission_mode,
        read_backstop_override(crosslink_dir),
    );

    // Write initial status sentinel BEFORE sending the command.
    // This ensures we never have a worktree in limbo with no status.
    std::fs::write(worktree_dir.join(".kickoff-status"), "LAUNCHING\n")
        .context("Failed to write initial .kickoff-status")?;

    // Send the command to the tmux session
    let output = Command::new("tmux")
        .args(["send-keys", "-t", session_name, &cmd, "Enter"])
        .output()
        .context("Failed to send command to tmux session")?;

    if !output.status.success() {
        // INTENTIONAL: status file write is best-effort — used for monitoring, not control flow
        let _ = std::fs::write(worktree_dir.join(".kickoff-status"), "FAILED\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to send keys to tmux: {}", stderr.trim());
    }

    // INTENTIONAL: status file write is best-effort — used for monitoring, not control flow
    let _ = std::fs::write(worktree_dir.join(".kickoff-status"), "RUNNING\n");

    // Spawn watchdog sidecar to nudge idle agents
    let watchdog_cfg = read_watchdog_config(crosslink_dir);
    if watchdog_cfg.enabled {
        if let Err(e) = spawn_watchdog(session_name, worktree_dir, &watchdog_cfg) {
            tracing::warn!("failed to spawn watchdog: {}", e);
        }
    }

    Ok(())
}

/// Launch the agent in a Docker or Podman container.
///
/// `protected_doc_rel`, when `Some`, is the worktree-relative path of the design
/// document passed via `--doc`. It is overlay-bind-mounted read-only on top of
/// the writable workspace mount so the agent physically cannot edit the
/// canonical design input. See GH#580.
///
/// `host_repo_root` is the host path to the main repo (the worktree's parent
/// repo). The main repo's `.git/` directory is bind-mounted at its host
/// absolute path inside the container so the worktree's `.git` file -- which
/// contains an absolute `gitdir: <host>/.git/worktrees/<branch>/` reference
/// -- resolves and git operations inside the container work. See GH#584.
#[allow(clippy::too_many_arguments)]
pub(super) fn launch_container(
    runtime: &ContainerMode,
    agent_binary: &str,
    agent_type: &str,
    worktree_dir: &Path,
    host_repo_root: &Path,
    image: &str,
    agent_id: &str,
    model: &str,
    allowed_tools: &str,
    timeout: Duration,
    protected_doc_rel: Option<&Path>,
) -> Result<String> {
    let runtime_cmd = match runtime {
        ContainerMode::Docker => "docker",
        ContainerMode::Podman => "podman",
        ContainerMode::None => unreachable!(),
    };

    // Check runtime is available
    if !command_available(runtime_cmd) {
        bail!("{runtime_cmd} is not installed. Install it or use --container none for local mode.");
    }

    let timeout_secs = timeout.as_secs();
    // ASES #192: inside the container the timeout wrapper must never kill a
    // healthy agent — use the generous backstop, not the guide value.
    let backstop_secs = timeout_backstop_secs(timeout_secs);
    let container_name = format!("crosslink-agent-{agent_id}");

    // Get host UID/GID for remapping (skip on Windows — Docker Desktop handles user mapping)
    let uid_gid = if cfg!(target_os = "windows") {
        None
    } else {
        let uid = Command::new("id").arg("-u").output().map_or_else(
            |_| "1000".to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        );
        let gid = Command::new("id").arg("-g").output().map_or_else(
            |_| "1000".to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        );
        Some((uid, gid))
    };

    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        container_name,
        // `--stop-timeout` does NOT self-trigger: it only sets the SIGTERM
        // grace period (seconds) that a later user-invoked `docker stop`
        // will wait before SIGKILL. The container runs until it is stopped
        // explicitly (`kickoff stop` / `container stop`) — no kill happens
        // at this value. The actual destroyer guard for a wedged agent is
        // the `timeout {backstop_secs}s` wrapper inside the container
        // (ASES #192). This value is the guide duration, kept for
        // compatibility with `docker stop` semantics.
        "--stop-timeout".to_string(),
        format!("{}", timeout_secs),
        // Mount the worktree as workspace
        "-v".to_string(),
        format!("{}:/workspaces/repo", worktree_dir.to_string_lossy()),
        // Environment
        "-e".to_string(),
        format!("AGENT_ID={}", agent_id),
    ];

    // Mount claude credentials read-only and forward Anthropic auth env vars
    // only when the configured agent is claude. Other agents have no use for
    // `~/.claude` auth or the Anthropic/Claude OAuth tokens.
    if agent_binary == "claude" {
        // Resolve host auth path for credential mounting
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let host_auth = format!("{home}/.claude");
        args.push("-v".to_string());
        args.push(format!("{host_auth}:/host-auth:ro"));
    }

    // Bind-mount the main repo's `.git/` at its host absolute path. The
    // worktree's `.git` is a single file containing an absolute
    // `gitdir: <host>/.git/worktrees/<branch>/` pointer; without this mount
    // that pointer dangles inside the container and every git operation
    // (status, diff, commit, sync) fails. We mount rw because the per-
    // worktree subdir under `.git/worktrees/<branch>/` legitimately needs
    // writes (HEAD, index, refs) for the agent to commit. Hook policy still
    // blocks the genuinely destructive ops (`push --force`, `reset --hard`,
    // etc.) regardless of mount mode. See GH#584.
    let host_git_dir = host_repo_root.join(".git");
    if host_git_dir.exists() {
        let git_path = host_git_dir.to_string_lossy();
        args.push("-v".to_string());
        args.push(format!("{git_path}:{git_path}:rw"));
    }

    // Pass UID/GID to container for user remapping (non-Windows only)
    if let Some((uid, gid)) = &uid_gid {
        args.extend([
            "-e".to_string(),
            format!("HOST_UID={uid}"),
            "-e".to_string(),
            format!("HOST_GID={gid}"),
        ]);
    }

    // Forward Claude auth env vars from the host when set. Using the
    // `-e NAME` form (no value) tells the runtime to pull the value from
    // the parent process env, so tokens don't appear in `ps`. macOS hosts
    // — where the Keychain holds the OAuth credential rather than
    // `~/.claude/.credentials.json` — rely on this passthrough. See GH#580.
    // Only relevant when the configured agent is claude.
    if agent_binary == "claude" {
        for var in ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
            if std::env::var(var).is_ok_and(|v| !v.is_empty()) {
                args.push("-e".to_string());
                args.push(var.to_string());
            }
        }
    }

    // Overlay-bind the design doc read-only so the agent cannot rewrite the
    // canonical `--doc` input. Mounting a single file on top of a writable
    // parent mount is supported by both docker and podman. See GH#580.
    if let Some(rel) = protected_doc_rel {
        let host_doc = worktree_dir.join(rel);
        if host_doc.is_file() {
            let container_path = format!("/workspaces/repo/{}", rel.display());
            args.push("-v".to_string());
            args.push(format!(
                "{}:{}:ro",
                host_doc.to_string_lossy(),
                container_path
            ));
        }
    }

    // Image and command
    args.push(image.to_string());
    args.push("bash".to_string());
    args.push("-c".to_string());
    if agent_binary == "claude" {
        args.push(format!(
            "cd /workspaces/repo && timeout {backstop_secs}s {agent_binary} --model {model} --agent {agent_type} --allowedTools '{allowed_tools}' -- \"$(cat KICKOFF.md)\""
        ));
    } else {
        // Non-Claude agents: pipe the prompt via stdin
        args.push(format!(
            "cd /workspaces/repo && timeout {backstop_secs}s {agent_binary} < KICKOFF.md"
        ));
    }

    let output = Command::new(runtime_cmd)
        .args(&args)
        .output()
        .with_context(|| format!("Failed to launch {runtime_cmd} container"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(format_container_launch_error(runtime_cmd, image, &stderr));
    }

    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(container_id)
}

/// URL of the published GHCR package — surfaced in the launch-failure hint so
/// users can confirm whether the image they're requesting actually exists.
const AGENT_IMAGE_PACKAGE_URL: &str =
    "https://github.com/forecast-bio/crosslink/pkgs/container/crosslink-agent";

/// Format the error message emitted when `docker run` / `podman run` fails.
///
/// Detects pull-failure substrings in the runtime's stderr and appends a
/// hint pointing at `just build-image` (for local builds) and the GHCR
/// package page (to confirm what's actually published). For other failure
/// modes (e.g. invalid mount, OOM), the original stderr is returned without
/// the hint to avoid misdirection.
fn format_container_launch_error(runtime_cmd: &str, image: &str, stderr: &str) -> String {
    let trimmed = stderr.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let pull_failure = ["not found", "denied", "manifest unknown", "no such image"]
        .iter()
        .any(|needle| lowered.contains(needle));

    if pull_failure {
        format!(
            "{runtime_cmd} container launch failed: {trimmed}\n\n\
             Hint: the image `{image}` could not be pulled. Either:\n  \
               * Build it locally:  just build-image       (tags as :local)\n  \
               * Or pick a published tag from {AGENT_IMAGE_PACKAGE_URL}\n  \
                 and pass it via `--image ghcr.io/dollspace-gay/crosslink-agent:<tag>`."
        )
    } else {
        format!("{runtime_cmd} container launch failed: {trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_failure_not_found_yields_hint() {
        let stderr = "Unable to find image 'ghcr.io/dollspace-gay/crosslink-agent:latest' locally\nError response from daemon: manifest unknown";
        let msg = format_container_launch_error(
            "docker",
            "ghcr.io/dollspace-gay/crosslink-agent:latest",
            stderr,
        );
        assert!(msg.contains("docker container launch failed"));
        assert!(msg.contains("Hint:"));
        assert!(msg.contains("just build-image"));
        assert!(msg.contains(AGENT_IMAGE_PACKAGE_URL));
        assert!(msg.contains("ghcr.io/dollspace-gay/crosslink-agent:latest"));
    }

    #[test]
    fn pull_failure_denied_yields_hint() {
        let stderr = "Error response from daemon: pull access denied for some/image, repository does not exist or may require 'docker login'";
        let msg = format_container_launch_error(
            "podman",
            "ghcr.io/dollspace-gay/crosslink-agent:nightly",
            stderr,
        );
        assert!(msg.contains("podman container launch failed"));
        assert!(msg.contains("Hint:"));
        assert!(msg.contains("just build-image"));
    }

    #[test]
    fn pull_failure_no_such_image_yields_hint() {
        let stderr = "Error: No such image: ghcr.io/dollspace-gay/crosslink-agent:does-not-exist";
        let msg = format_container_launch_error(
            "docker",
            "ghcr.io/dollspace-gay/crosslink-agent:does-not-exist",
            stderr,
        );
        assert!(msg.contains("Hint:"));
    }

    #[test]
    fn non_pull_failure_omits_hint() {
        let stderr = "docker: Error response from daemon: invalid mount config for type \"bind\": bind source path does not exist";
        let msg = format_container_launch_error(
            "docker",
            "ghcr.io/dollspace-gay/crosslink-agent:latest",
            stderr,
        );
        assert!(msg.contains("docker container launch failed"));
        assert!(
            !msg.contains("Hint:"),
            "non-pull errors must not get the build-image hint (would misdirect): {msg}"
        );
        assert!(!msg.contains("just build-image"));
    }

    #[test]
    fn pull_failure_is_case_insensitive() {
        let stderr = "Error: NOT FOUND";
        let msg = format_container_launch_error("docker", "image:tag", stderr);
        assert!(msg.contains("Hint:"));
    }
}
