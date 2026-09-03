//! Shared AGENTS hygiene bridge (temporary).
//!
//! Minimal check/sync for the canonical shared policy (`ASES/AGENTS.md`)
//! using existing Crosslink sync/init/session paths. Temporary scaffolding
//! for the ASES tracker issue "Implement shared AGENTS hygiene bridge"
//! (recon: ASES #551); Crosslink remains authoritative for task identity,
//! state, evidence, retries, handoff association, and closure.
//!
//! Design constraints (do not expand without a new approved scope):
//! - Record minimal hash/version metadata in
//!   `.crosslink/agents-hygiene.json`; copy only the canonical shared
//!   `AGENTS.md` into the repository root and never read, write, or
//!   incorporate `AGENTS.repo.md`.
//! - Substantive delegation requires an active corresponding Crosslink
//!   issue via the existing issue/session/kickoff mechanisms.
//! - Worker recovery keeps the existing carriers (issue comments, session
//!   handoff notes, `.kickoff-status`); this module only formats/parses
//!   the existing `[PROGRESS]` stub shape, it does not redesign stubs.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::db::Database;
use crate::models::IssueStatus;

/// State file under `.crosslink/` holding the last-synced canonical hash.
pub const STATE_FILENAME: &str = "agents-hygiene.json";

/// Repo-local guidance file that sync must never touch or incorporate.
pub const REPO_GUIDANCE_FILENAME: &str = "AGENTS.repo.md";

/// Sentinel written by `crosslink session work` with the active issue id.
const ACTIVE_ISSUE_SENTINEL: &str = ".active-issue";

/// Minimal persisted record of the canonical shared-policy snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct HygieneState {
    /// SHA-256 hex of the canonical `AGENTS.md` content at sync time.
    pub canonical_sha256: String,
    /// SHA-256 hex of the repository-root shared `AGENTS.md` after sync.
    pub shared_sha256: String,
    /// Where the canonical content was read from (as given/resolved).
    pub canonical_source: String,
    /// Sync timestamp (RFC 3339).
    pub recorded_at: String,
    /// Crosslink build version that recorded the snapshot.
    pub recorded_by_version: String,
}

/// Freshness of the local snapshot relative to the canonical file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HygieneStatus {
    /// Snapshot hash matches the canonical file.
    Current { sha256: String },
    /// Canonical file differs from the snapshot.
    Stale {
        expected: String,
        actual: String,
        canonical: PathBuf,
        target: PathBuf,
    },
    /// The canonical file could not be found.
    CanonicalMissing { attempted: PathBuf },
    /// No snapshot recorded yet (canonical readable, hash provided).
    NoRecord { actual: String, canonical: PathBuf },
}

/// Parsed form of the existing `[PROGRESS]` recovery stub carried on issue
/// comments (see ASES #552 progress comments for the live shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressStub {
    pub state: String,
    pub completed: String,
    pub next: String,
    pub blocker: String,
}

