---
artifact: panel review
reviewer: 3
requested_model: Big Pickle → MiMo V2.5 Free (substitute)
model_id: opencode/mimo-v2.5-free
provider: OpenCode Zen (substitute)
variant: default
privacy: MiMo-V2.5 Free: during free period collected data may be used to improve the model (sanitized packet used). Big Pickle failed twice (403 free-tier), MiMo substituted; MiMo's run ended with the same 403 after emitting the full report.
started: 2026-09-21T04:28:35Z
finished: 2026-09-21T04:33:00Z
tokens_total: 912417
cost_usd: 0.0
assistant_messages: 9
finish_reasons: ['tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'stop']
session_id: ses_f3dc6d92cffePhFQb9Zou0Ppzf
extraction: raw events (report complete in the final text part)
---

# Raw reviewer output — Big Pickle → MiMo V2.5 Free (substitute)

Now I have everything I need. Here is my independent review.

---

# Independent Architecture Panel Review — Issue #802

## 1. Architecture Assessment

**Verdict: Conditionally sound for its stated scope; the adapter boundary is the right seam but the trait is over-abstracted relative to what it actually needs to abstract.**

The `ProjectStateTransport` trait is the correct *location* for the abstraction (between Crosslink's persistence consumers and the broker's wire protocol). The recon in `design/state-broker-transport.md:29-43` is accurate: `HubSource` is read-only, `hub_v3` is git-plumbing-specific, and `SyncManager` is the git transport itself. No existing seam spans read + mutation against a backend-agnostic durable state store.

However, the trait's *shape* conflates three concerns:

1. **Wire protocol** (five operations matching the broker v1 API) — correctly owned by `StateBrokerClient`.
2. **Projection management** (`hydrate_into`) — materializing blobs to disk. This is a consumer-side concern, not a transport concern. The real broker does not have a "hydrate" operation; the adapter fabricates one by iterating `read_blob` calls and writing files.
3. **CAS reconciliation** (`commit_cas`) — the provided method that wraps `commit` + `read_state` + `verify` with retry logic.

The trait should be narrower: expose the four wire operations (`read_state`, `read_blob`, `verify`, `commit`) and leave `hydrate_into` and `commit_cas` as free functions or extension methods on `dyn ProjectStateTransport`. The current shape forces every future implementor (including the mock, which duplicates `hydrate_into` logic) to carry the full trait surface.

**Fit with existing persistence model:** The adapter *preserves* the existing model. The design doc's non-goals (§1, lines 19-25) are correct and the code respects them: no SQLite changes, no hub ref changes, no default-behavior changes. The `StateBackend::Local` path is untouched. The `broker_projection_feeds_existing_state_hydration` test (`tests.rs:271-328`) proves the projection feeds the existing `hydrate_from_state` path without modification. This is the right incremental strategy.

---

## 2. Unnecessary Complexity (Q2)

The diff is +5515/-1. The breakdown:

| Component | Lines (approx) | Justified? |
|---|---|---|
| `client.rs` (HTTP client, envelope parsing, typed errors) | 756 | Mostly yes — the broker v1 envelope handling is genuinely complex. But the `send()` method (lines 649-734) duplicates envelope/error logic that could be a generic helper. |
| `config.rs` (settings, env, hook-config, redaction) | 708 | Partially. The `from_lookup` / `from_env` / `resolve_with` triple is testable but adds surface. The HTTP loopback guard, token file support, and redaction are necessary. |
| `error.rs` (typed codes, redaction, Display) | 405 | Yes — the contract error codes + client-side codes are necessary. |
| `validate.rs` (client-side validation) | 316 | Yes — mirrors the broker's limits to avoid pointless round-trips. |
| `mock.rs` (deterministic in-memory broker) | 441 | Yes — essential for unit tests without network. |
| `transport.rs` (trait + `commit_cas` + `hydrate_into`) | 389 | Partially. `hydrate_into` (lines 140-211) is 70 lines of file I/O that could be a free function. `commit_cas` (lines 233-323) is 90 lines of retry logic that could be a free function. The trait itself is 5 methods + 2 provided. |
| `tests.rs` (unit tests) | 342 | Yes — 7 focused tests. |
| `tests/state_broker_contract.rs` (loopback stub) | 1241 | Partially. The stub itself is ~850 lines. This is the main size contributor and is unavoidable for real HTTP contract testing, but the stub could be extracted to a shared test utility crate if more contract tests are added later. |
| `tests/state_broker_live.rs` | 86 | Yes — minimal live probe. |
| `commands/state_broker.rs` | 160 | Yes — simple CLI entry point. |
| `mod.rs` + re-exports | 74 | Yes. |
| Design doc + handoff + CHANGELOG | ~500 | Standard documentation overhead. |

**Unnecessary machinery identified:**

1. `transport_from_env()` (`transport.rs:368-373`) — a convenience constructor that combines config resolution + client construction. This couples transport construction to the environment, which is fine for the CLI but should not be used by library consumers. Low severity.

2. The `CommitRequest::paths()` method (`client.rs:412-414`) allocates a `Vec<String>` on every call. It's called in the hot `commit_cas` retry loop. Should return an iterator.

3. The `redact_value` recursive JSON walk (`error.rs:325-338`) clones every JSON value tree on every error. For the typical error case (small details object), this is fine but could be avoided by redacting at construction time rather than at every read.

---

## 3. Semantic Risks

### Q5: Disposable projection becoming authoritative

There is **one path** by which this could happen, though it requires misconfiguration:

The `default_projection_dir` (`config.rs:408-410`) writes under `.crosslink/state-projection/`. If a downstream consumer (not in this change) reads from this directory *without* going through the transport, and if the projection is stale (written at time T, broker advanced at time T+1), the consumer sees stale data with no CAS protection.

The design doc correctly labels this a cache (`mod.rs:27-31`), but there is **no runtime guard** (no file lock, no marker file, no staleness check) on the projection directory. The `hydrate_into` method (`transport.rs:175-209`) writes files without any pre-check that the projection is fresh relative to the head it just read. A crash between `read_state` and the completion of `hydrate_into` leaves a partial projection with no indication it is incomplete.

**Risk level:** Low for the current change (no production consumer exists yet), but this MUST be documented as an invariant for future consumers: "always read the head from the transport, never trust the projection directory without verifying freshness."

### Q6: `commit_cas` stale-state reconciliation

The reconciliation (`transport.rs:233-323`) is conservative and correct:

1. On `stale_state`, re-read the head once.
2. If the head's `Broker-Op:` trailer matches our `op_id`, verify content match and return `already_applied: true`.
3. Otherwise re-issue with the fresh head.

**Missing information preservation:** When the final `stale_state` error is returned (retries exhausted, `transport.rs:320`), the error carries only the broker's envelope error. It does **not** carry the list of heads that were observed during the retry loop. A caller doing manual reconciliation needs to know what the head moved to; this information is lost.

Additionally, the `already_applied` branch (`transport.rs:268-314`) sets `previous_head: None` (line 305). This is incorrect — the previous head is the head that existed before our write landed, which was the stale head we read. A caller using `previous_head` to reconstruct history will see a gap.

**Lost update risk:** The `max_retries` parameter controls how many times `stale_state` is tolerated. With `max_retries = 0`, a single conflict is fatal. With `max_retries = 1` (the typical case from `commit_cas`), a competing writer that lands two commits between our read and our retry will still lose. The design does not document the expected retry budget or its semantic consequences.

---

## 4. Per-Agent-Ref vs Whole-Tree-CAS Analysis (Q4)

The fundamental mismatch:

| | Crosslink v3 | Broker v1 |
|---|---|---|
| State shape | N independent per-agent refs (`refs/heads/crosslink/agents/<id>`) + checkpoint ref + meta ref | One project state tree (`refs/heads/projects/<uuid>/state`) |
| CAS granularity | Per-ref (each agent CASes its own ref independently) | Whole-tree (one expected-head for the entire project) |
| Concurrency model | Multiple agents write concurrently, no serialization | All writes serialize through the single state ref |

### Option A: Encode per-agent refs inside the broker state tree

Map the v3 layout into the broker's namespace:
```
agents/<agent-id>/events.log
agents/<agent-id>/heartbeat.json
checkpoint/state.json
meta/hub.json
```

**Pros:** Preserves the v3 logical structure; the projection can feed `RefHubSource` directly; no Crosslink code changes beyond routing.

**Cons:** Every agent write (appending one event) requires a whole-tree CAS commit, serializing all writers. With N agents, write throughput drops by a factor of N. The 32-files/commit and 1-MiB limits may be hit when many agents are active simultaneously. The `commit_cas` retry loop amplifies under contention.

**Semantic consequence:** This turns the hub from a concurrent multi-writer system into a single-writer system. It is correct only if the deployment has at most one active agent at a time (which may be the case for Codex Cloud, but is not stated as an invariant).

### Option B: Extend broker later with finer-grained heads

Add per-agent ref tracking to the broker contract v2.

**Pros:** Preserves concurrency; clean long-term architecture.

**Cons:** Requires broker contract change; out of scope for this task; uncertain timeline.

**Semantic consequence:** The adapter boundary (`ProjectStateTransport`) would need to grow or be replaced. The current trait's five operations assume a single state head; finer-grained heads would need per-agent `commit` + `verify` methods.

### Option C: Compose per-agent state into one project checkpoint

At each write, reduce the full event set and write the resulting `CheckpointState` as the sole file in the broker state tree.

**Pros:** Natural fit for the broker's single-tree model; the checkpoint is already the composition point; `hydrate_from_state` consumes it directly.

**Cons:** Loses per-agent event history (the broker has no delete, so old events accumulate unless the commit replaces the entire tree). Every write requires a full reduce, which is O(total events) — expensive for large hubs. Two agents writing concurrently will serialize through CAS, but each write carries the full reduced state, so the CAS body is large.

**Semantic consequence:** This changes the hub from event-sourced to snapshot-based at the broker boundary. Local agents still have their event logs, but the durable central state is a snapshot. Reconciliation after a split requires re-reducing from the local event logs, which is already how Crosslink works.

### Option D: Move/change the adapter boundary

Instead of mapping v3 refs onto the broker tree, make the broker the *sole* state store and remove local git refs entirely.

**Pros:** Eliminates the ref-vs-tree impedance mismatch; simplest long-term architecture.

**Cons:** Massive redesign; contradicts the "no redesign" constraint; breaks the local-first model; requires all agents to be online to read state.

**Semantic consequence:** This would be a v4 hub redesign, not a transport adapter.

### Recommendation

For the immediate deployment (single agent, Codex Cloud), **Option A** is the pragmatic choice. The serialization cost is acceptable because there is only one writer. The adapter should document that the single-tree CAS model implies single-writer semantics, and that multi-agent concurrent writes through the broker will serialize and may hit the 32-file/1-MiB limits.

For the long term, **Option C** (compose into checkpoint) is the most natural fit because the checkpoint already exists as the composition point and `hydrate_from_state` consumes it directly. The event logs can be retained locally for split-recovery.

---

## 5. Missing Invariants (Q8)

1. **Projection freshness:** No invariant states "the projection directory reflects the head returned by the most recent `read_state`." A crash between `read_state` and `hydrate_into` completion leaves a partial projection. Future consumers must not trust the projection without re-verifying the head.

2. **Single-writer per op_id:** The design doc states "single-writer-per-op-id is assumed for `commit_cas`" (`design/state-broker-transport.md:189`), but this is not enforced or validated. Two processes sharing the same `op_id` would produce incorrect `already_applied` detection — each would see the other's commit as its own if the op_id matches.

3. **CAS retry budget semantics:** The `max_retries` parameter to `commit_cas` has no documented semantic: is it "total attempts minus one"? "Number of conflicts tolerated"? The code treats it as "number of conflicts tolerated" (attempt < max_retries at line 258), but callers must know this to set the budget correctly.

4. **No tombstone support:** The broker v1 has no delete operation. State that must be removed needs a tombstone convention or a broker contract change (`design/state-broker-transport.md:186`). This invariant is documented but not enforced — a caller could attempt to "delete" a file by committing an empty tree, which the broker would reject or produce unexpected results.

5. **`commit_cas` is for whole-file upserts only:** The provided method (`transport.rs:224-226`) documents this restriction but does not enforce it at the type level. An append-style mutation passed to `commit_cas` would be silently wrong — the idempotent replay assumption (re-issuing the same content) does not hold for appends.

6. **No heartbeat/liveness between operations:** The adapter has no mechanism to detect a stale connection or expired token between operations. A long-running process could attempt `commit_cas` after the token has been rotated, receiving a 401 that is not retryable.

---

## 6. Missing Tests (Q9)

The passing suite (41 lib + 8 contract + 7 unit) is thorough for the code present but does not exercise:

1. **Concurrent `commit_cas` from two processes:** The mock's `inject_competing_commit` simulates this in-process, but there is no test with two `MockStateTransport` instances (or two real HTTP clients against the same stub) actually competing. The mutex-based mock cannot produce true interleaving.

2. **`hydrate_into` partial failure:** If `read_blob` fails on file 3 of 5, the projection has 2 files written. There is no test for this scenario or for the expected recovery behavior (delete the partial projection? retry from scratch?).

3. **`commit_cas` retry exhaustion with content mismatch:** The test `commit_cas_exhausts_retries_with_a_typed_stale_error` (`tests.rs:190-216`) uses `fail_next_commit` to inject a non-stale error on the retry. But there is no test where retries are exhausted purely through `stale_state` conflicts (each retry sees a new head that still conflicts).

4. **`verify` at a non-head commit against the real client:** The contract test's stub now supports historical commits (after the prior review fix), but the test `verify_and_blob_read_support_historical_commits` (`state_broker_contract.rs:958-1000`) only verifies a non-head commit for *reads*, not for *CAS commits*. The interaction between historical reads and CAS is untested.

5. **Token expiry mid-operation:** No test verifies behavior when the broker returns 401 in the middle of a `commit_cas` retry loop.

6. **Large payload handling:** The 1-MiB commit limit and 256-KiB file limit are validated client-side but not tested against the broker boundary (e.g., a file exactly at the limit).

7. **`message_records_op` with multi-line messages containing `Broker-Op:` in non-trailer lines:** The test (`transport.rs:380-388`) covers the basic case but not a message where `Broker-Op:` appears mid-line (e.g., `"note: we use Broker-Op: foo"`), which should NOT match because the line does not consist solely of `Broker-Op: <op_id>`.

8. **`commit_cas` when `read_state` fails during reconciliation:** If `read_state` returns an error after a `stale_state` (line 259), the error from `read_state` masks the original `stale_state`. No test covers this path.

---

## 7. Recommended Next Step (Q10, Q11)

**Q10: `SyncManager` wiring — design decision required first.**

The `SyncManager` should NOT be wired to the broker transport after the current live smoke test. The design doc correctly identifies this (`design/state-broker-transport.md:156-169`): the per-agent-ref layout of v3 has no natural mapping onto the broker's single state tree, and the mapping decision (Options A-D in Q4) fundamentally changes the hub's concurrency model. Wiring `SyncManager` without this decision would commit Crosslink to a concurrency model that may not be correct for multi-agent deployments.

The live smoke test (`crosslink state-broker status`) should proceed — it exercises only read operations and confirms the HTTP/auth/envelope path. But `SyncManager` wiring requires:
1. A decision on the mapping strategy (Q4).
2. A load test with realistic agent counts to validate the CAS contention model.
3. A concurrency test proving that two agents writing through the broker do not lose events.

**Q11: Smallest next architectural step.**

Write a **mapping design document** that:
1. Defines the exact file layout under `projects/<uuid>/state/` for the v3 hub contents.
2. Specifies the CAS commit strategy (one commit per agent write? batch?).
3. Proves that `hydrate_from_state` (or a new adapter-specific hydrator) can reconstruct a `RefHubSource`-compatible view from the projection.
4. Analyzes the concurrency implications (serialization, retry amplification, 32-file limit) for 1, 2, 5, and 10 concurrent agents.
5. Identifies the minimal `SyncManager` interface changes needed (if any) to route through `ProjectStateTransport`.

This document should be reviewed before any `SyncManager` wiring code is written.

---

## 8. Confidence and Main Uncertainty

**Confident about:**
- The adapter boundary (`ProjectStateTransport`) is correctly located between persistence consumers and the wire protocol.
- The existing persistence model is preserved (no SQLite changes, no default-behavior changes).
- Secret handling is sound (redaction, `pub(crate)` token exposure, loopback-only HTTP).
- The `commit_cas` reconciliation logic is correct after the prior review's fix (content comparison in `already_applied`).
- The contract types match the broker v1 wire format (envelope, error codes, limits).
- The `validate_message` C1 control fix (matching the broker's exact rule) is correct.

**Uncertain about:**
- Whether the 32-file/1-MiB limits are sufficient for a realistic v3 hub with multiple agents. A hub with 10 agents, each with a 100-KiB event log, heartbeat, and requests-ack subtree would need 30+ files per commit, exceeding the limit. This is untestable without a real deployment.
- Whether the broker's fast-forward-only ref update (`force: false` in `state.ts:390`) will interact correctly with the CAS retry model under actual Cloudflare Worker concurrency (the broker is stateless; the GitHub API is the serialization point).
- Whether the `pseudo_git_sha` function (`digest.rs:17-21`) produces identifiers that are sufficiently git-shaped for mock consumers. It truncates SHA-256 to 40 hex chars, which is not a valid git blob SHA (git uses SHA-1). This is fine for mocks but would break any code that tries to use these identifiers with real git plumbing.

**What would change my mind:**
- If a live broker test revealed that the envelope format diverges from the contract (e.g., different field names in the `result` object), the adapter's deserialization would break silently.
- If the broker's tree read (`state.ts:224`) truncates a large hub state tree, the adapter would receive a partial inventory with no retry mechanism (the broker throws `upstream_error` for truncated trees, which is correct, but the adapter does not handle this specifically).
- If the 256-KiB file limit is hit by a real event log, the adapter would fail at commit time with no graceful degradation.

---

## 9. Answers Table

| Q | Answer |
|---|---|
| Q1 | `ProjectStateTransport` is the correct location but over-abstracted; `hydrate_into` and `commit_cas` should be free functions, not trait methods. |
| Q2 | ~5515 lines: ~2400 in new module code, ~1300 in contract tests, ~500 in mock, ~500 in docs. The loopback stub (1241 lines) is the main size driver and is justified but could be shared. `hydrate_into` and `commit_cas` as trait methods add unnecessary trait surface. |
| Q3 | The adapter preserves the existing model. `StateBackend::Local` is untouched; `hydrate_into` feeds the existing `hydrate_from_state` path; no SQLite changes. |
| Q4 | Option A (encode per-agent refs in tree) is pragmatic for single-agent; Option C (compose checkpoint) is better long-term. Option B (finer-grained heads) requires broker v2. Option D is a v4 redesign. |
| Q5 | One path: a downstream consumer reads the projection directory without re-verifying freshness via the transport. No runtime guard exists. |
| Q6 | `commit_cas` is conservative and correct. `previous_head: None` in `already_applied` is incorrect. Retry exhaustion error loses observed heads. |
| Q7 | Git coupling: `pseudo_git_sha` is mock-only. Broker v1 coupling: envelope parsing hardcodes v1 format. v3 layout coupling: `hydrate_into` assumes `checkpoint/state.json` path convention. |
| Q8 | Six missing invariants: projection freshness, single-writer per op_id, CAS retry budget semantics, no tombstone support, whole-file-upsert-only for `commit_cas`, no heartbeat between operations. |
| Q9 | Eight untested behaviors: concurrent multi-process CAS, `hydrate_into` partial failure, retry exhaustion pure stale_state, historical CAS, token expiry mid-operation, large payload boundary, `message_records_op` edge cases, `read_state` failure during reconciliation. |
| Q10 | Design decision required first. Live smoke test (reads only) should proceed; `SyncManager` wiring must wait for the mapping strategy decision. |
| Q11 | Write a mapping design document specifying the v3-to-broker-tree layout, CAS strategy, hydration reconstruction, concurrency analysis, and minimal `SyncManager` interface changes. |
