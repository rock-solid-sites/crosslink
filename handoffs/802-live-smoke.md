---
issue: 802
title: Live read-only deployed-broker smoke + hardening gate matrix (ADR-802 follow-up)
status: complete
branch: feature/pp3g-state-broker-adapter
base: cf0902986 (ADR-802 commit)
date: 2026-09-21
scope: |
  Read-only verification only. No broker writes, no commit_cas, no SyncManager
  wiring, no code changes, no branch merges. The ADR's decision is unchanged.
inputs:
  - .design/state-broker-authority-adr.md (ADR-802, committed at cf0902986)
  - /tmp/opencode/secrets/crosslink-state-broker-codex-cloud.env (token never printed)
  - crosslink/tests/state_broker_live.rs (prepared ignored probe)
  - fix/pp3g-802-decision-independent-hardening @ 12de00f65 (read-only inspection)
---

# Live read-only smoke — deployed RSS broker

## 1. What was run

| Step | Command (env from the secrets file; token never echoed) | Result |
|---|---|---|
| Prepared probe | `cargo test --test state_broker_live -- --ignored --nocapture` | 1 passed, 0 writes |
| CLI status | `CROSSLINK_STATE_BACKEND=broker crosslink state-broker status --json` | backend `broker` |
| CLI default | same command without `CROSSLINK_STATE_BACKEND` | backend `local` (no silent switch) |
| Adapter probe | throwaway crate at `/tmp/opencode/adr802-probe` using the real `StateBrokerClient` | reads only, digest checks pass |

## 2. Observed deployed state

- broker host: `https://crosslink-state-broker.rss-tools.workers.dev`
- broker version: `0.1.0`; registry present; baseline matches
- token id: `codex-cloud-codex-build`; scopes `state:read`, `state:write`
- project UUID: `1d440dcf-bcbf-4d1a-987c-d5334568a716`
- state ref: `refs/heads/projects/1d440dcf-bcbf-4d1a-987c-d5334568a716/state`
- **durable head:** `93b1c6a6737231e4a3913eb7d87246b239c2a5e8`
- baseline: expected `94fa0e38ae68c95e13d91226e74e6d4f6f1524dd`, observed the same, `matches: true`
- inventory (5 files, decoded):
  - `experiments/codex-cloud-durability-2026-09-21/first-task.json` (215 B)
  - `experiments/codex-cloud-durability-2026-09-21/second-task.json` (288 B)
  - `probe/2026-09-21-broker-client-probe.json` (68 B)
  - `probe/2026-09-21T02-10-03Z-orchestrator-e2e.json` (135 B)
  - `probe/2026-09-21T02-33-15Z-orchestrator-e2e-2.json` (137 B)
- live head message confirms the broker trailer block:

  ```text
  experiment: codex-cloud-durability-2026-09-21 recovery phase

  Project-UUID: 1d440dcf-bcbf-4d1a-987c-d5334568a716
  Broker: crosslink-state-broker/0.1.0
  Broker-Op: durability-second-20260921T103131-1f64e7b1ae5a
  ```

## 3. Verification checklist

| Item | Evidence | Verdict |
|---|---|---|
| Project UUID binding | env == whoami == state == configured; adapter `identity_mismatch` checks passed | verified |
| Backend selection | selected → `broker`; unselected (broker env present) → `local` | verified |
| Baseline match | observed == documented expected, `matches: true` | verified |
| Current durable head | `93b1c6a6…a5e8`, second durability mutation | verified |
| Inventory decoding | 5 entries deserialized and listed | verified |
| Blob decoding/digests | all 5 read at pinned head; `sha256`/`size`/`blob_sha` match `verify` and the inventory; envelope digest enforced | verified |
| Typed errors | live `not_found` (absent path); live `invalid_input` (unsafe path); no live 401/403/409 attempted | verified (read-side) |
| Token never exposed | probe assertion `token_leaked_in_rendered_output: false`; outputs show token id only; token never printed or committed | verified |
| Disposable projection | 1 file hydrated from the head, digest verified, directory deleted (`projection_deleted: true`) | verified |
| Writes performed | probe/live test/CLI all read-only | 0 |

