---
issue: 801
parent_issue: 800
title: codex-web Phase 2 persistent Tailscale-only deployment
status: in progress
model: opencode-go/muse-spark-1.3-contributor
provider: opencode-go
reasoning_effort: medium
---

# Scope

Implement the approved codex-web deployment from audit commit `54b7a055`.
Use the existing authenticated Codex state, keep the frontend loopback-only,
and expose it only through Tailscale. Do not modify project repositories.

# Recovery contract

Before each meaningful boundary, update this handoff with exact observed
facts, changed paths, service state, validation, rollback, and next step;
commit the checkpoint with a conventional commit; post `[PROGRESS]` to issue
#801; and run `crosslink sync`. Never leave deployment state only in session
output. Preserve rollback paths and do not use destructive cleanup.

# Gates

- [ ] source/build and Linux asset requirement resolved
- [ ] reviewed dependencies installed
- [ ] dedicated app-server/frontend service definitions prepared
- [ ] local health and identity/state checks pass
- [ ] Tailscale-only exposure configured without public 8214
- [ ] restart/rollback validation complete

# Resume note

Start from audit handoff `issue-800-codex-web-audit.md` and commit `54b7a055`.
Do not adopt or stop the existing code-mode-owned app-server. Stop at any
unresolved source, privilege, identity, or exposure gate and record the exact
operator action required.
