# Crosslink State Broker transport adapter

Status: implemented (Crosslink issue #802); decision-independent review
hardening applied (branch `fix/pp3g-802-decision-independent-hardening`).
Branch: `feature/pp3g-state-broker-adapter`
Baseline: `feature/pp3g-Zbk6-cleanup-rescope-349` @ `d323f020dd1a592113d71ebdb1facefb4b426a18`
Parent context: ASES Crosslink issue #564

## 1. Scope

Add the smallest clean Crosslink-side adapter for the deployed
`crosslink-state-broker` so Crosslink can:

- read the durable project state/head;
- hydrate required state blobs into explicitly disposable local projections;
- submit mutations with expected-head/compare-and-swap semantics;
- handle `stale_state` and the other broker failures as typed conditions;
- verify writes by read-back where Crosslink requires it.

Explicit non-goals (unchanged from the stub):

- no redesign of Crosslink's hub v3 model (per-agent refs stay as they are);
- no historical state migration;
- no GitHub credentials in Crosslink;
- no live mutations against the production broker during development;
- no change to the default local/direct git behavior.

## 2. Recon: existing persistence boundaries

Inspected, in order of proximity to "durable central state":

| Boundary | Shape | Why it is not the adapter |
|---|---|---|
| `src/hub_source.rs` — `HubSource` (L49) | read-only input to the compaction reducer (`WorktreeSource`, `ObjectStoreSource`, `RefHubSource`) | no mutation, git/ref-shaped |
| `src/hub_v3.rs` — `CasExpectation`, `commit_upserts_to_ref` (private), `commit_blob_to_ref`, `commit_files_to_ref`, `push_ref*` | per-ref CAS writes through git plumbing | git-object/ref specific; no remote semantic transport; `push` needs a git remote credential |
| `src/sync/mod.rs`, `src/sync/cache.rs`, `src/sync/core.rs` — `SyncManager::fetch`, `fetch_and_adopt_v3_refs` (L432) | remote fetch/adopt of per-agent refs + checkpoint | this *is* the git transport (`git fetch`/`git push`); not abstracted |
| `src/hydration.rs` — `hydrate_to_sqlite_exempt` (L306), `hydrate_from_state` (L520) | writes the local SQLite projection from hub files / reduced state | consumer of state, not a transport; already "disposable projection" in spirit |
| `src/db/*` | local SQLite | local read cache; not durable central state |

**Finding:** no existing `StateStore`/backend abstraction spans read **and**
mutation. The closest candidates are read-only (`HubSource`) or git-specific
(`hub_v3`, `SyncManager`). The broker contract's five semantic operations have
no existing home, so the minimum architectural change is to introduce one
narrow trait — not to refactor the hub.

## 3. Architecture chosen

New module `crosslink/src/state_broker/` (lib + bin module trees):

```
state_broker/
  mod.rs        module contract, re-exports, "local state is disposable" rule
  config.rs     StateBrokerConfig, SecretToken, StateBackend selection
  error.rs      BrokerErrorCode (8 contract codes + transport/protocol/
                configuration/local_io), StateBrokerError, redaction helpers
  validate.rs   client-side mirror of the broker's input rules and limits
  client.rs     StateBrokerClient — blocking HTTP client for contract v1
  transport.rs  ProjectStateTransport trait (+ broker impl, hydrate_into,
                commit_cas reconciliation)
  mock.rs       MockStateTransport — deterministic in-memory broker
  digest.rs     SHA-256 helpers used by client/mock/projection tests
  tests.rs      cross-cutting unit tests
```

### 3.1 The broker-v1-shaped transport seam: `ProjectStateTransport`

The trait names the broker contract's semantic operations. It is a broker-v1
CAS transport, not a Crosslink-domain persistence seam: the mapping from v3
per-agent refs onto one whole-tree CAS is the open §4 decision, and nothing in
this shape pre-answers it.

| Method | Broker op | Notes |
|---|---|---|
| `read_state()` | `GET .../state` | ref, head, inventory, baseline; project UUID and state ref are checked against configuration |
| `read_blob(path, at)` | `GET .../blob` | `at` = exact commit or head |
| `verify(commit, paths)` | `GET .../verify` | read-back digests (≤32 paths) |
| `commit(request)` | `POST .../commit` | expected-head CAS; **never blind-retried**; ambiguous failures become `reconcile_required` |
| `hydrate_into(dir, paths)` | (provided) | materializes blobs into a caller-chosen projection; checks each blob against the inventory (path, commit, blob sha, size) and writes an identity/freshness marker |
| `verify_projection(dir)` | (provided) | fail-closed gate: missing/incomplete/stale/tampered/wrong-project projections are errors |
| `reconcile(request)` | (provided) | explicit op-id reconciliation: landed / not-landed / landed-with-different-content |
| `commit_cas(request, max_retries)` | (provided) | reconciled CAS; see below |

`commit_cas` returns a `CasResolution` verdict:

1. `Applied` — our write landed and read-back verified it.
2. `AlreadyApplied` — the head already records our `op_id` and every requested
   path matches the intended payload byte-for-byte. Only then is a replay
   treated as success; `previous_head` is **not** fabricated.
3. `ReconcileRequired` — never an ordinary success and never a blind retry:
   - `AmbiguousWrite` — timeout/lost/unparseable response, upstream read-back
     mismatch, `verified: false`, or a reconciliation read that failed;
   - `WriteNotLanded` — reconciliation proved the write is not at the head but
     the retry budget was exhausted;
   - `OpIdReusedWithDifferentContent` — the head records our op id with other
     bytes (reused op id or later overwrite);
   - `OverlappingPaths` / `OverlapUnprovable` — a competing commit touched a
     requested path (or the proof could not be read), so rebasing would discard
     another writer's bytes;
   - `HeadMovedDuringReconcile` — the head moved between the verdict and its
     re-check.

**Automatic rebase is refused unless non-overlap is proven.** On a conflict the
retry compares per-path digests at the base and observed heads (`verify` at
both commits); it re-issues only when every requested path is unchanged, or
already carries exactly the intended payload. A vanished ref is never
"rebased" by bootstrapping a fresh history over it.

`op_id` is mandatory for `commit_cas`/`reconcile`. Uniqueness is a caller
obligation: one op id identifies one intended payload for one writer; reuse
with different content is detected, never accepted. Callers with append-style
semantics must use `commit()` and reconcile explicitly.

### 3.2 Local projections are disposable, self-identifying, and freshness-bound

`hydrate_into` writes state files under a caller-chosen root (default:
`.crosslink/state-projection/`, via `default_projection_dir`). It never touches
`SQLite`, never commits to git, and the projection may be deleted at any time:
the durable head is always re-read from the transport.

Every projection carries a marker file
(`.crosslink-state-projection+v1.json`, deliberately outside the broker path
grammar) recording:

- the broker project UUID and state ref (backend/project binding), plus the
  backend host label when the transport has one;
- the head commit the projection was materialized from;
- the logical path, size, and SHA-256 of every projected file;
- a `complete` flag that is set only after the last file is on disk.

The marker is written *incomplete* before the first file, so an interrupted
hydration can never look complete. `verify_projection(dir)` is the fail-closed
gate for consumers: it refuses a missing, incomplete, stale, tampered, or
wrong-project projection. Re-hydrating a directory upserts the selection and
removes files that a previous marker listed but the new selection excludes.
Two projections of different projects cannot share a directory.

The unit test `broker_projection_feeds_existing_state_hydration` executes the
claim: a broker projection containing the v3 checkpoint
(`checkpoint/state.json`, produced by the existing `write_checkpoint`) is read
back with `crate::checkpoint::read_checkpoint` and hydrated into SQLite by
`crate::hydration::hydrate_from_state` — the same entry point the v3 write path
uses. The destructive v2 `hydrate_to_sqlite` path is intentionally not used
from the adapter or its tests: it is audit-guarded to exactly two documented
exemptions (`integrity_drift` temp-DB and `migrate` v2-only import).

### 3.3 Backend selection (preserves existing behavior)

Resolution (`StateBackend::resolve`, env first, then
`.crosslink/hook-config.json`):

| Selection | Result |
|---|---|
| nothing configured | `StateBackend::Local` — existing behavior, untouched |
| `CROSSLINK_STATE_BACKEND=git` / `local` / `direct`, or `"state_backend": "git"` | `Local` |
| `CROSSLINK_STATE_BACKEND=broker` or `"state_backend": "broker"` | `Broker(config)`, or a hard configuration error if URL/token/UUID are missing |
| unknown value | hard configuration error |

Broker environment variables alone (URL/token/UUID present, no selection key)
deliberately do **not** switch the backend: the Codex Cloud environment sets
them for its own client, and Crosslink must not silently change where durable
state goes. There is no silent fallback either — a misconfigured broker
selection fails loudly.

Concretely, a present-but-unparsable `hook-config.json` **fails hard when the
raw text mentions `state_backend`** (the file may have selected the broker and
falling back to Local would silently route durable state to the wrong store).
When the raw text does not mention the key, the file cannot have selected a
backend: the adapter warns and keeps Local, so unrelated config damage does not
become a hard failure.

### 3.4 Secret handling

- `SecretToken` redacts itself in `Debug`, implements neither `Display` nor
  `Serialize`, and exposes its value only to the HTTP client
  (`pub(crate)` path plus a documented `expose()`).
- Every error message and `details` value is passed through
  `StateBrokerConfig::redact`; `redact_value` walks JSON.
- The token travels only in the `Authorization` header to the configured host.
- Plain-`http` broker URLs are refused unless the host is loopback (the token
  would be cleartext).
- `CROSSLINK_STATE_BROKER_TOKEN_FILE` is supported so the operator can place
  the secret on the machine without it entering chat/config.
- Crosslink never writes the token to any file.

## 4. Exact integration point and what is still required

### Delivered call sites

- `crosslink state-broker status` (`src/commands/state_broker.rs`) — read-only
  operator/live-test entry point (whoami + state; never commits).
- Tests: `tests/state_broker_contract.rs` (loopback stub) and
  `tests/state_broker_live.rs` (ignored, read-only live probe).

### The next production integration point (not switched in this change)

The hub's remote transport boundary is:

- **read:** `sync::SyncManager::fetch_and_adopt_v3_refs` (`src/sync/cache.rs`)
  and `hub_v3::fetch_v3_refs_for_join`;
- **write:** `hub_v3::push_agent_ref` / `push_ref` / `push_ref_with_lease`, and
  the per-ref CAS core `hub_v3::commit_upserts_to_ref`.

Those operate on Crosslink's v3 multi-ref layout
(`refs/heads/crosslink/agents/*`, `crosslink/checkpoint`, `crosslink/meta`)
while the broker owns a single state tree under
`refs/heads/projects/<uuid>/state`. The mapping decision this document
originally deferred is now recorded in **`.design/state-broker-authority-adr.md`**
(ADR-802, status *Proposed — binding for any `SyncManager` wiring until
superseded*): adopt **C now** (broker = derived checkpoint/read tier plus a
bounded inbox transport; v3 per-agent refs remain the journal of record), reject
option A as steady state, and track per-writer heads (B) as a broker-v2 goal.
Wiring `SyncManager` remains out of scope here and is gated by the ADR's safe
condition.

### Remaining assumptions (WHAT-NOT-TESTED)

- **No live broker call was made during development.** The contract types were
  derived from the broker source (`src/app.ts`, `src/state.ts`, `src/errors.ts`,
  `src/paths.ts`) and its documented examples; `tests/state_broker_live.rs`
  exists to confirm the contract against the deployment when the operator runs
  it ([certainty: evidence-based, from source; not yet verified live]).
- **No broker contract incompatibility was found** (see §5).
- The broker has **no delete operation** in v1 (`commit` upserts only). State
  that must be *removed* needs a tombstone convention or a broker contract
  change; Crosslink's hub prune/rewrite paths are therefore out of scope for
  the broker transport. (Still unresolved; deliberately not decided here.)
- Concurrency: the broker's CAS is a whole-ref compare-and-swap with retry
  semantics, unlike v3's per-agent refs. `op_id` uniqueness is a **caller
  obligation**; `commit_cas` detects same-op/different-content and refuses
  automatic rebases that cannot be proven non-overlapping.
- The full Crosslink hub tree (event logs, checkpoints, meta, locks) has not
  been round-tripped through the broker; only the transport semantics and the
  v3-checkpoint projection path are proven.
- `hook-config.json`'s `state_backend` key is read but deliberately not
  registered in the config registry/TUI in this change (env is the primary
  selector). `crosslink config get state_backend` will not know it yet.
- Path ownership (which broker paths are agent-private vs shared) remains
  unstate; `commit_cas` is safe only where the non-overlap proof holds.

## 5. Broker compatibility notes

- Envelope handling covers success/failure, unknown codes (→ typed `protocol`
  error, never silently mapped), non-envelope bodies, and 401/403/404/405/409/
  502/500 statuses (`tests/state_broker_contract.rs`).
- Client-side validation mirrors the broker limits (32 files/commit,
  256 KiB/file, 1 MiB/commit, 512-char single-line message, 32 verify paths,
  path grammar). The broker remains authoritative; local checks only avoid
  pointless round-trips.
- `stale_state` (409) carries `details.observed_head` and writes nothing —
  verified against the stub and exercised through `commit_cas`.
- **Write ambiguity is typed.** On `POST /commit`, transport failures,
  unparseable responses, `internal_error`, `upstream_error`, and success
  envelopes with `verified: false` all surface as
  `BrokerErrorCode::ReconcileRequired` (`details.reason` = `transport_ambiguous`
  | `response_ambiguous` | `readback_mismatch` | `verified_false`), never as an
  ordinary error or success. Definite rejections (401/403/400/404/405/409) pass
  through unchanged. `upstream_error` defaults to non-retryable, matching the
  broker's own default (`errors.ts`: `options.retryable ?? false`).
- Broker-reported project identity is bound to configuration: `whoami()` and
  `state()` reject a project UUID or state ref that contradicts the configured
  identity (`identity_mismatch`).
- No incompatibility with the deployed broker contract was found in this work.

## 6. Configuration reference

| Name | Kind | Purpose |
|---|---|---|
| `CROSSLINK_STATE_BACKEND` | env | `broker` selects the broker; `git`/`local`/`direct` forces existing behavior |
| `state_backend` (`hook-config.json`) | config | Same selection, lower precedence than the env var |
| `CROSSLINK_STATE_BROKER_URL` | env | Broker base URL (`https`, or `http` only for loopback) |
| `CROSSLINK_STATE_BROKER_TOKEN` | env secret | Bearer token |
| `CROSSLINK_STATE_BROKER_TOKEN_FILE` | env path | Path to a file containing the token (alternative to the env secret) |
| `CROSSLINK_STATE_PROJECT_UUID` | env | Project UUID the token is bound to (required when selecting the broker) |
| `CROSSLINK_STATE_TIMEOUT_MS` | env | Per-request timeout, default 15000 |

## 7. Evidence: tests and checks

Commands run in the worktree (`CARGO_TARGET_DIR` pointed at the existing build
cache; no source changes to the preserved branch):

| Command | Result |
|---|---|
| `cargo test --lib state_broker` | 41 passed, 0 failed |
| `cargo test --bin crosslink state_broker` | 42 passed, 0 failed |
| `cargo test --bin crosslink -- --skip proptest --skip agents_hygiene` (full bin suite) | 2914 passed, 0 failed, 53 filtered (37 proptest + 16 `agents_hygiene`) |
| `cargo test --test cli_integration` (full CLI suite) | 199 passed, 0 failed |
| `cargo test --test state_broker_contract` | 8 passed, 0 failed (real HTTP over 127.0.0.1) |
| `cargo test --test state_broker_live` | 0 run, 1 ignored (live probe; requires operator env) |
| `cargo clippy --lib` | 0 warnings from `state_broker` (pre-existing lib warnings remain) |
| `cargo clippy --bin crosslink` | 1 pedantic warning in the new code (`needless_pass_by_value` on the command dispatcher), matching the existing command-module pattern |
| `crosslink state-broker status` (local backend) | reports `local (default)` and the selection instructions |
| `crosslink state-broker status` with `state_backend=broker` but no env | hard configuration error naming the missing variables |
| `crosslink state-broker status` with broker env, unreachable loopback | typed `transport` error, host-only URL, token absent |

Repo-wide `cargo fmt --all` was **not** applied: the preserved baseline is not
fmt-clean (unrelated files would churn). `rustfmt` was applied to the new and
touched files only.

## 7.1 Independent review

An independent read-only adversarial review by `opencode-go/hy3` (operator
approved per launch; catalog refreshed 2026-09-21) examined the module, tests,
and this document against the broker contract source. Findings and
dispositions are recorded in full in `handoffs/802-review-hy3.md`. Summary:

- **major** — `commit_cas`'s `already_applied` branch reported `verified` from
  path presence instead of comparing digests against the intended payload;
  fixed with a regression test.
- **minor** (×2) — the loopback stub was more lenient than the real broker
  (head-only verify, no commit-body validation); the stub now keeps commit
  history and re-validates inputs independently.
- **nit** (×2) — `SecretToken::expose` narrowed to `pub(crate)`; message
  validation now matches the broker's exact control-character rule.

The reviewer's contract-conformance, secret-safety, projection-safety, and
integration-point verdicts were clean. It ran no tests and made no live broker
call (disclosed).

## 7.2 Known pre-existing flake (not from this change)

`commands::agents_hygiene::tests` is parallelism-flaky because `run_sync`
installs the policy at `crosslink_dir.parent()/AGENTS.md` and the tests pass a
bare tempdir (so the target is the shared `/tmp/AGENTS.md`). Reproduction: 2
failures in 15 consecutive module runs; the module passes in isolation, and its
source is untouched here. Filed as Crosslink issue #803; the adapter's own
tests use per-test tempdirs only.

## 7.3 Clean-room panel review and decision-independent hardening

A five-model clean-room panel and its synthesis (`handoffs/review-802/`) plus
the earlier Hy3 review identified defects that do not depend on the deferred
mapping decision. They are fixed on branch
`fix/pp3g-802-decision-independent-hardening`, without wiring `SyncManager` and
without deciding the mapping (now recorded in ADR-802). Finding-by-finding
disposition and evidence: `handoffs/802-hardening.md`. Summary:

- non-ASCII commit messages no longer panic client validation;
- `commit_cas` refuses automatic same-path rebases without a per-path
  non-overlap/equivalence proof and returns explicit `CasResolution` verdicts;
- `verified: false` (overall or per file) and ambiguous writes
  (timeout/response/upstream) are typed `reconcile_required` and are never
  ordinary success or blind retries;
- projections carry backend/project/head/digest markers with a fail-closed
  `verify_projection` gate; interrupted hydration cannot look complete;
- a corrupt `hook-config.json` that may select the broker fails hard;
- blob reads are cross-checked against the pinned inventory;
- mock and stub now model commit history, historical reads, and broker limits
  faithfully;
- broker-legal Windows-reserved paths are representable (reversible escape)
  instead of un-hydratable;
- unused public surface removed (`BrokerErrorCode::ALL`,
  `StateBackend::into_broker`, `transport_from_env`, `StateBlob::text`).

Hardening evidence (final run): `cargo test --lib state_broker` 71 passed;
`cargo test --bin crosslink state_broker` 72 passed; full library 1884 passed;
full bin suite 2944 passed (53 filtered: proptest + the pre-existing
`agents_hygiene` flake); `cli_integration` 199 passed;
`state_broker_contract` 15 passed (real HTTP over loopback). The deployed
read-only smoke is `handoffs/802-live-smoke.md`.

## 8. Next step: live verification (after the Codex Cloud durability experiment passes)

1. Operator places the broker token on the machine, e.g.
   `/tmp/opencode/secrets/crosslink-state-broker.token` (never in chat), and
   exports `CROSSLINK_STATE_BROKER_URL`, `CROSSLINK_STATE_BROKER_TOKEN_FILE`,
   `CROSSLINK_STATE_PROJECT_UUID`. Crosslink never persists the token.
2. Read-only smoke test from a Crosslink checkout:
   `CROSSLINK_STATE_BACKEND=broker crosslink state-broker status --json`
   → expect `backend: broker`, the project UUID, scopes, `head_commit`
   (or `null`), and `baseline_matches: true`.
3. Contract confirmation:
   `cargo test --test state_broker_live -- --ignored --nocapture`
   (reads `health`, `whoami`, `state`; performs zero writes).
4. First write (separate, reviewed step): hydrate the full state tree into a
   disposable projection with `hydrate_into`, then CAS a single checkpoint file
   through `commit_cas`; assert the `CasResolution` is `Applied`/`AlreadyApplied`
   (`is_verified()`) and re-verify with `verify()`/`verify_projection()`. This
   is the moment to validate the §4 mapping decision for the hub before
   `SyncManager` is routed to the broker.

## 9. Changed paths

```
crosslink/Cargo.toml                                  (+ reqwest "blocking")
crosslink/Cargo.lock                                  (feature deps)
crosslink/src/lib.rs                                  (pub mod state_broker)
crosslink/src/main.rs                                 (mod + `state-broker` command)
crosslink/src/commands/mod.rs                         (pub mod state_broker)
crosslink/src/commands/state_broker.rs                (new: read-only CLI)
crosslink/src/state_broker/…                          (new module)
crosslink/tests/state_broker_contract.rs              (new: loopback stub contract tests)
crosslink/tests/state_broker_live.rs                  (new: ignored read-only live probe)
.design/state-broker-transport.md                     (this document)
.design/state-broker-authority-adr.md                 (ADR-802 mapping decision)
handoffs/802-hardening.md                             (decision-independent hardening report)
CHANGELOG.md                                          (Unreleased entry)
```

Review hardening (branch `fix/pp3g-802-decision-independent-hardening`) adds
`crosslink/src/state_broker/projection.rs` and extends the files above; see
`handoffs/802-hardening.md` for the finding-by-finding disposition.
