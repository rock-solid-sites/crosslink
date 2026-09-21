---
artifact: panel review
reviewer: 5
requested_model: Muse Spark 1.3
model_id: opencode-go/muse-spark-1.3-contributor
provider: OpenCode Go
variant: high
privacy: contributor tier — prompts/completions may train future Meta models; not ZDR (sanitized packet used)
started: 2026-09-21T04:25:14Z
finished: 2026-09-21T04:28:14Z
tokens_total: 731590
cost_usd: 0.018004192000000002
session_id: ses_f3dc9d00affeacc0ABhe49Q6Up
extraction: opencode export
---

# Raw reviewer output — Muse Spark 1.3

# Independent architecture review — `feature/pp3g-state-broker-adapter` (issue #802)

Read-only review from the frozen packet. No live broker operations performed. Citations are `file:line` into the packet. Anything not read is marked **[unverified]**.

## 1. Architecture assessment

**Verdict: cleanly-built adapter to the wrong-shaped seam — safe to land as dead-end-free scaffolding, unsafe to wire into production without a prior mapping decision.**

**Observation.** The change introduces `ProjectStateTransport` (`src/state_broker/transport.rs:79-117`) with five methods mirroring broker v1 ops 1:1 (`read_state`, `read_blob`, `verify`, `commit`, plus provided `hydrate_into` at `transport.rs:140-211` and `commit_cas` at `transport.rs:233-324`), backed by a blocking HTTP client (`src/state_broker/client.rs:462-465`), config/backend selection (`src/state_broker/config.rs:312-317`), validators (`src/state_broker/validate.rs`), typed errors (`src/state_broker/error.rs`), an in-memory mock (`src/state_broker/mock.rs:32-34`), a 1241-line loopback-stub contract suite (`tests/state_broker_contract.rs`), and a read-only CLI (`src/commands/state_broker.rs` per design doc §4).

**Observation.** The patch is +5515/−1 across 22 files (`diff/stat.txt:23`), touching existing code only to register the module and command (`diff/modified-existing-files.patch:61-138`: `lib.rs`, `main.rs`, `commands/mod.rs`, plus `Cargo.toml` adding reqwest `blocking`). Default behavior is provably preserved: `StateBackend::resolve_with` returns `Local` for absent/unknown-unset selection (`src/state_broker/config.rs:358-359`), and broker env vars alone do not switch backends (regression-tested at `src/state_broker/config.rs:651-663`).

**Interpretation.** As a *broker client*, this is competent work: envelope handling (`src/state_broker/client.rs:700-734`), no blind retries on `commit` (`src/state_broker/client.rs:587-592`), digest-checked blob decode (`src/state_broker/client.rs:201-223`), secret redaction (`src/state_broker/config.rs:299-304`, `src/state_broker/error.rs:316-338`), and a prior adversarial round whose fixes I re-verified in the code (digest-compared reconciliation at `transport.rs:268-299`; `pub(crate)` token exposure at `config.rs:82`; exact C0/DEL message rule at `validate.rs:111-114` matching `broker/state.ts:487`).

**Interpretation.** As a *persistence seam*, the abstraction faces the wrong direction: it names what the broker sells (whole-tree file CAS), not what Crosslink needs (per-agent-ref appends, checkpoint adoption, fetch/join). No existing caller can use it without the §4 mapping decision the design doc explicitly defers (`design/state-broker-transport.md:160-169`). That deferral is correct, but it means Q1's answer is "no" — and every semantic risk below flows from that gap, not from implementation sloppiness.

**Fit with the persistence model.** Preserved today (nothing routes through it), compromised the moment it is wired under any whole-tree mapping without resolving §4. The design doc's claim that adopting option (a) later is "an adapter implementation plus call-site routing — no redesign" (`design/state-broker-transport.md:171-173`) understates the concurrency-model change it admits two paragraphs earlier (`design/state-broker-transport.md:160-169`). Those two claims are in tension; the second one is true.

## 2. Unnecessary complexity (Q2)

Where the +5515 comes from (`diff/stat.txt`): ~3.4k lines of new `state_broker/` module (client 756 + config 708 + mock 441 + transport 389 + validate 316 + error 405 + tests 342 + digest 35 + mod 74), ~1.24k lines of stub contract test, ~311 lines design doc, plus handoffs/docs. Removable without sacrificing correctness:

