---
issue: 804
title: CDP-1 derived-checkpoint publish path — implementation handoff
status: complete — ready for independent implementation review
branch: feat/pp3g-804-cdp1
base: fix/pp3g-802-decision-independent-hardening @ d4105831e
adapter_integration: feature/pp3g-state-broker-adapter @ 89a59a52b merged at c11059880 (no conflicts)
protocol_revision: design/pp3g-802-derived-publish @ 7e3f71f94
date: 2026-09-21
---

# CDP-1 implementation

## 1. Protocol revision implemented

`.design/state-broker-derived-publish.md` at `7e3f71f94` (the corrected,
reviewed CDP-1 revision), brought into this branch unchanged by
`4e30c7d04`. The clean-room synthesis and raw reviews are preserved under
`handoffs/review-cdp1/`.

The implementation branch is based on the hardening branch
(`fix/pp3g-802-decision-independent-hardening` @ `d4105831e`) because that
branch carries the ADR-802 §17 prerequisite code (reconcile-required class,
op-id divergence outcome, projection marker, §5 overlap proof, inventory↔blob
cross-checks, identity binding). This is a branch-base choice, not a protocol
change; the frozen spec was applied on top unmodified except for the
corrections already made in `7e3f71f94`.

## 2. Files changed

New module `crosslink/src/state_broker/cdp1/` (5696 lines):

| File | Purpose |
|---|---|
| `mod.rs` | constants, accounting models, slot paths, owned namespace |
| `manifest.rs` | `CheckpointManifestV1`, canonical compact JSON, §4.3 M1–M16 validation |
| `payload.rs` | deterministic gzip encode, bounded decode, fixed-slot split/join |
| `capacity.rs` | §8 budgets, fail-closed checks, exact boundary math |
| `source.rs` | pushed-checkpoint resolution, repository↔UUID binding, journal anchor |
| `attempt.rs` | write-ahead attempt record + atomic store + blocking rules |
| `publisher.rs` | head classification, single-commit CAS, read-back, op-id reconcile |
| `reader.rs` | pinned reads, digest ladder, provenance, defect classes |
| `projection.rs` | v2 marker, hydration gate, freshness verification |
| `tests.rs` | 72 deterministic tests (T01–T55 where offline-verifiable) |

Touched existing files:

- `crosslink/Cargo.toml` / `Cargo.lock` — `flate2` (miniz_oxide backend).
- `crosslink/src/state_broker/mod.rs` — `pub mod cdp1`.
- `crosslink/src/state_broker/error.rs` — `protocol_with_details` for
  `details.defect`.
- `crosslink/src/state_broker/mock.rs` — commit-attempt counter, lost-response
  injection, head-snapshot-aware corruption/removal, live-only upsert (inventory
  mismatch injection), read counter.
- `crosslink/src/hub_v3.rs` — ADR-802 §17 item 9 journal high-water-mark check
  at append + regression test.
- `crosslink/src/commands/state_broker.rs`, `crosslink/src/main.rs` —
  `state-broker publish-checkpoint [--dry-run] [--verify-full] [--publisher-id]`.
- `crosslink/tests/state_broker_contract.rs` — 5 CDP-1 loopback tests.
- `crosslink/tests/state_broker_live.rs` — 2 ignored read-only probes.
- `crosslink/tests/cli_integration.rs` — 2 fail-closed CLI tests.
- `CHANGELOG.md` — Unreleased entry.

## 3. Architecture

**Publisher** (`publisher::publish_checkpoint`): preflight (identity/scope) →
crash recovery by op id from the local attempt record → `plan_publish`
(resolve the *pushed* checkpoint blob via `git fetch` + remote-tracking ref,
parse, require a watermark, gzip, chunk, manifest, capacity) → head
classification by watermark/semantic identity → one `CommitRequest`
(manifest + active chunks, `expected_head`, `op_id`) → read-back
(`verify` + manifest re-read + optional full reconstruction) → op-id
reconciliation for every ambiguous outcome. It never calls the generic
`commit_cas`; the publisher-specific reconcile implements the §5.3 ownership +
watermark proof and the §5.7 active-path verification.