/// Compute the SHA-256 hex digest of the given content.
#[must_use]
pub fn sha256_hex_str(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Resolve the canonical shared-policy path.
///
/// Precedence: explicit `--canonical` override, then
/// `hook-config.json` (`agents_hygiene.canonical_path`, falling back to the
/// legacy top-level `agents_hygiene_canonical_path`), then the sibling
/// checkout `../ASES/AGENTS.md` when it exists. Errors name the remediation
/// instead of guessing.
pub fn resolve_canonical_path(
    crosslink_dir: &Path,
    canonical_override: Option<&str>,
) -> Result<PathBuf> {
    if let Some(raw) = canonical_override {
        return Ok(PathBuf::from(raw));
    }
    if let Some(configured) = read_configured_canonical(crosslink_dir) {
        return Ok(PathBuf::from(configured));
    }
    let repo_root = crosslink_dir.parent().unwrap_or(crosslink_dir);
    let sibling = repo_root.join("../ASES/AGENTS.md");
    if sibling.is_file() {
        return Ok(sibling);
    }
    anyhow::bail!(
        "canonical shared policy not found: pass --canonical <path to ASES/AGENTS.md> \
         or set agents_hygiene.canonical_path in .crosslink/hook-config.json"
    )
}

fn read_configured_canonical(crosslink_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(crosslink_dir.join("hook-config.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("agents_hygiene")
        .and_then(|section| section.get("canonical_path"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            value
                .get("agents_hygiene_canonical_path")
                .and_then(|v| v.as_str())
        })
        .map(str::to_string)
}

/// Read the persisted snapshot, if any.
#[must_use]
pub fn read_state(crosslink_dir: &Path) -> Option<HygieneState> {
    let raw = std::fs::read_to_string(crosslink_dir.join(STATE_FILENAME)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_state(crosslink_dir: &Path, state: &HygieneState) -> Result<()> {
    let path = crosslink_dir.join(STATE_FILENAME);
    let tmp_path = crosslink_dir.join(format!("{STATE_FILENAME}.tmp"));
    let mut output =
        serde_json::to_string_pretty(state).context("Failed to serialize hygiene state")?;
    output.push('\n');
    std::fs::write(&tmp_path, &output)
        .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "Failed to rename {} → {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Compare the canonical file against the persisted snapshot (pure read).
pub fn check_status(
    crosslink_dir: &Path,
    canonical_override: Option<&str>,
) -> Result<HygieneStatus> {
    let canonical = resolve_canonical_path(crosslink_dir, canonical_override)?;
    let content = match std::fs::read_to_string(&canonical) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HygieneStatus::CanonicalMissing {
                attempted: canonical,
            });
        }
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context(format!("Failed to read {}", canonical.display()))
            );
        }
    };
    let canonical_sha256 = sha256_hex_str(&content);
    let target = crosslink_dir
        .parent()
        .unwrap_or(crosslink_dir)
        .join("AGENTS.md");
    let target_content = match std::fs::read_to_string(&target) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context(format!("Failed to read {}", target.display()))
            );
        }
    };
    let target_sha256 = sha256_hex_str(&target_content);
    match read_state(crosslink_dir) {
        None => Ok(HygieneStatus::NoRecord {
            actual: canonical_sha256,
            canonical,
        }),
        Some(state)
            if state.canonical_sha256 == canonical_sha256
                && state.shared_sha256 == target_sha256
                && target_content == content =>
        {
            Ok(HygieneStatus::Current {
                sha256: canonical_sha256,
            })
        }
        Some(state) => Ok(HygieneStatus::Stale {
            expected: state.canonical_sha256,
            actual: canonical_sha256,
            canonical,
            target,
        }),
    }
}

/// `crosslink agents-hygiene check`: report freshness, failing loudly when
/// the snapshot is stale or missing so startup wiring cannot silently pass.
pub fn run_check(
    crosslink_dir: &Path,
    canonical_override: Option<&str>,
    quiet: bool,
) -> Result<HygieneStatus> {
    let status = check_status(crosslink_dir, canonical_override)?;
    match &status {
        HygieneStatus::Current { sha256 } => {
            if !quiet {
                println!("agents-hygiene: current (sha256:{sha256})");
            }
            Ok(status)
        }
        HygieneStatus::Stale {
            expected,
            actual,
            canonical,
            target,
        } => {
            println!(
                "agents-hygiene: STALE — shared policy differs from canonical or last sync\n  canonical: {}\n  target:    {}\n  expected canonical sha256:{expected}\n  actual   canonical sha256:{actual}\n  remedy: crosslink agents-hygiene sync",
                canonical.display(),
                target.display()
            );
            anyhow::bail!("shared policy is stale; run `crosslink agents-hygiene sync`")
        }
        HygieneStatus::CanonicalMissing { attempted } => {
            println!(
                "agents-hygiene: canonical policy not found at {}\n  remedy: pass --canonical <path to ASES/AGENTS.md>",
                attempted.display()
            );
            anyhow::bail!("canonical shared policy not found")
        }
        HygieneStatus::NoRecord { actual, canonical } => {
            println!(
                "agents-hygiene: no snapshot recorded for {}\n  canonical sha256:{actual}\n  remedy: crosslink agents-hygiene sync",
                canonical.display()
            );
            anyhow::bail!("no hygiene snapshot recorded; run `crosslink agents-hygiene sync`")
        }
    }
}