1. **Two fakes, one contract.** `mock.rs` (in-memory transport) and the `StubBroker` HTTP stub (`tests/state_broker_contract.rs:30-56`) both reimplement broker CAS semantics. Worse, the *client-side* validators (`validate.rs`) are reimplemented a **third** time inside the stub (`tests/state_broker_contract.rs:730-846`: `stub_sha_ok`, `stub_path_ok`, `stub_message_ok`, `stub_op_id_ok`, `stub_validate_commit_body`). Keep the mock *or* the stub, not both; if the stub is kept, it should import shared limit constants instead of re-deriving them.
2. **Stale mock fidelity gap left behind.** The stub was fixed to serve historical commits (`tests/state_broker_contract.rs:399-431`, `445-447`), but the mock was not: `mock.rs:222-228` (`read_blob` 404s unless `at == head`) and `mock.rs:257-261` (`verify` 404s unless `commit == head`) still contradict the real broker, which reads any commit tree (`broker/state.ts:473-476`, `302`). The mock also skips duplicate-path and 32-path checks (`mock.rs:245-284`). Either bring the mock to parity or delete it and test against the stub.
3. **`digest.rs` (35 lines)** is two thin wrappers over `sha2`/`hex`. Fold into `client.rs` or `validate.rs`.
4. **Doc-comment weight.** The module carries book-quality rustdoc (e.g. `transport.rs:1-33`, `config.rs:1-27`, `client.rs:1-17`). Valuable, but a large fraction of the line count is prose restating the design doc. Not a defect; it is where much of the +5515 lives, and reviewers should not mistake size for surface area.
5. **Necessary size, kept:** the typed-error taxonomy (`error.rs`), the `commit`/`commit_cas` split, redaction plumbing, and the `hydrate_into` traversal guard (`validate.rs:206-218`). These are load-bearing.

## 3. Semantic risks (Q5, Q6)

### Q5 — paths by which a disposable projection becomes authoritative

**Observation.** `hydrate_into(dir, paths)` accepts *any* caller-chosen `dir` with no disjointness constraint (`src/state_broker/transport.rs:140-144`), and the default root sits inside the Crosslink dir (`src/state_broker/config.rs:408-410`). The only executable "disposability" proof is a test that deletes the dir and re-reads the head (`src/state_broker/tests.rs:46-50`) — it proves the mock still serves, not that no future caller treats the dir as source.

Concrete escalation paths:

- **P1 — projection pointed at the hub cache worktree.** Nothing in the trait prevents `hydrate_into(cache_dir, …)`. From there, the destructive v2 path `hydrate_to_sqlite` (which reads `cache_dir/issues/`, `v3/hydration-excerpts.rs:104-134`) would ingest broker files as if they were coordination-branch truth. The fail-closed dispatcher (`v3/hydration-excerpts.rs:188-211`) guards *hub-version* confusion, not *directory-identity* confusion.
- **P2 — checkpoint-blob hydration bypasses the event model.** The projection test feeds a broker blob straight into `hydrate_from_state` (`src/state_broker/tests.rs:317-328`), which clears and reinserts SQLite guarded only by an empty-state early return (`v3/hydration-excerpts.rs:364-366`). A stale or partial broker checkpoint (e.g. one file of many, or an older head) would wipe newer SQLite rows the merge logic cannot see. The test proves mechanical compatibility, not freshness/ordering equivalence — watermarks, `OrderingKey` reduction, and per-agent logs are all skipped.
- **P3 — silent backend confusion in the other direction.** `read_hook_config_backend` returns `None` (→ `Local`) on unparsable JSON with only a warning (`src/state_broker/config.rs:473-484`). An operator who believes they selected the broker while the file is corrupt gets durable writes to the *wrong backend silently* — directly contradicting the "no silent fallback" claim (`design/state-broker-transport.md:119-121`). **Recommendation:** hard-error on present-but-unparsable config when any broker env/selection key exists, or at minimum surface it in `state-broker status`.
- **P4 — unbounded full-inventory hydration.** `hydrate_into` with `paths: None` materializes the entire inventory with no size/count cap (`src/state_broker/transport.rs:167-173`). A large tree can exhaust disk in the Crosslink dir. Minor; add a cap or stream with a budget.

### Q6 — `commit_cas` reconciliation

**Confirmed fixed** (prior review finding 1): the `already_applied` branch now compares `sha256`/`size` against intended payload (`src/state_broker/transport.rs:268-299`), with a regression test for op-id reuse (`src/state_broker/tests.rs:147-187`).

**Remaining gaps, each a real lost-update or false-confidence path:**