**Reader** (`reader::read_derived_checkpoint`): pins the head commit `C`, reads
the manifest and only the manifest's active chunks at `C`, checks
`blob.commit == C` and the head inventory, runs the digest ladder
(chunk → payload → decompressed state), parses, compares the watermark by
value, and returns `provenance = JournalAnchored` only after the git anchor
check (`state_blob_sha` + byte equality + digest). Broker-only reads are
`Advisory`.

**Projection** (`projection::write/verify_derived_projection`): v2 marker
derived from the verified manifest; `journal_anchored` is mandatory for the
authoritative hydration seam; freshness re-reads the head and the manifest and
re-checks the projected file.

## 4. T01–T55 disposition

All tests are deterministic and offline; live L1/L2 remain out of CI.

| IDs | Disposition |
|---|---|
| T01–T02 | `t01_t02_one_and_multiple_chunks_publish_and_read_back` (M, multi-chunk via high-entropy state) |
| T03–T04 | `t03_t04_shrinking_publish_leaves_old_slots_inert` (M) |
| T05–T09 | `t05`–`t09` (M): corrupt chunk/manifest, ordering, digest layers, missing chunk |
| T10 | `t10` (M) + `cdp1_stale_state_over_loopback_http_does_not_clobber` (H) |
| T11–T14 | `t11`–`t14` (M), incl. op-id reuse divergence |
| T15–T16 | `t15`, `t16` (M) + `cdp1_unverified_commit_reconciles_over_loopback_http` (H) |
| T17–T18 | `cdp1_readback_mismatch_retries_and_lands_over_loopback_http` (H), `t42`, `t43`, stale-unattached H case |
| T19 | `t19` (M, reconcile read failure → unknown) |
| T20–T21 | `t20`, `t21` (U/M) + capacity unit boundaries |
| T22–T25 | `t22`–`t25` (M) + `t24_projection_min_watermark` |
| T26 | `t26_incomplete_projection_marker_is_refused` (M) |
| T27 | `t27` (M, pinned read across a concurrent publish) |
| T28–T29 | `payload::gzip_is_deterministic_and_header_is_frozen`, `manifest::canonical_bytes_round_trip_and_are_stable` (U) |
| T30 | `t30` (U/M, watermark equality by value) |
| T31 | `t31` (M, no delete, active-path reads only) |
| T32 | `t32` (M, attempt-record crash recovery) |
| T33 | **Deferred:** needs a captured real-checkpoint fixture. Synthetic full reconstruction is covered by T01–T02/T41; the real-blob comparison is part of L1. |
| T34 | `t34` (M, foreign `checkpoint/**` without manifest) |
| T35 | CDP-1 H tests exercise the strict loopback stub (limits/path grammar/base64) |
| T36 | `t36` (M, anchored vs advisory) |
| T37 | `t37` (M, projection write/verify/stale/tamper) |
| T38 | `t38` (M, zero broker calls) + CLI tests |
| T39 | `t39_inventory_blob_mismatch_is_refused` (M); H-level mismatch injection is not modelled by the loopback stub |
| T40 | `t40` (M, content-identical re-commit → `AlreadyCurrent`) |
| T41 | `t41` (M, same op id + equal identity + differing payload → `Landed`) |
| T42 | `t42` (M, active-paths-only reconcile) |
| T43 | `t43` (M, vanished ref → unknown, no bootstrap) |
| T44 | `t44` (M, advisory projection refused by the hydration gate) |
| T45 | `t45` (M, null head watermark → `HeadManifestUnreadable`) |
| T46 | `t46` (M, equal-watermark race → `AlreadyCurrent`/`Diverged`, never clobber) |
| T47 | `t47` (U, wire boundaries 196,608 / 782,336 / 4 slots) |
| T48 | `t48` (U, bounded decompression) |
| T49 | `t49` (U, source is always a git commit/blob, never a projection) |
| T50 | `t50` (M, forged manifest advisory-only) |
| T51 | `t51` (U, cross-agent `Ord`) |
| T52 | `cdp1_oversized_manifest_is_refused_at_loopback` (H, 4,097-byte manifest) |
| T53 | `t53` (U, op-id grammar boundary) |
| T54 | `t54` (U, commit-message boundaries) |
| T55 | `t55` (M, `Refused` ≠ `ReconcileRequired`; wrong token → `FailedClosed`) |

