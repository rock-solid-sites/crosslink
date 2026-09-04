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

# Resume note

Begin with the upstream repository and current VPS read-only checks. Preserve
secrets and private conversation contents. Update this file at the first
completed milestone before continuing.