/// `crosslink agents-hygiene sync`: install the canonical shared policy and
/// record both source and installed hashes.
///
/// Idempotent: when the snapshot already matches, nothing is rewritten.
/// Never touches `AGENTS.repo.md` (neither reads nor writes it); repo-local
/// guidance stays available exactly as the repo left it.
pub fn run_sync(crosslink_dir: &Path, canonical_override: Option<&str>) -> Result<HygieneStatus> {
    let canonical = resolve_canonical_path(crosslink_dir, canonical_override)?;
    let content = std::fs::read_to_string(&canonical)
        .with_context(|| format!("Failed to read {}", canonical.display()))?;
    let canonical_sha256 = sha256_hex_str(&content);
    let target = crosslink_dir
        .parent()
        .unwrap_or(crosslink_dir)
        .join("AGENTS.md");
    let target_is_canonical = canonical
        .canonicalize()
        .ok()
        .zip(target.canonicalize().ok())
        .is_some_and(|(source, destination)| source == destination);
    let target_content = std::fs::read_to_string(&target).unwrap_or_default();
    let target_sha256 = sha256_hex_str(&target_content);
    if let Some(state) = read_state(crosslink_dir) {
        if state.canonical_sha256 == canonical_sha256
            && state.shared_sha256 == target_sha256
            && (target_is_canonical || target_content == content)
        {
            println!("agents-hygiene: already current (sha256:{canonical_sha256})");
            return Ok(HygieneStatus::Current {
                sha256: canonical_sha256,
            });
        }
    }
    if !target_is_canonical && target_content != content {
        let tmp = target.with_extension("md.crosslink-tmp");
        std::fs::write(&tmp, &content)
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("Failed to install shared policy at {}", target.display()))?;
    }
    let state = HygieneState {
        canonical_sha256: canonical_sha256.clone(),
        shared_sha256: sha256_hex_str(&content),
        canonical_source: canonical.display().to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
        recorded_by_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    write_state(crosslink_dir, &state)?;
    println!(
        "agents-hygiene: synced {} → {} (sha256:{canonical_sha256})",
        canonical.display(),
        target.display()
    );
    Ok(HygieneStatus::Current {
        sha256: canonical_sha256,
    })
}

/// Best-effort shared-policy refresh during `crosslink init`.
///
/// Installs the canonical shared file when it resolves and reads cleanly; any
/// failure is a silent no-op so init never breaks on hygiene bookkeeping.
/// Never creates or modifies `AGENTS.repo.md`.
pub fn refresh_on_init(repo_path: &Path) {
    let crosslink_dir = repo_path.join(".crosslink");
    if !crosslink_dir.is_dir() {
        return;
    }
    let _ = run_sync(&crosslink_dir, None);
}

/// Gate substantive delegation on an active corresponding Crosslink issue.
///
/// Accepts the explicit `--issue` binding (verified to exist and be open)
/// or falls back to the session's active issue via the existing
/// `.active-issue` sentinel / session state. Closed, archived, and missing
/// issues fail loudly; read-only recon without an issue stays out of scope
/// for this gate by construction (callers only invoke it for delegation).
pub fn require_corresponding_issue(
    db: &Database,
    crosslink_dir: &Path,
    issue_opt: Option<i64>,
) -> Result<i64> {
    if let Some(id) = issue_opt {
        let Some(issue) = db
            .get_issue(id)
            .with_context(|| format!("Failed to look up issue #{id}"))?
        else {
            anyhow::bail!(
                "no corresponding Crosslink issue #{id}: pass --issue <open id> \
                 or set one via `crosslink session work <id>`"
            )
        };
        if issue.status != IssueStatus::Open {
            anyhow::bail!(
                "corresponding Crosslink issue #{id} is {}: substantive delegation \
                 requires an active corresponding Crosslink issue",
                issue.status
            )
        }
        return Ok(id);
    }
    if let Some(id) =
        read_sentinel_issue(crosslink_dir).or_else(|| current_session_issue(db, crosslink_dir))
    {
        let Some(issue) = db
            .get_issue(id)
            .with_context(|| format!("Failed to look up issue #{id}"))?
        else {
            anyhow::bail!(
                "stale active-issue reference #{id}: re-bind with \
                 `crosslink session work <id>` before delegating"
            )
        };
        if issue.status != IssueStatus::Open {
            anyhow::bail!(
                "active Crosslink issue #{id} is {}: substantive delegation \
                 requires an active corresponding Crosslink issue",
                issue.status
            )
        }
        return Ok(id);
    }
    anyhow::bail!(
        "substantive delegation requires an active corresponding Crosslink issue: \
         run `crosslink session work <id>` or pass --issue <open id>"
    )
}