## 5. Handling of the S1–S5 corrections

- **S1 (semantic identity):** `classify_head` and `reconcile` compare
  `(watermark, source.state_sha256)` only. `source.commit`, `state_blob_sha`,
  `state_bytes`, and `payload_sha256` are provenance. Covered by T40 and T41.
- **S2 (pinned/inventory checks):** the reader enforces `blob.commit == C` and
  the inventory↔blob cross-check for the manifest and every chunk before any
  parse; Appendix B's checks are implemented in `reader.rs`. Covered by T27/T39.
- **S3 (repo↔project UUID binding):** `RepositoryBinding::require_for` reads
  `state_broker_binding` from `.crosslink/hook-config.json`, normalizes the
  tracker remote (`host/path`), and refuses when the binding is absent or
  disagrees with either the configured UUID or the repository. The CLI calls it
  before any broker call; covered by unit tests and
  `state_broker_publish_checkpoint_requires_repo_binding`.
- **S4 (provenance gate):** `Provenance::{JournalAnchored, Advisory}` is on the
  reader result and the v2 marker; `write_derived_projection` requires the git
  anchor, and `verify_derived_projection` refuses an `advisory` marker when the
  consumer is the authoritative hydration seam. Covered by T36/T44/T50.
- **S5 (accounting):** `AccountingModel::{Decoded, Wire}` parameterizes the slot
  size, payload cap, manifest validation (M11), capacity checks, and Appendix D
  values. Primary is `Decoded`; the Wire model uses 196,608 / 782,336 / 4 slots.
  Covered by T21/T47.

## 6. Test results

| Command | Result |
|---|---|
| `cargo test --lib state_broker` | 152 passed, 0 failed |
| `cargo test --bin crosslink state_broker` | 153 passed, 0 failed |
| `cargo test --lib hub_v3` | 54 passed, 0 failed |
| `cargo test --test state_broker_contract` | 21 passed, 0 failed (5 CDP-1 loopback) |
| `cargo test --test state_broker_live` | 0 run, 3 ignored (read-only probes) |
| `cargo test --bin crosslink -- --skip proptest --skip agents_hygiene --skip dashboard::projects::tests` | 3000 passed, 0 failed, 77 filtered |
| `cargo test --test cli_integration` | 201 passed, 0 failed |
| `cargo clippy --lib --tests` | 0 warnings in `cdp1/`, `mock.rs`, `state_broker_contract.rs`; pre-existing warnings elsewhere |
| `cargo clippy --bin crosslink` | 0 warnings in the new command/module |
| `rustfmt --check` (touched files, `skip_children`) | clean |
| `git diff --check` | clean |

Environment note: the full `cargo test --lib` and the unfiltered bin suite hang
in this sandbox on `dashboard::projects::tests` (each test runs >60 s, likely a
blocked git subprocess); they are unrelated to CDP-1 and were skipped. The
established suite selection from the prior evidence was used.

## 7. Divergence from the frozen protocol

None. Two clarifications where the protocol left room:

1. **Verify error at a carried commit:** the spec's "otherwise ⇒ `NotLanded`"
   is implemented as `NotLanded` for a `not_found` carried commit and
   `Unknown` for other read failures. A missing carried commit means the broker
   never attached it (safe to re-classify and retry); a transport failure is
   genuinely unknown and blocks. This is stricter than the literal pseudocode
   for non-404 failures and does not change any safe path.
