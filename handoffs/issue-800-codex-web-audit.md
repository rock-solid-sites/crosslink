---
issue: 800
title: codex-web VPS deployment source gate and recovery-managed audit
status: in progress
model: opencode-go/muse-spark-1.3-contributor
provider: opencode-go
reasoning_effort: medium
---

# Scope

Read-only audit of the `0xcaff/codex-web` source and the VPS prerequisites for
a persistent, Tailscale-only frontend using the existing authenticated Codex
environment. No deployment changes are authorized in this phase.

# Recovery contract

The worker must checkpoint this handoff after each milestone, post a truthful
`[PROGRESS]` observation on Crosslink issue #800, and run `crosslink sync`.
If interrupted, the next worker resumes from this file, the issue position,
and the git history rather than relying on session output.

# Milestones

- [ ] upstream source/install/proxy requirements
- [ ] VPS Codex/app-server/project/service/network facts
- [ ] firewall/Tailscale gate
- [ ] blockers and implementation-ready next stage

# Partial checkpoint — 2026-09-04

The Crosslink worker launched and posted its plan, then encountered repeated
OpenCode Go `rate_limit_exceeded` errors. It performed an upstream checkout
into `/tmp/opencode/codex-web-upstream` at commit
`8cc728dc11a5745ef8afa0c8d60fb6fcb064b77e` before the session was stopped.
The worker also attempted to remove that temporary path despite the
read-only contract; no application, service, firewall, Tailscale, Codex, or
repository state was intentionally changed. No durable upstream findings or
VPS report were produced, and the issue progress comment was not synced.

## Resume note

Resume by verifying the worktree and issue state, then inspect the pinned
upstream checkout without destructive cleanup. Use a new paid model launch
only after a fresh catalog check. Checkpoint this handoff and sync the issue
after each milestone; preserve secrets and private conversation contents.
