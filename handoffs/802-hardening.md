---
issue: 802
title: Decision-independent hardening from the clean-room review panel
status: complete
branch: fix/pp3g-802-decision-independent-hardening
base: feature/pp3g-state-broker-adapter @ cf0902986 (includes ADR-802)
date: 2026-09-21
scope: |
  Only the review findings that do not depend on the deferred per-agent-ref /
  whole-tree-CAS architecture decision. No SyncManager wiring, no live broker
  writes, no architecture decision, no Crosslink redesign.
inputs:
  - handoffs/review-802/99-synthesis.md (+ the five individual reviews)
  - handoffs/802-review-hy3.md
  - .design/state-broker-transport.md
  - .design/state-broker-authority-adr.md (landed on the base branch during this work)
  - crosslink/src/state_broker/* and crosslink/tests/state_broker_*
---

# Decision-independent hardening (#802)

## 1. Disposition per requested finding

| # | Finding | Disposition |
|---|---|---|
| 1 | Non-ASCII `validate_message` panic | **Fixed** — byte-prefix compare in `validate.rs`; identical pattern fixed in the contract stub; tests with `éééé`, `€€€…`, `日本語…`, `𝄞𝄞𝄞` |
| 2 | Same-path `commit_cas` rebase / lost update | **Fixed** — automatic rebase now requires a per-path non-overlap proof (`verify` at base and observed heads) or payload equivalence; otherwise `ReconcileRequired` with `OverlappingPaths`/`OverlapUnprovable` and no write |
| 3 | `Ok` + `verified: false` representable as success | **Fixed** — CAS success shapes (`Applied`/`AlreadyApplied`) imply content verification by construction; `verified: false` (overall *or* per file) becomes `ReconcileRequired` |
| 4 | Corrupt explicit backend config silently falls back to Local | **Fixed** — unparsable `hook-config.json` that mentions `state_backend` is a hard `Configuration` error; only a file that cannot contain the key stays fail-safe to Local |
| 5 | Projection identity/freshness | **Fixed** — every projection carries an atomic marker (schema, backend host, project UUID, state ref, head commit, per-file digests, `complete`); `verify_projection` refuses missing/incomplete/stale/tampered/wrong-project/wrong-backend projections; interrupted hydration leaves `complete: false`; rehydration removes files outside the new manifest |
| 6 | Timeout/upstream/read-back ambiguity | **Fixed** — write-path transport/response/internal/upstream failures and `verified: false` map to `reconcile_required`; `commit_cas` reconciles by op id (landed / not-landed / diverged) and never blind-retries |
| 7 | Mock/stub fidelity divergence | **Fixed** — mock keeps commit history (historical `verify`/`read_blob`), enforces verify path limits/duplicates, reports the commit actually read; stub non-ASCII panic fixed and historical blob `commit` made truthful; the previously-reported "stub requires `expected_head` but the broker treats it as optional" was checked against `state.ts` and is **unsupported** (see §4) |
| 8 | Inventory↔blob cross-check | **Fixed** — `hydrate_into` pins `blob.commit` to the inventory commit and compares path, `blob_sha`, and size against the listed `StateEntry`; `bytes()` still enforces the envelope digest |
| 9 | Windows-reserved path divergence | **Fixed** — no rejection on platforms that can represent the name; Windows gets a documented, reversible `~` escape (reserved names and trailing dots) that cannot collide with broker grammar |
| 10 | Unused/redundant public surface | **Partial** — removed `BrokerErrorCode::ALL`, `StateBackend::into_broker`, `transport_from_env`, `StateBlob::text`; `state_branch` made private. Deferred: `VerifiedFile`/`VerifiedEntry` unification (wire-type churn, not a defect); `digest.rs` kept (used); mock/stub kept (both now referenced by distinct test layers) |

Additional review findings also closed in the same class:

- head-vanished bootstrap escalation (Qwen): a ref that disappears during
  reconciliation is refused with `OverlapUnprovable`, never bootstrapped over;
- verify-after-read TOCTOU (Muse G3): the verdict is re-checked against the
  current head after `verify`; a moved head yields `HeadMovedDuringReconcile`;
- fabricated `previous_head: None` (Muse G2): `AlreadyApplied` no longer
  fabricates a `CommitOutcome`; it carries the commit, message, and verified
  files only;
- `upstream_error` retryability default (GLM): client default flipped to
  non-retryable, matching `errors.ts` (`options.retryable ?? false`);
- silent `hook-config` fallback wording in the design doc: corrected to the
  precise rule.

## 2. Invariants formalized in code/tests

- **Backend/project identity binding.** `whoami()`/`state()` reject a reported
  project UUID or state ref that contradicts the configuration
  (`identity_mismatch`). Projection markers record backend host + project UUID +
  state ref, and both `hydrate_into` and `verify_projection` refuse a projection
  bound to another backend or project.
- **Op-id semantics.** One op id = one intended payload for one writer;
  `commit_cas`/`reconcile` require it; reuse with different content is a
  distinct hard outcome, never success.
- **Stale conflict vs replay vs same-op-different-content.** `stale_state`
  without our trailer ⇒ rebase only after proof; our trailer + matching digests
  ⇒ `AlreadyApplied`; our trailer + different digests ⇒
  `ReconcileRequired(OpIdReusedWithDifferentContent)`; ambiguous transport ⇒
  `reconcile_required`, resolved through `reconcile()` to
  landed / not-landed / diverged.

## 3. Files changed

```
.design/state-broker-transport.md
CHANGELOG.md
crosslink/src/state_broker/client.rs
crosslink/src/state_broker/config.rs
crosslink/src/state_broker/error.rs
crosslink/src/state_broker/mock.rs
crosslink/src/state_broker/mod.rs
crosslink/src/state_broker/projection.rs     (new)
crosslink/src/state_broker/tests.rs
crosslink/src/state_broker/transport.rs
crosslink/src/state_broker/validate.rs
crosslink/tests/state_broker_contract.rs
```

## 4. Findings that turned out unsupported

- **Nemotron: the stub validates `expected_head` presence "which the broker
  treats as optional-null".** Checked against the broker source:
  `state.ts:498` tests `input.expected_head !== null` — `undefined` (a missing
  key) fails that test and is rejected. The stub's requirement matches the
  broker; no change made.
- **GLM: torn-projection claim** was already adjudicated unsupported by the
  synthesis (hydration pins every blob to the head captured at the start). A
  pinning regression test now exists (`stale_projection_is_refused_over_http`).
- **Qwen: cross-host redirect token replay** was already adjudicated
  unsupported (reqwest strips `Authorization`); unchanged.

## 5. Tests added

Unit tests (in-crate): module/unit test count for `state_broker` rose from 41 to
71. New coverage includes: non-ASCII messages; rejected same-path rebase and
allowed equivalent-content rebase; replay vs same-op-different-content;
ambiguous-write reconciliation (landed / not-landed / read-failure);
head-moved-during-reconcile; deleted-ref refusal; projection marker
identity/staleness/incompleteness/tamper/backend-mismatch; stale-file cleanup on
rehydration; inventory↔blob mismatch checks; corrupt-config rules; historical
mock reads; mock verify limits; retryability defaults; Windows escape
reversibility; broker path-grammar exclusion of the marker name.

Integration tests (`tests/state_broker_contract.rs`): 8 → 15, adding
non-ASCII round-trip, `verified: false` (overall and per-file), same-path rebase
refusal over real HTTP, projection staleness over HTTP, project-identity
mismatch, `upstream_error`-on-write classification, and the
`unattached_commit` stale shape.

## 6. Test results

See §7 for the exact command transcript. Summary (final run, this branch):

| Command | Result |
|---|---|
| `cargo test --lib state_broker` | 71 passed, 0 failed |
| `cargo test --lib` (full library) | 1884 passed, 0 failed |
| `cargo test --bin crosslink -- --skip proptest --skip agents_hygiene` | see §7 |
| `cargo test --test state_broker_contract` | 15 passed, 0 failed |
| `cargo test --test cli_integration` | see §7 |
| `cargo test --test state_broker_live` | 1 ignored (live probe; zero writes) |
| `cargo clippy --lib --bins --tests` | no warnings from `state_broker` except the pre-existing `needless_pass_by_value` on the command dispatcher |
| `rustfmt --edition 2021 --check` on touched files | clean |

## 7. Line-count change

Relative to the base branch tip (`feature/pp3g-state-broker-adapter` @
`cf0902986`): see the final commit message / `git diff --stat <base>...HEAD`.

## 8. Remaining architectural blockers before `SyncManager` wiring

These are unchanged by this work and are owned by ADR-802 (binding for wiring):

1. **C-tier machinery does not exist yet**: manifest + fixed-slot chunked
   publish, watermark comparison on `stale_state`, `state_sha256`, and the
   inbox/drain protocol. This branch only hardened the transport primitives the
   ADR's invariants rest on.
2. **Path ownership is still unstated in code**: `commit_cas` refuses
   unprovable rebases, but there is no typed notion of owned paths, an
   owner-scoped op id, or a writer id to bind uniqueness to
   `(project_uuid, writer_id, op_id)`.
3. **Unknown-outcome blocking**: ADR §7 requires the client to *block further
   writes to those paths* until an unknown resolves; the transport returns the
   explicit `reconcile_required` verdict, but no policy layer blocks writers.
4. **Projection disjointness**: ADR §8 requires the projection directory to be
   disjoint from `.crosslink/.hub-cache` and authoritative dirs. The marker
   gate is in place, but `hydrate_into` still accepts any caller-chosen
   directory (a `.git`-worktree/cache guard is not implemented).
5. **Delete/tombstone accounting** remains out of scope (broker v1 has no
   delete).
6. **Broker v2 / per-writer heads (option B)** remains the tracked end-state;
   nothing here presumes it or precludes it.
7. **Live evidence**: no live broker call was made. The ignored read-only probe
   remains the first step for the operator.

## 9. WHAT-NOT-TESTED

- No live broker call, no live write; the deployment's actual envelope shapes
  against the client are still source-derived (the loopback stub is faithful to
  the source but is not the deployment).
- No Windows execution: the Windows escape path is covered by parameterized
  unit tests on Linux, not by a Windows run.
- No multi-process race: the mock/stub serialize internally; true concurrent
  CAS interleavings are not exercised.
- No wire-level limit-boundary traffic (the client rejects locally first).
- The ADR's watermark/manifest/inbox invariants are not implemented or tested
  here by design (decision-dependent).