- **G1 — overlapping-path clobber on rebase.** On `stale_state` without our op-id, `commit_cas` re-issues the *same whole-file upserts* against the fresh head (`src/state_broker/transport.rs:317`). For disjoint paths this merges (the only case tested: `src/state_broker/tests.rs:78-107` asserts the competitor's file survives). For **overlapping paths** the retry silently overwrites the competitor's bytes — a last-writer-wins lost update with `verified: true`. The doc comment warns "only safe for whole-file upserts" (`transport.rs:224-226`) but an upsert is exactly what destroys a concurrent same-path write. **Recommendation:** require the retry path to re-read and compare same-path bytes, or return the conflict to the caller when paths intersect the intervening commit.
- **G2 — fabricated `previous_head: None`.** The `already_applied` outcome synthesizes `previous_head: None` (`src/state_broker/transport.rs:305`) when there *was* a previous head. Any consumer using `previous_head` for provenance/lineage gets a lie. Use the observed pre-head or `Option` semantics honestly.
- **G3 — verify-after-read TOCTOU.** `commit_cas` reads state, then verifies at that head (`transport.rs:259-281`). If the head moves between those calls, `verify` (pinned to the old sha) still matches and returns `already_applied: true` pointing at a **superseded commit** while a newer head exists. The caller believes its write is current. Re-check `current_head` after `verify`, or return the observed head alongside the verdict.
- **G4 — narrow verify scope.** Reconciliation verifies only `request.paths()` (`transport.rs:281`); sibling files changed by others in the landing commit are invisible to the verdict. Acceptable for the idempotence question, but `verified: true` overstates "the head is exactly what I intended."
- **G5 — error-path opacity.** If `read_state`/`verify` fails inside reconciliation, the error propagates indistinguishably from a write failure, even though the write may have landed (`transport.rs:259,281` use `?`). Callers cannot tell "landed but unverifiable" from "never sent." A typed variant would close this.

## 4. Per-agent-ref vs whole-tree-CAS analysis (Q4)

The mismatch, stated plainly: Crosslink v3 concurrency is **per-agent-ref fast-forward + watermark-adopted checkpoint** (`v3/hub_v3-excerpts.rs:38-50`, `push_agent_ref` at `:227-231`, adoption at `v3/sync-cache.rs:432-473`), while broker v1 serializes **all writers through one ref's CAS** (`broker/state.ts:328-339`, `broker/README.md:93-99`). These are different linearization granularities, and no adapter code can make whole-tree CAS behave like per-ref CAS — it can only choose where the serialization cost and the semantic distortion land.

- **A. Encode per-agent refs inside the broker tree** (design §4 option (a): `agents/<id>/events.log`, `checkpoint/state.json`). *Semantics:* every agent append becomes a whole-tree commit; all agents contend on one CAS, so throughput collapses to single-writer and `commit_cas` retries become the normal path, not the exception. Worse, G1 above turns cross-agent appends into same-tree conflicts resolved by blind re-issue — event loss by overwrite unless the adapter re-reads/merges per file. Append-style event logs are precisely the workload the trait says must not use `commit_cas` (`transport.rs:224-226`), yet option (a) forces them through it. Chunking for the 32-file/256 KiB/1 MiB limits (`broker/state.ts:25-32`) further fragments one logical append into many CAS rounds, widening the race window. **Verdict: semantically hostile to the event-sourced model; do not choose.**
- **B. Extend the broker with finer-grained heads** (per-path or per-prefix CAS). *Semantics:* the only option that preserves per-agent independence — each agent's prefix gets its own compare-and-swap, matching `CasExpectation` per ref (`v3/hub_v3-excerpts.rs:62-70`). Requires a broker contract bump (new error/variant surface, prefix-scoped `expected_head`), broker-side migration, and Crosslink client changes — but it moves complexity to the component that owns the constraint. **Verdict: the coherent long-term fix; correctly deferred, not correctly avoided forever.**
- **C. Compose per-agent state into one project checkpoint** (ship only reduced `CheckpointState`, not event logs). *Semantics:* this is what the projection test actually proves (`src/state_broker/tests.rs:271-328`). It converts the broker from a *journal* into a *snapshot store*: readers get `hydrate_from_state` compatibility, but writers lose per-event attribution, `OrderingKey` replay, prune safety (`compact_v3` prune invariant at `v3/hub_v3-excerpts.rs:554-567`), and the deterministic-reduce story. Concurrent compositors race on the snapshot (REQ-7's "identical content" benign-race argument only holds if both reduced the *same* event set — unguaranteed once agents write outside the broker). Tombstones/deletes have nowhere to go (broker has no delete op; design doc §4 admits this). **Verdict: viable as a read/cache tier, not as the durable journal.**
- **D. Move/change the adapter boundary.** *Semantics:* stop mirroring broker ops and instead expose Crosslink-domain operations (append events for agent X; publish checkpoint; fetch-and-adopt), with the broker mapping hidden inside. Reads could implement `HubSource` over broker blobs (the trait is read-shaped and pin-friendly: `v3/hub_source.rs:49-83`, `RefHubSource` pinning at `:430-447`); writes stay on git until option B lands. This makes the "second model" problem explicit and keeps `SyncManager` as the single routing point (`v3/sync-cache.rs:332-354` `fetch` dispatch). **Verdict: the correct next architectural move — narrow the Crosslink-facing seam, keep the broker client as an internal detail.**

## 5. Missing invariants (Q8)

1. **No-delete invariant.** Broker v1 has upsert-only commits (`broker/state.ts:328-431`; design doc §4). There is no tombstone convention, no prune path, and no test for deletion — yet Crosslink has deletion/tombstone semantics (`deleted_issues` in `v3/checkpoint.rs:29-34`, prune in `hub_v3-excerpts.rs:554-567`). Unstated: "broker state grows monotonically; deletes are out of scope."
2. **Projection freshness invariant.** Nothing binds a projection to the head it came from at consumption time (`ProjectionReport.commit` is advisory, `transport.rs:50-60`). No consumer checks it. Stated nowhere that `hydrate_from_state` must only consume projections whose commit equals `current_head`.
3. **Single-writer-per-op-id.** Assumed by `commit_cas` (design §4) but unenforced: op-ids are caller-chosen strings (`src/state_broker/validate.rs:137-150`), and reuse across payloads is only *detected*, never *prevented*.
4. **Whole-tree serializability.** The adapter never states that two concurrent `commit_cas` loops on overlapping paths lose one writer's bytes (G1). The trait docs should state last-writer-wins explicitly or forbid the pattern.
5. **Backend-identity invariant.** No check that the broker's `project_uuid`/`state_ref` matches local project identity beyond config plumbing (`config.rs:271-292` helpers exist but no cross-check against the Crosslink project is shown in the packet).
6. **Checkpoint/event coherence.** Nothing requires that a broker-hosted `checkpoint/state.json` be consistent with broker-hosted event logs (or even that both live in the broker). The projection test writes both from one source (`tests.rs:309-315`) and never tests skew.
7. **Message-trailer trust boundary.** `message_records_op` (`transport.rs:330-335`) trusts broker-reported messages; sound today (broker owns trailers, `broker/state.ts:598-613`, caller spoofing rejected at `broker/state.ts:493-497`), but the invariant "never treat a caller-supplied string as trailer evidence" is documented only in a comment.
8. **Timeout/retry budget.** `commit_cas(max_retries: u8)` bounds CAS attempts but each attempt is a full HTTP round-trip under `DEFAULT_TIMEOUT_MS` (`config.rs:63`); no total-deadline invariant. Minor.

## 6. Missing tests (Q9)

Despite green suites (design §7 table), the following behavior is untested:

1. **Overlapping-path concurrent commit** through `commit_cas` (G1) — only disjoint paths are exercised (`tests.rs:78-107`).
2. **Superseded-head replay** (G3) — no test moves the head between `read_state` and `verify` in the `already_applied` path.
3. **Verify-fails-during-reconciliation** (G5) — injected `fail_next` covers `commit` only (`mock.rs:289-291`); no failure injection for `read_state`/`verify`.
4. **Any live broker call** — `tests/state_broker_live.rs` is ignored by design; the packet records zero live reads *and* writes. All "contract conformance" evidence is stub/mock-derived. The `upstream_error` 502 shape from a real broker is exercised only via injected envelopes (`tests.rs:233-261`, stub flag at `contract.rs:314-333`).
5. **Boundary-limit traffic over the wire** — 256 KiB/file, 1 MiB total, 32 files, 512-char message, 32 verify paths are unit-validated locally but never sent through the HTTP client against the stub (largest payloads in tests are bytes-long).
6. **Delete/tombstone round-trip** — no delete op exists; consequently nothing tests hub prune or issue deletion through the broker, though these are core hub operations.
7. **Multi-file atomicity under conflict** — no test lands a competing commit touching *some but not all* of the retried request's paths and checks per-file outcomes.
8. **Config failure modes** — invalid-JSON `hook-config.json` silent-`None` (P3), unreadable token file, `state_backend` non-string error path (`config.rs:495-498` — the error branch has no shown test), non-loopback `http` over `transport_from_env`.
9. **Full hub-tree round-trip** — admitted in the design doc (`design/state-broker-transport.md:191`); only the single-checkpoint projection path is proven.
10. **`SyncManager` routing** — none, by design. Any future wiring PR must add fetch/adopt/push parity tests against the v3 suite.

## 7. Recommended next step (Q10, Q11)

**Q10: a design/invariant decision is required first — do not wire `SyncManager` after the smoke test.** The live smoke test (`design/state-broker-transport.md:277-295`: `state-broker status` + ignored live probe) can only confirm transport conformance (envelopes, auth, head readability). It answers nothing about §4: ref→tree mapping, delete convention, or the G1 lost-update semantics. Wiring `fetch_and_adopt_v3_refs` (`v3/sync-cache.rs:432`) or any `push_*` path (`hub_v3-excerpts.rs:227-303`) to whole-tree CAS before those are decided would silently change the hub's concurrency model — the exact outcome the task's "do not redesign" constraint forbids.

**Q11 — smallest next architectural step (not implemented):** run the read-only live probe to ground the contract evidence, *then* freeze a one-page mapping decision record before any write path exists. Concretely:

1. Operator runs `state-broker status --json` + the ignored live test (reads `health`/`whoami`/`state`, zero writes) and records head/baseline/registry observations.
2. Write the decision record choosing among Q4 options (my recommendation: D now, B as the tracked broker evolution, C explicitly read-tier-only, A rejected with G1 cited), including the delete/tombstone convention and the overlapping-write rule (merge vs. refuse).
3. Only then: a first *single-checkpoint-file* `commit_cas` write with an overlapping-path regression test (G1) as the gate for any `SyncManager` routing.

Total new code for step 1–2: zero. That is the point.

## 8. Confidence and main uncertainty

- **Confident:** trait/client shape and line-level claims (read all of `transport.rs`, `client.rs`, `config.rs`, `error.rs`, `validate.rs`, `mock.rs`, `digest.rs`, `mod.rs`, `tests.rs`, the stub, `broker/{README,state,paths,errors,app}.ts`, design doc, both handoffs, `stat.txt`, both patches, `commits.txt`, and the v3 excerpts cited). The prior review's five findings are fixed in the code as claimed — I re-verified each rather than trusting the disposition record.
- **Could not verify (marked where used):** anything outside the packet — actual `SyncManager` construction/callers beyond excerpts, `is_windows_reserved_name`, the live broker's behavior, test-suite results (design §7 table taken as reported), and whether `state_backend` registration in config registry/TUI has since landed. I did not run any commands (hard rule).
- **What would change my mind:** (a) a live read-only probe transcript confirming envelope shapes against the deployment — would collapse the largest evidence gap; (b) a mapping decision record showing per-agent appends surviving concurrent whole-tree CAS without G1 loss — would rehabilitate option A; (c) demonstration that `hydrate_from_state` consumers always pin `ProjectionReport.commit == current_head` — would retire P2.

## 9. Answers table (Q1–Q11)

| Q | One-line answer |
|---|---|
| Q1 | No — `ProjectStateTransport` mirrors broker v1 ops instead of naming a Crosslink-domain seam; the coherent boundary is Crosslink-shaped (option D: HubSource-style reads + SyncManager-routed writes). |
| Q2 | +5515/−1 is ~3.4k lines module + ~1.2k stub + docs; remove one of the two fakes, the stub's third copy of validators, `digest.rs`, and prose weight. |
| Q3 | Preserved today by non-wiring (default `Local`), but option-(a) wiring would introduce a competing whole-tree model that checkpoint-only tests do not legitimize. |
| Q4 | A serializes all agents (reject: G1 event loss); B (per-prefix CAS) is the coherent fix; C is read-tier-only; D (re-shape the seam) is the correct next move. |
| Q5 | Yes — unconstrained `hydrate_into` dir (cache-worktree poisoning), checkpoint-direct SQLite wipe, silent-`Local` on corrupt config, unbounded inventory hydration. |
| Q6 | Idempotence fixed via digest compare, but overlapping-path rebase loses writes (G1), `previous_head` is fabricated, and verify-after-read can vouch for a superseded head. |
| Q7 | Git: sha/ref string formats duplicated across config/client/mock/stub; broker v1: envelopes+limits hardcoded in three places; v3: checkpoint path/schema baked into tests. |
| Q8 | Missing: no-delete rule, projection freshness binding, op-id uniqueness, whole-tree serializability statement, backend-identity check, checkpoint/event coherence, retry budget. |
| Q9 | Overlapping-path CAS, superseded replay, reconcile-time read failures, any live call, wire-level limits, deletes, partial-overlap atomicity, config failure modes, full-tree round-trip. |
| Q10 | Design decision first — the smoke test proves transport only and cannot authorize `SyncManager` wiring. |
| Q11 | Zero-code step: run the read-only live probe, then freeze the §4 mapping decision (D now / B later / A rejected) with an overlapping-write gate test before any write. |