## 4. ADR-802 assumption comparison

No ADR assumption was contradicted. Confirmations from live evidence:
state-ref format, project-UUID binding, baseline/registry shape, trailer block
format (`Broker-Op:`), head/inventory/blob/verify contract, and read-only
compatibility of the Rust client with the deployment.

Notes (not discrepancies):
- The ADR's size evidence (1.8 MiB Crosslink checkpoint) is about Crosslink's
  own hub, not this broker project; this project is far below the limits, so
  the size rule is untested live but not contradicted.
- ADR §16 conditions still open: trait re-scope (D-hygiene), repo↔project-UUID
  binding, unknown-outcome write blocking, projection-directory disjointness.
- A raw `python-urllib` GET returned 403 while the adapter's `reqwest` client
  succeeded; this is a client/WAF artifact, not a contract discrepancy.

## 5. Hardening gate matrix (ADR §17), branch `fix/pp3g-802-decision-independent-hardening` @ `12de00f65`

Static inspection only (no build/test run in that worktree; it has uncommitted
doc edits).

| # | ADR §17 gate | Status | Evidence / residual |
|---|---|---|---|
| 1 | non-ASCII `validate_message` panic | **satisfied** | byte-prefix compare in `validate.rs`; multi-byte tests; stub pattern fixed |
| 2 | reconcile-required class | **satisfied** | `BrokerErrorCode::ReconcileRequired`; `CasResolution::ReconcileRequired` + reasons; `reconcile()` verdicts. Residual: no policy layer blocks writers (handoff §8.3) |
| 3 | op-id-seen/content-differs hard | **satisfied** | `OpIdReusedWithDifferentContent`; `AlreadyApplied` only when all files match |
| 4 | projection marker mandatory | **partial** | marker written before files, `complete` last; `verify_projection` refuses missing/incomplete/stale/tampered/wrong-project/wrong-backend. Residual: no call site is forced to verify; no `.hub-cache`/authoritative-dir disjointness guard |
| 5 | corrupt backend config hard-fails | **satisfied** | unparsable text mentioning `state_backend` → `Configuration`; non-string value → error |
| 6 | blind same-path rebase replaced by §5 proof | **satisfied** | `prove_non_overlap`: verify base vs observed; overlap ⇒ `OverlappingPaths`; unprovable/vanished ⇒ `OverlapUnprovable`; head-moved recheck. Residual: no typed path ownership/writer id |
| 7 | inventory↔blob + `blob.commit` cross-checks | **satisfied** | `check_blob` path/commit/`blob_sha`/size; `bytes()` digest |
| 8 | backend identity binding | **partial** | `whoami`/`state` reject wrong UUID/ref; projection binds host+UUID+ref. Residual: per-read, not at construction; repo↔project-UUID binding missing |
| 9 | journal high-water-mark check at append | **unresolved** | `hub_v3.rs` untouched; `append_event_to_ref` has no duplicate/monotonic `(agent_id, agent_seq)` check |

Totals: 6 satisfied, 2 partial, 1 unresolved.

## 6. Exact remaining gate before the first reviewed live Crosslink write

The transport hardening is necessary but not sufficient. The only writes ADR-802
permits are an idempotent derived publish or an inbox item; neither exists. The
gate is therefore **the C-now derived-publish path implemented and reviewed**
(fixed-slot manifest + ≤32 chunks, one whole-tree CAS, watermark comparison on
`stale_state`, fail-closed sizing, digest-verified read-back), with ADR §17 item 9
and the item-4/item-8 residuals closed. For this repository the checkpoint is
1.8 MiB, so a single-file write is impossible and chunking is mandatory. No
SyncManager involvement.

## 7. WHAT-NOT-TESTED

- No live write, no `commit_cas`, no 409/401/403 envelope exercised live.
- The adapter's reconcile path (op-id trailers) was not exercised live; the
  trailer format was only read from the deployed head.
- No C-now manifest/chunk/inbox code exists to test.
- The hardening branch was not built or run here.
- No live test of the broker size limits (this project is far below them).