fn read_sentinel_issue(crosslink_dir: &Path) -> Option<i64> {
    std::fs::read_to_string(crosslink_dir.join(ACTIVE_ISSUE_SENTINEL))
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
}

fn current_session_issue(db: &Database, crosslink_dir: &Path) -> Option<i64> {
    let agent_id = crate::identity::AgentConfig::load(crosslink_dir)
        .ok()
        .flatten()
        .map(|config| config.agent_id);
    // `None` falls back to any active session (backward-compat path).
    let session = db
        .get_current_session_for_agent(agent_id.as_deref())
        .ok()??;
    session.active_issue_id
}

/// Format a `[PROGRESS]` recovery stub in the existing carrier shape.
///
/// Preserves the `state=/completed=/next=/blocker=` field order used by the
/// ASES #552 progress comments so existing parsers keep working.
#[must_use]
pub fn format_progress_stub(state: &str, completed: &str, next: &str, blocker: &str) -> String {
    format!("[PROGRESS] state={state} completed={completed} next={next} blocker={blocker}")
}

/// Parse a `[PROGRESS]` stub from free text (e.g. an issue comment body).
/// Values run to the next ` key=` boundary, a `;` trailer separator, or
/// end of input — matching the live carrier shape where trailing prose
/// follows the stub after `;` (e.g. the ASES #552 progress comments).
#[must_use]
pub fn parse_progress_stub(text: &str) -> Option<ProgressStub> {
    let start = text.find("[PROGRESS]")?;
    let mut rest = text[start + "[PROGRESS]".len()..].trim_start();
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let keys = ["state", "completed", "next", "blocker"];
    while !rest.is_empty() {
        let key = keys
            .iter()
            .find(|key| rest.starts_with(*key) && rest[key.len()..].starts_with('='))?;
        rest = &rest[key.len() + 1..];
        rest = rest.trim_start();
        let mut end = rest.len();
        for other in keys {
            if other == *key {
                continue;
            }
            let marker = format!(" {other}=");
            if let Some(pos) = rest.find(&marker) {
                end = end.min(pos);
            }
        }
        // Trailing prose after the stub is separated by `;`.
        if let Some(pos) = rest[..end].find(';') {
            end = pos;
        }
        fields.insert((*key).to_string(), rest[..end].trim_end().to_string());
        rest = rest[end..].trim_start();
        // A `;` remainder is trailing prose after the stub, not more fields.
        if rest.starts_with(';') {
            break;
        }
    }
    Some(ProgressStub {
        state: fields.remove("state")?,
        completed: fields.remove("completed")?,
        next: fields.remove("next")?,
        blocker: fields.remove("blocker")?,
    })
}

