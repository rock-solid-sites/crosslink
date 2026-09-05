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

- [x] upstream source/install/proxy requirements
- [x] VPS Codex/app-server/project/service/network facts
- [x] firewall/Tailscale gate
- [x] blockers and implementation-ready next stage

# Audit findings — 2026-09-05 (resumed worker, commit 72014e35)

All inspection was read-only. No service, firewall, Tailscale, Codex, or
repository state was changed. No secrets, tokens, conversation contents, or
peer addresses are recorded here.

## 1. Upstream source (pinned commit 8cc728dc11a5745ef8afa0c8d60fb6fcb064b77e)

Checkout at `/tmp/opencode/codex-web-upstream` verified at the pinned commit
(`Revert "add browser notifs (#28)"`). Path left intact; no cleanup performed.

- Install: `npx --yes github:0xcaff/codex-web`, or `nix run
  github:0xcaff/codex-web`. npm `prepare` script runs `./scripts/prepare`
  (downloads ChatGPT desktop zip) + `build:browser` (vite) + `build:server`
  (`tsc` in `src/server`).
- Server entrypoint: `src/server/main.js` (package `bin`). Flags: `--host`
  (default `127.0.0.1`), `--port` (default `8214`), `-h/--help`. Loopback-only
  by default — matches the Tailscale-only requirement with no extra flags.
- `codex_remote_proxy` (`scripts/codex_remote_proxy`, also a nix
  `writeShellApplication`): bash stdio-to-Unix-socket bridge. Supports ONLY
  `app-server` mode (exit 64 otherwise); strips `-c <val>` pairs; requires
  env `CODEX_UNIX_SOCKET`; optional `CODEX_BUFFER_SIZE` (default 100 MiB);
  execs `websocat -E -t -B <size> - "ws-c:unix:<socket>"`. Runtime deps:
  bash, coreutils, websocat.
- Unix-socket variables: `CODEX_UNIX_SOCKET` (proxy target, required),
  `CODEX_CLI_PATH` (codex binary or proxy path; `npm run server` defaults it
  to `$(which codex)`). No `CODEX_HOME` references anywhere in source.
- Codex resolution: `codex` from `PATH`, or `CODEX_CLI_PATH` override. Sign-in
  prerequisite: `codex login --device-auth` on the host before starting.
- Subagents: supported per README ("working today" list, with inline images,
  editor sidepanel, transcription). Not yet wired: browser panel, Linux
  computer use, terminal support, git worker integration.
- Caveats: (a) `scripts/prepare` hardcodes a **darwin-arm64** ChatGPT zip URL
  (`APP_VERSION="26.707.30751"`); `default.nix` `codexZip` uses the same
  darwin URL with a pinned sha256. A Linux desktop asset URL must be confirmed
  from upstream before any Linux install — do not guess it. (b) 16 patches
  under `patches/` applied at install time via `prepare_asar` (needs unzip,
  patch, prettier, sharp-cli). (c) `better-sqlite3` needs a native build
  (python3 + node headers). (d) No authn/authz in codex-web itself — README
  directs operators to Tailscale/Wireguard/SSH + gateway. Treat anyone who can
  reach the port as able to run codex as the server user.
- npm deps (reviewed list, `package.json`): fastify 5, @fastify/multipart,
  @fastify/static, ws 8, glob 13, better-sqlite3 12, react 19, vite 8,
  typescript 6, prettier 3, sharp-cli 5, @electron/asar 4, @tanstack/react-query
  5; devDeps electron 41.2.0, http-server 14.

## 2. VPS facts (this host is the target; user `claude-code`, HOME same)

- Codex: `/home/claude-code/.local/bin/codex`, version `codex-cli 0.152.1`,
  `codex login status` reports logged in via ChatGPT (details redacted).
  `CODEX_HOME` unset (default `~/.codex` in use). `~/.codex/auth.json`
  present (mode 600). Existing state (sessions index, sqlite stores,
  history) present — reuse as-is, no separate account.
- Toolchain: node v22.22.3, npm 10.9.8, git 2.45.2 present. **websocat NOT
  installed** (no binary in PATH, none under `~/.cargo/bin`) — required for
  the proxy mode; must be installed (reviewed source) before Phase 2.
- Managed app-server: pid 2811108 since 2026-09-03, owner `claude-code`:
  `codex -c features.code_mode_host=true app-server --listen unix://`
  (cmdline shows no socket path suffix) with child `codex-code-mode-host`.
  Control socket `~/.codex/app-server-control/app-server-control.sock`
  (mode 600, same owner). A second `codex app-server proxy` process (started
  today) belongs to agent-harness infrastructure, not to the managed
  desktop session. **Gate verdict: do NOT adopt the managed app-server.**
  Its listener/socket layout is owned by the desktop/code-mode integration;
  Phase 2 must start a dedicated long-lived `codex app-server --listen
  unix://<dedicated-sock>` (e.g. under a service-owned dir, NOT /tmp shared
  paths) plus a separate codex-web frontend service, per README advanced
  usage. No second competing codex-web server; port 8214 currently free.
- Projects: `~/projects` holds the existing workspaces; codex `config.toml`
  trusts `/home/claude-code` and `/home/claude-code/projects`. codex-web
  itself must live OUTSIDE project repos (e.g. `~/.local/share/codex-web` or
  `/home/claude-code/srv/codex-web` — path decision deferred to Phase 2).
- Services: systemd user manager in use, lingering enabled. Existing active
  user services: chat-ui, gemini-proxy, mcp-bash (+dbus). Convention:
  unit files in `~/.config/systemd/user/`. Follow it for the two new
  services (app-server + frontend). No user timers.
- Network: nothing on port 8214. Loopback listeners (8081, 3001, 6379,
  3306, 46213, …) plus existing public 0.0.0.0 listeners (80/443/7080/5771/
  8188 — pre-existing, unrelated). Tailscale node `vps` is up on the tailnet
  with one active desktop-class peer; the phone peer is offline (~20 days),
  so phone/browser validation is currently impossible. `tailscale serve`
  has NO config yet — Phase 2 work.

## 3. Firewall/Tailscale gate

- Firewall state could NOT be verified: no passwordless sudo/root in this
  session (`ufw status` refused). codex-web defaults to loopback-only so no
  firewall weakening is needed; pre-Phase-2 must still confirm (operator or
  privileged worker) that no public rule exposes 8214 and that 80/443 stay
  as-is. Recorded as a Phase 2 pre-condition, not a blocker for the audit.
- Tailscale-only exposure path is clear: keep `--host 127.0.0.1`, then
  `tailscale serve` the loopback port. No public port opening authorized.

## 4. Blockers and Phase 2 gates

Phase 2 (deployment) is NOT authorized by this issue ("No deployment changes
in this phase") and additionally gated on:

1. [gate] New issue/operator approval explicitly authorizing installation
   and service creation.
2. [blocker] Linux desktop asset URL for `prepare`/`prepare_asar` — confirm
   from upstream; the pinned source only ships a darwin-arm64 URL.
3. [blocker] websocat installation (reviewed source only).
4. [pre-condition] Firewall read-back by a privileged party (8214 closed
   publicly); Tailscale Serve config; phone-peer availability for any
   phone/browser claim (currently offline — do not claim it).
5. [safety] Unique timestamped install path outside repos; pin commit
   8cc728d; keep rollback (stop/disable new units, remove serve config).

Smallest safe next action: operator opens a Phase 2 deployment issue (or
approves here), then a worker installs websocat + resolves the Linux asset
question before touching services.

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
