# Crosslink State Broker transport adapter

Status: implemented (Crosslink issue #802)
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

### 3.1 The narrow seam: `ProjectStateTransport`

Five operations, matching the broker contract semantically:

| Method | Broker op | Notes |
|---|---|---|
| `read_state()` | `GET .../state` | ref, head, inventory, baseline, registry |
| `read_blob(path, at)` | `GET .../blob` | `at` = exact commit or head |
| `verify(commit, paths)` | `GET .../verify` | read-back digests (≤32 paths) |
| `commit(request)` | `POST .../commit` | expected-head CAS; **never blind-retried** |
| `hydrate_into(dir, paths)` | (provided) | materializes blobs into a caller-chosen directory |

`commit_cas(request, max_retries)` is a provided method that reconciles
`stale_state` conservatively:

1. re-read the durable head once;
2. if the head commit's trailers record **our own `op_id`** (`Broker-Op:`) the
   write already landed → verify our paths at that head and return
   `already_applied: true` without writing;
3. otherwise re-issue with the freshly observed head (whole-file upserts only).

`op_id` is mandatory for `commit_cas`; callers with append-style semantics must
use `commit()` and reconcile explicitly. This mirrors the broker README's
"never blind-retry a write" rule.

### 3.2 Local projections are disposable — and existing code can consume them

`hydrate_into` writes state files under a caller-chosen root (default:
`.crosslink/state-projection/`, via `default_projection_dir`). It never touches
`SQLite`, never commits to git, and the projection may be deleted at any time:
the durable head is always re-read from the transport.

The unit test `broker_projection_feeds_existing_sqlite_hydration` executes the
claim: a broker projection containing v2-layout `issues/<uuid>.json` +
`meta/counters.json` is fed **unchanged** to
`crate::hydration::hydrate_to_sqlite_exempt`, which populates SQLite as today.

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
- `state_broker::transport_from_env()` — construction for programmatic callers.
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
`refs/heads/projects/<uuid>/state`. Swapping the transport therefore requires
one design decision that is intentionally **not** made here:

> **How Crosslink's v3 per-agent refs map onto the broker's single state
> tree.** Options: (a) one file per per-agent ref tip (`agents/<id>/events.log`,
> `checkpoint/state.json`, `meta/hub.json`) with a whole-tree CAS per mutation —
> simple but serializes all writers and needs chunking for the 32-file/256 KiB/
> 1 MiB limits; (b) event-log shards per agent per commit; (c) keep per-agent
> refs by storing ref-index files. Until that is decided and reviewed, wiring
> `SyncManager` to the broker would change the hub's concurrency model, which
> the task's "do not redesign Crosslink" constraint forbids.

Because the trait already expresses the broker's semantics, adopting option (a)
later is an adapter implementation plus call-site routing — no redesign of
`hydrate`/`db`/`compaction`.

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
  the broker transport.
- Concurrency: the broker's CAS is a whole-ref compare-and-swap with retry
  semantics, unlike v3's per-agent refs. Single-writer-per-op-id is assumed for
  `commit_cas` (the op-id trailer check is the reconciliation authority).
- The full Crosslink hub tree (event logs, checkpoints, meta, locks) has not
  been round-tripped through the broker; only the transport semantics and the
  v2-file projection path are proven.
- `hook-config.json`'s `state_backend` key is read but deliberately not
  registered in the config registry/TUI in this change (env is the primary
  selector). `crosslink config get state_backend` will not know it yet.

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
- Read-back mismatch (`upstream_error` with `details.failed_paths`) is surfaced
  as non-retryable with the commit sha preserved, per the contract's "reconcile
  before retrying".
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
| `cargo test --lib state_broker` | 40 passed, 0 failed |
| `cargo test --bin crosslink state_broker` | 41 passed, 0 failed |
| `cargo test --test state_broker_contract` | 6 passed, 0 failed (real HTTP over 127.0.0.1) |
| `cargo test --test state_broker_live` | 0 run, 1 ignored (live probe; requires operator env) |
| `cargo clippy --lib` | 0 warnings from `state_broker` (pre-existing lib warnings remain) |
| `cargo clippy --bin crosslink` | 1 pedantic warning in the new code (`needless_pass_by_value` on the command dispatcher), matching the existing command-module pattern |
| `crosslink state-broker status` (local backend) | reports `local (default)` and the selection instructions |
| `crosslink state-broker status` with `state_backend=broker` but no env | hard configuration error naming the missing variables |
| `crosslink state-broker status` with broker env, unreachable loopback | typed `transport` error, host-only URL, token absent |

Repo-wide `cargo fmt --all` was **not** applied: the preserved baseline is not
fmt-clean (unrelated files would churn). `rustfmt` was applied to the new and
touched files only.

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
   through `commit_cas`; assert `outcome.verified` and re-verify with
   `verify()`. This is the moment to validate the §4 mapping decision for the
   hub before `SyncManager` is routed to the broker.

## 9. Changed paths

```
crosslink/Cargo.toml                                  (+ reqwest "blocking")
crosslink/Cargo.lock                                  (feature deps)
crosslink/src/lib.rs                                  (pub mod state_broker)
crosslink/src/main.rs                                 (mod + `state-broker` command)
crosslink/src/commands/mod.rs                         (pub mod state_broker)
crosslink/src/commands/state_broker.rs                (new: read-only CLI)
crosslink/src/state_broker/…                          (new module, 9 files)
crosslink/tests/state_broker_contract.rs              (new: loopback stub contract tests)
crosslink/tests/state_broker_live.rs                  (new: ignored read-only live probe)
.design/state-broker-transport.md                     (this document)
CHANGELOG.md                                          (Unreleased entry)
```
