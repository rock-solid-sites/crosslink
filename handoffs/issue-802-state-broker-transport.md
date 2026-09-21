---
issue: 802
title: State broker transport adapter (Crosslink-side)
status: implemented — pending independent review verdict
model: claude (session author; OpenCode agent)
provider: opencode
reasoning_effort: default
branch: feature/pp3g-state-broker-adapter
worktree: .worktrees/pp3g-state-broker-adapter
baseline: d323f020dd1a592113d71ebdb1facefb4b426a18
---

# Scope

Smallest clean Crosslink-side adapter for the deployed
`crosslink-state-broker`: read durable project state/head, hydrate state blobs
into disposable local projections, submit expected-head/CAS mutations with
typed `stale_state` handling, verify by read-back. No redesign, no migration,
no GitHub credentials, no live broker mutations, default local/direct behavior
preserved.

# Deliverables

- `crosslink/src/state_broker/` — `StateBrokerClient`, `ProjectStateTransport`,
  typed errors, config/selection, mock broker, validators.
- `crosslink/src/commands/state_broker.rs` — read-only `crosslink state-broker
  status` (+ `--json`).
- `crosslink/tests/state_broker_contract.rs` — loopback stub HTTP contract
  tests; `crosslink/tests/state_broker_live.rs` — ignored read-only live probe.
- `.design/state-broker-transport.md` — integration point, assumptions, live
  test steps.
- `CHANGELOG.md`, `docs_src/reference/commands.qmd`.

# Verification evidence

- `cargo test --lib state_broker`: 41 passed.
- `cargo test --bin crosslink state_broker`: 42 passed.
- `cargo test --test state_broker_contract`: 8 passed (real HTTP over
  loopback; no live broker dependency).
- `cargo test --test state_broker_live`: ignored by default.
- `cargo test --test cli_integration`: 199 passed.
- Full bin suite: see `handoffs/802-review-hy3.md` for the final run result
  (one discovered test was the audit-guarded v2 hydration inventory guard,
  resolved by moving the adapter test to the v3 checkpoint path).
- `cargo clippy --lib`: 0 warnings from `state_broker`.
- CLI smoke: local status; broker-selected-without-env hard error;
  unreachable-loopback typed transport error with no token exposure.
- Repo-wide `cargo fmt --all` deliberately not applied (baseline not
  fmt-clean); new/touched files are rustfmt-clean.

# Delegated review (per-launch operator approval)

- Catalog refreshed `2026-09-21` via `opencode models --verbose --refresh`.
- Approved model: `opencode-go/hy3` (OpenCode Go), effort default.
- Cost comparison (Go pricing table fetched 2026-09-21):
  `hy3` $0.14 in / $0.58 out, cache read $0.035, monthly $60, 0-day retention;
  `mimo-v2.5-pro` $0.435 / $0.87, $15 monthly; `kimi-k2.7-code` $0.95 / $4.00,
  $60 monthly; `nemotron-verifier` free ($0).
- Operator approval: verbal selection "Hy3" via the question tool.
- Deliverable: bounded read-only adversarial review of the module, tests, and
  design doc.
- Result: 1 major + 2 minor + 2 nit findings, all triaged and fixed; full
  report and dispositions in `handoffs/802-review-hy3.md`.
- Discovered separately: pre-existing `agents-hygiene` test flake (shared
  `/tmp/AGENTS.md`), filed as issue #803 (not fixed here).

# Known findings (own review, fixed)

- Explicit `retryable: false` from the broker was overridden by the code
  default for `upstream_error` (read-back mismatches appeared retryable).
  Fixed in `c29fea112` with a regression test through the HTTP client.

# Resume note

Run `cargo test --test cli_integration` and the bin suite in this worktree if
not already green; then reconcile the Hy3 review findings, update
`.design/state-broker-transport.md` if findings change any claim, and post the
final `[PROGRESS]`/handoff comment on issue #802. The next production step is
the hub ref→state-tree mapping decision documented in
`.design/state-broker-transport.md` §4; do not wire `SyncManager` to the broker
before that decision.