/// KICKOFF.md stanza binding the worker to its corresponding issue.
///
/// States the delegation gate, the hygiene check, `AGENTS.repo.md`
/// preservation, and the existing recovery carriers without adding new
/// orchestration semantics.
#[must_use]
pub fn build_kickoff_stanza(issue_id: i64) -> String {
    format!(
        r"
## Shared Policy Hygiene Bridge (temporary)

- Your active corresponding Crosslink issue is #{issue_id}. Substantive
  delegation and commits require that issue to stay open; if it closes,
  stop and re-bind via `crosslink session work <id>` before continuing.
- Canonical shared policy is `ASES/AGENTS.md`. Check freshness with
  `crosslink agents-hygiene check` and refresh the snapshot with
  `crosslink agents-hygiene sync` after pulling latest state.
- `AGENTS.repo.md` (when present) is repo-local guidance: leave it in
  place, keep it readable, and never merge it into shared policy or
  overwrite it from the canonical file.
- Recovery uses the existing carriers only: milestone checkpoint comments
  (`[PROGRESS] state=... completed=... next=... blocker=...` on #{issue_id}),
  session handoff notes, and `.kickoff-status` — then `crosslink sync`.
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_canonical(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn canonical_arg(path: &Path) -> Option<String> {
        Some(path.display().to_string())
    }

    fn setup_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.db")).unwrap();
        (db, dir)
    }

    // ── current / stale / idempotent sync ─────────────────────────────

    #[test]
    fn test_sync_then_check_is_current() {
        let crosslink = tempfile::tempdir().unwrap();
        let canonical = write_canonical(&crosslink, "# policy v1\n");
        let arg = canonical_arg(&canonical);

        let status = run_sync(crosslink.path(), arg.clone().as_deref()).unwrap();
        assert!(matches!(status, HygieneStatus::Current { .. }));

        let check = run_check(crosslink.path(), arg.as_deref(), true).unwrap();
        assert!(matches!(check, HygieneStatus::Current { .. }));
    }

    #[test]
    fn test_stale_snapshot_fails_check_until_resync() {
        let crosslink = tempfile::tempdir().unwrap();
        let canonical = write_canonical(&crosslink, "# policy v1\n");
        let arg = canonical_arg(&canonical);
        run_sync(crosslink.path(), arg.clone().as_deref()).unwrap();

        std::fs::write(&canonical, "# policy v2\n").unwrap();
        assert!(run_check(crosslink.path(), arg.clone().as_deref(), true).is_err());

        run_sync(crosslink.path(), arg.clone().as_deref()).unwrap();
        assert!(run_check(crosslink.path(), arg.as_deref(), true).is_ok());
    }

    #[test]
    fn test_sync_is_idempotent() {
        let crosslink = tempfile::tempdir().unwrap();
        let canonical = write_canonical(&crosslink, "# policy\n");
        let arg = canonical_arg(&canonical);

        run_sync(crosslink.path(), arg.clone().as_deref()).unwrap();
        let before = std::fs::read_to_string(crosslink.path().join(STATE_FILENAME)).unwrap();
        run_sync(crosslink.path(), arg.as_deref()).unwrap();
        let after = std::fs::read_to_string(crosslink.path().join(STATE_FILENAME)).unwrap();
        // Idempotent resync rewrites identical content except the timestamp.
        let mut before_value: serde_json::Value = serde_json::from_str(&before).unwrap();
        let mut after_value: serde_json::Value = serde_json::from_str(&after).unwrap();
        before_value.as_object_mut().unwrap().remove("recorded_at");
        after_value.as_object_mut().unwrap().remove("recorded_at");
        assert_eq!(before_value, after_value);
    }

    #[test]
    fn test_check_without_snapshot_fails_loudly() {
        let crosslink = tempfile::tempdir().unwrap();
        let canonical = write_canonical(&crosslink, "# policy\n");
        assert!(run_check(crosslink.path(), canonical_arg(&canonical).as_deref(), true).is_err());
    }

    #[test]
    fn test_check_with_missing_canonical_fails_loudly() {
        let crosslink = tempfile::tempdir().unwrap();
        let missing = crosslink.path().join("does-not-exist.md");
        assert!(run_check(crosslink.path(), canonical_arg(&missing).as_deref(), true).is_err());
    }

    // ── AGENTS.repo.md preservation / availability ────────────────────

    #[test]
    fn test_sync_preserves_repo_guidance() {
        let project = tempfile::tempdir().unwrap();
        let crosslink_dir = project.path().join(".crosslink");
        std::fs::create_dir(&crosslink_dir).unwrap();
        let canonical_dir = tempfile::tempdir().unwrap();
        let canonical = write_canonical(&canonical_dir, "# shared\n");
        let repo_guidance = project.path().join(REPO_GUIDANCE_FILENAME);
        std::fs::write(&repo_guidance, "# repo-local\n").unwrap();

        run_sync(&crosslink_dir, canonical_arg(&canonical).as_deref()).unwrap();

        assert_eq!(
            std::fs::read_to_string(project.path().join("AGENTS.md")).unwrap(),
            "# shared\n"
        );
        assert_eq!(
            std::fs::read_to_string(&repo_guidance).unwrap(),
            "# repo-local\n"
        );
    }

    #[test]
    fn test_stale_target_is_reinstalled_from_canonical() {
        let project = tempfile::tempdir().unwrap();
        let crosslink_dir = project.path().join(".crosslink");
        std::fs::create_dir(&crosslink_dir).unwrap();
        let canonical_dir = tempfile::tempdir().unwrap();
        let canonical = write_canonical(&canonical_dir, "# shared v1\n");
        std::fs::write(project.path().join("AGENTS.md"), "# local stale\n").unwrap();

        run_sync(&crosslink_dir, canonical_arg(&canonical).as_deref()).unwrap();
        assert_eq!(
            std::fs::read_to_string(project.path().join("AGENTS.md")).unwrap(),
            "# shared v1\n"
        );
        std::fs::write(&canonical, "# shared v2\n").unwrap();
        assert!(run_check(&crosslink_dir, canonical_arg(&canonical).as_deref(), true).is_err());
        run_sync(&crosslink_dir, canonical_arg(&canonical).as_deref()).unwrap();
        assert_eq!(
            std::fs::read_to_string(project.path().join("AGENTS.md")).unwrap(),
            "# shared v2\n"
        );
        assert!(run_check(&crosslink_dir, canonical_arg(&canonical).as_deref(), true).is_ok());
    }

    #[test]
    fn test_sync_without_repo_guidance_does_not_create_it() {
        let crosslink = tempfile::tempdir().unwrap();
        let canonical = write_canonical(&crosslink, "# shared\n");

        run_sync(crosslink.path(), canonical_arg(&canonical).as_deref()).unwrap();

        assert!(!crosslink.path().join(REPO_GUIDANCE_FILENAME).exists());
    }

    // ── corresponding-issue delegation gate ───────────────────────────

    #[test]
    fn test_gate_accepts_explicit_open_issue() {
        let (db, _dir) = setup_db();
        let id = db.create_issue("work", None, "medium").unwrap();
        let crosslink = tempfile::tempdir().unwrap();
        assert_eq!(
            require_corresponding_issue(&db, crosslink.path(), Some(id)).unwrap(),
            id
        );
    }

    #[test]
    fn test_gate_rejects_missing_issue() {
        let (db, _dir) = setup_db();
        let crosslink = tempfile::tempdir().unwrap();
        assert!(require_corresponding_issue(&db, crosslink.path(), Some(99999)).is_err());
    }

    #[test]
    fn test_gate_rejects_closed_issue() {
        let (db, _dir) = setup_db();
        let id = db.create_issue("work", None, "medium").unwrap();
        db.close_issue(id).unwrap();
        let crosslink = tempfile::tempdir().unwrap();
        let err = require_corresponding_issue(&db, crosslink.path(), Some(id)).unwrap_err();
        assert!(err.to_string().contains("requires an active corresponding"));
    }

    #[test]
    fn test_gate_falls_back_to_session_active_issue() {
        let (db, _dir) = setup_db();
        let id = db.create_issue("work", None, "medium").unwrap();
        let crosslink = tempfile::tempdir().unwrap();
        db.start_session().unwrap();
        let session = db.get_current_session().unwrap().unwrap();
        db.set_session_issue(session.id, id).unwrap();
        std::fs::write(crosslink.path().join(ACTIVE_ISSUE_SENTINEL), id.to_string()).unwrap();
        assert_eq!(
            require_corresponding_issue(&db, crosslink.path(), None).unwrap(),
            id
        );
    }

    #[test]
    fn test_gate_fails_without_any_issue() {
        let (db, _dir) = setup_db();
        let crosslink = tempfile::tempdir().unwrap();
        db.start_session().unwrap();
        let err = require_corresponding_issue(&db, crosslink.path(), None).unwrap_err();
        assert!(err.to_string().contains("requires an active corresponding"));
    }

    // ── recovery compatibility ────────────────────────────────────────

    #[test]
    fn test_progress_stub_round_trip() {
        let stub = format_progress_stub("syncing", "hash-recorded", "wire-hooks", "none");
        let parsed = parse_progress_stub(&stub).unwrap();
        assert_eq!(
            parsed,
            ProgressStub {
                state: "syncing".to_string(),
                completed: "hash-recorded".to_string(),
                next: "wire-hooks".to_string(),
                blocker: "none".to_string(),
            }
        );
    }

    #[test]
    fn test_progress_stub_parses_legacy_issue_comment_shape() {
        let legacy = "[PROGRESS] state=implementation-stub-created \
             completed=recon-basis-and-scope-recorded next=dispatch-builder blocker=none; \
             recovery stub saved";
        let parsed = parse_progress_stub(legacy).unwrap();
        assert_eq!(parsed.state, "implementation-stub-created");
        assert_eq!(parsed.completed, "recon-basis-and-scope-recorded");
        assert_eq!(parsed.next, "dispatch-builder");
        assert_eq!(parsed.blocker, "none");
    }

    #[test]
    fn test_kickoff_stanza_names_issue_and_carriers() {
        let stanza = build_kickoff_stanza(552);
        assert!(stanza.contains("#552"));
        assert!(stanza.contains("agents-hygiene check"));
        assert!(stanza.contains("AGENTS.repo.md"));
        assert!(stanza.contains("[PROGRESS]"));
    }
}
