// E-ana tablet — design command: launch foreground agent session for design doc authoring
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// Run `crosslink design` — launch a foreground agent session with the /design skill prompt.
///
/// If called from inside an agent session (detected via `AGENT_SESSION` env var), prints
/// a message directing the user to `/design` and exits with code 1.
pub fn run(
    description: Option<&str>,
    issue: Option<i64>,
    gh_issue: Option<i64>,
    continue_slug: Option<&str>,
) -> Result<()> {
    // 1. Agent session detection
    if std::env::var("AGENT_SESSION").is_ok() {
        eprintln!("Already inside an agent session — use /design instead.");
        std::process::exit(1);
    }

    // 2. Read agent binary from hook-config.json (default: claude)
    let agent_binary = read_agent_binary();

    // 2. Verify agent CLI is on PATH
    let binary_available = Command::new("which")
        .arg(&agent_binary)
        .output()
        .is_ok_and(|o| o.status.success());

    if !binary_available {
        bail!(
            "`{}` CLI not found. Install it or configure a different agent via hook-config.json's `agent.binary` field.",
            agent_binary
        );
    }

    // 3. Build the prompt arguments line
    let mut args_parts = Vec::new();

    if let Some(slug) = continue_slug {
        args_parts.push(format!("--continue {slug}"));
    } else if let Some(desc) = description {
        args_parts.push(format!("\"{desc}\""));
    }

    if let Some(id) = issue {
        args_parts.push(format!("--issue {id}"));
    }
    if let Some(id) = gh_issue {
        args_parts.push(format!("--gh-issue {id}"));
    }

    let arguments = args_parts.join(" ");

    // 4. Read the /design skill template for the configured agent
    // Only the claude design doc exists currently; other agents fall back to it
    let skill_prompt = include_str!("../../resources/claude/commands/design.md");

    // Strip the YAML frontmatter (everything between first --- and second ---)
    let prompt_body = strip_frontmatter(skill_prompt);

    // 5. Build the full prompt with arguments substituted
    let full_prompt = if arguments.is_empty() {
        prompt_body.to_string()
    } else {
        format!("ARGUMENTS: {arguments}\n\n{prompt_body}")
    };

    // 6. Launch foreground agent session.
    // The agent CLI accepts the initial prompt as a positional argument;
    // there is no `--prompt` flag.
    let status = Command::new(&agent_binary)
        .arg(&full_prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to launch agent session")?;

    if !status.success() {
        let code = status.code().unwrap_or(1);
        std::process::exit(code);
    }

    Ok(())
}

/// Read the agent binary from hook-config.json's `agent.binary` (default: "claude")
fn read_agent_binary() -> String {
    let config_path = Path::new(".crosslink").join("hook-config.json");
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap_or(serde_json::Value::Null);
    parsed
        .get("agent")
        .and_then(|a| a.get("binary"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "claude".to_string())
}

/// Strip YAML frontmatter (---\n...\n---) from the beginning of a markdown document.
fn strip_frontmatter(content: &str) -> &str {
    if !content.starts_with("---") {
        return content;
    }

    // Find the closing --- (skip the opening one)
    content[3..].find("\n---").map_or(content, |end| {
        let after_frontmatter = &content[3 + end + 4..];
        after_frontmatter.trim_start_matches('\n')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_frontmatter_with_frontmatter() {
        let input = "---\nallowed-tools: Read\ndescription: test\n---\n\n## Context\nBody here";
        let result = strip_frontmatter(input);
        assert!(result.starts_with("## Context"));
    }

    #[test]
    fn test_strip_frontmatter_without_frontmatter() {
        let input = "## Context\nBody here";
        let result = strip_frontmatter(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_frontmatter_empty() {
        let result = strip_frontmatter("");
        assert_eq!(result, "");
    }
}