2. **CLI dry run skips `whoami`** so it performs zero network calls (T38); the
   write scope is therefore assumed, not checked, in dry-run mode. The JSON
   output labels the mode. A real run always checks `whoami` scopes.

## 8. Unresolved pre-L1 evidence gates

1. **Decoded-vs-wire limit accounting — UNRESOLVED.** No read-only request can
   reveal it, and the deployed broker source is not in this repository. The
   implementation is parameterized and fails closed under both models; the
   resolution requires the broker source or a reviewed live boundary probe (L2)
   on a synthetic project. Exact remaining gap: whether the broker's
   `maxFileBytes`/`maxTotalBytes` apply to decoded content or to the base64
   wire body. No near-boundary publish may run before this is resolved.
2. **Historical-commit serving — PROBE ADDED, NOT RUN.**
   `live_historical_commit_read_probe` performs a read-only blob/verify at a
   caller-supplied non-head commit. It needs an operator-supplied
   `CROSSLINK_STATE_HISTORICAL_COMMIT` and `CROSSLINK_STATE_HISTORICAL_PATH`
   because no non-head commit sha is discoverable from the client side. If the
   deployment only serves head-pinned reads, `LandedSuperseded` degrades to
   `NotLanded` (benign) and that degradation must be accepted explicitly.
3. **Trailer behavior under contention — UNRESOLVED.** Cannot be tested without
   a write. The trailer format is confirmed from the live smoke; the
   attribution argument (atomic commit + `Broker-Op` trailer) is exercised only
   against the mock/stub.
4. **Repo↔UUID binding** is implemented and enforced, but the operator must add
   the `state_broker_binding` block to `.crosslink/hook-config.json` before any
   real publish.

## 9. Readiness

- **Independent implementation review:** ready. The diff is additive, the
  protocol revision is frozen, and every §12 gate is either closed in code or
  listed above as an explicit pre-L1 gap. Suggested review focus: the publisher
  reconcile state machine, the reader defect ladder, and the binding/gate
  enforcement.
- **Live L1 write:** not yet safe to authorize. Gates 1–3 above must be
  resolved (or the historical/contention degradations explicitly accepted), the
  operator must add the repository binding, and the implementation must pass
  independent review. The write itself remains a separate per-write operator
  approval.

## 10. Final commit

`74f989c99` on `feat/pp3g-804-cdp1` (5 commits from the hardening base:
`4e30c7d04`, `51d9750a1`, `c6de2c4ed`, `ea9677448`, `f275d613e`, plus the
handoff commit). No push was performed. No live broker operation was performed.

## 11. Adapter-tip integration

The final adapter branch tip (`feature/pp3g-state-broker-adapter` @ `89a59a52b`,
which already contains the hardening tip `d4105831e`) was merged into CDP-1 at
`c11059880`:

- **No conflicts.** The net delta of the two adapter commits
  (`66eade87c`, `89a59a52b`) is `handoffs/802-hardening.md` only; the
  proptest-seed files are added and then removed, so they do not appear in the
  merge. CDP-1 touches neither file.
- **`89a59a52b` is an ancestor of the new CDP-1 HEAD** (verified with
  `git merge-base --is-ancestor`).
- **The reviewed CDP-1 implementation is unchanged:** `git diff 74f989c99..HEAD`
  contains exactly `handoffs/802-hardening.md`; no `state_broker/`, `cdp1/`,
  `hub_v3.rs`, CLI, test, `Cargo`, or frozen-spec file changed.
- **Post-merge focused suites (behavior unchanged):** `--lib
  state_broker::cdp1` 72 passed; `--lib state_broker` 152 passed; `--bin
  crosslink state_broker` 153 passed; `--lib hub_v3` 54 passed; `--test
  state_broker_contract` 21 passed; `--test state_broker_live` 3 ignored;
  focused `cli_integration` CDP-1 tests 2 passed; `git diff --check` clean.
- No protocol change, no feature work, no `SyncManager`, no live write, no push.
