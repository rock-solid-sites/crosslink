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

- [x] source/build and Linux asset requirement resolved (see checkpoint 1)
- [ ] reviewed dependencies installed
- [ ] dedicated app-server/frontend service definitions prepared
- [ ] local health and identity/state checks pass
- [ ] Tailscale-only exposure configured without public 8214
- [ ] restart/rollback validation complete

# Checkpoint 1 — 2026-09-05: source verified, Linux asset gate resolved

## Observed facts

- Pinned upstream checkout at `/tmp/opencode/codex-web-upstream` verified:
  `git rev-parse HEAD` = `8cc728dc11a5745ef8afa0c8d60fb6fcb064b77e`,
  `git status --short` clean. Existing temp data untouched; no deletion.
- Linux asset question resolved FROM UPSTREAM (no guessing):
  `scripts/prepare` hardcodes only a darwin-arm64 ChatGPT zip
  (`APP_VERSION="26.707.30751"`, `persistent.oaistatic.com/codex-app-prod/
  ChatGPT-darwin-arm64-...zip`), and `default.nix` builds for
  `aarch64-linux` + `x86_64-linux` using that SAME darwin zip
  (`codexZip` single definition, sha256 pinned). Upstream therefore
  intends the darwin zip on Linux: the asar is platform-independent JS,
  `prepare_asar` strips `node_modules/better-sqlite3` (rebuilt natively),
  and `unzip` extracts the `ChatGPT.app/.../app.asar` path on any host OS.
  README confirms "hostable on macOS, Linux". No separate Linux asset
  exists upstream; using the pinned darwin URL verbatim is the
  upstream-prescribed path, not a guess.
- VPS toolchain (host `IT3812`, user `claude-code`, x86_64): node v22.22.3,
  npm 10.9.8, python3 3.10.12, gcc/g++/make, unzip, patch all present.
  `websocat` NOT installed (required by `scripts/codex_remote_proxy`).
  `asar`/`prettier`/`sharp` CLIs absent but arrive via `npm install`
  (`@electron/asar`, `prettier`, `sharp-cli` are package deps).
- No prior codex-web install (`~/.local/share/codex-web`, `~/srv/codex-web`
  both absent). systemd user manager active, linger enabled
  (`Linger=yes`); running user units: chat-ui, gemini-proxy, mcp-bash,
  dbus. Port 8214 state recheck pending at service-validation boundary.

## Changed paths

- This handoff only. No deployment paths touched yet.

## Commands run (read-only)

- `git -C /tmp/opencode/codex-web-upstream rev-parse HEAD / status / log`
- `uname -m`, `command -v` toolchain probe, `systemctl --user list-units`,
  `loginctl show-user claude-code | grep -i linger`

## Validation

- Pinned commit matches audit expectation exactly. No destructive command
  used (`rm -rf`, `reset`, `checkout`, `clean` untouched).

## Rollback

- Nothing to roll back (no system change).

## Next step

- Install `codex-web` from the pinned source to a unique timestamped path
  OUTSIDE repos (e.g. `~/.local/share/codex-web-20260905-<ts>` via
  `git clone` of the verified checkout + `npm ci` + prepare/build), then
  install `websocat` from reviewed source. Checkpoint again before
  writing any unit files.

# Resume note

Start from audit handoff `issue-800-codex-web-audit.md` and commit `54b7a055`.
Do not adopt or stop the existing code-mode-owned app-server. Stop at any
unresolved source, privilege, identity, or exposure gate and record the exact
operator action required.
