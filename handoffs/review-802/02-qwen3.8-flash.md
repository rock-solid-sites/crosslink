---
artifact: panel review
reviewer: 2
requested_model: Qwen 3.8 Flash
model_id: opencode-go/qwen3.8-flash
provider: OpenCode Go
variant: xhigh
privacy: 0-day retention, not used for training
started: 2026-09-21T04:25:14Z
finished: 2026-09-21T04:38:05Z
tokens_total: 4108148
cost_usd: 0.122723
assistant_messages: 32
finish_reasons: ['tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'tool-calls', 'stop']
session_id: ses_f3dc9d543ffe2smEMB30himfzi
extraction: raw events (export unavailable)
---

# Raw reviewer output — Qwen 3.8 Flash

All evidence read. Composing the review document now.


The hard rules forbid creating files — removing the scratch copy and returning the document inline instead.


# Independent clean-room architecture review — issue #802 (`feature/pp3g-state-broker-adapter`)

Reviewer: panel member 2 (independent). Read-only; no tests were run; no live broker contact. All claims cite packet paths; `src/...` = `src/state_broker/...` unless noted. Anything not verified is marked.

---

## 1. Architecture assessment

**Verdict: the adapter is a well-executed broker-v1 client, and the scope discipline (opt-in backend, no hub redesign, disposable projection) is correct. But `ProjectStateTransport` is not an abstraction of Crosslink persistence — it is broker v1 renamed — and the packet's central unresolved mismatch (Q4) is a concurrency-model mismatch that no encoding choice erases. The design record itself says this (§4, design/state-broker-transport.md:147-173) and defers the decision; that deferral is the right call, and the current change should not be wired into `SyncManager` as-is.**

Observations supporting the verdict:

- The recon (§2, design doc:27-43) is accurate against the v3 excerpts: no existing boundary spans read + mutation (`HubSource` is read-only, v3/hub_source.rs:49-86; `hub_v3` CAS is git-plumbing-specific, v3/hub_v3-excerpts.rs:770-790; `SyncManager` *is* the git transport, v3/sync-cache.rs:432-473). Introducing one new trait rather than refactoring the hub is the minimum change.
- The persistence model is preserved today: nothing in the adapter writes git refs or SQLite (`hydrate_into` only writes a caller-chosen directory, transport.rs:140-211; the only production call site is the read-only status command, src/commands/state_broker.rs:26-116). Default behavior is unchanged (config.rs:337-374; broker env alone does not select the backend, config.rs:650-663 — matching the Codex Cloud environment contract, broker/CODEX-CLOUD.md:9-13).
- The prior review record (handoffs/802-review-hy3.md) was independently checked: its major finding (presence-based `verified`) is genuinely fixed (transport.rs:268-300 compares `sha256`/`size` against the intended payload, with a regression test at src/state_broker/tests.rs:147-187); the `expose()` nit is fixed (`pub(crate)`, config.rs:82); the stub-fidelity minors are fixed (commit→snapshot history + independent validators, tests/state_broker_contract.rs:399-408, 727-846). I also confirmed its clean verdicts on envelope handling (client.rs:690-733 vs broker/app.ts:340-398) and path safety (validate.rs:58-87 vs broker/paths.ts:30-61).
- I found **one real defect the prior review missed** and several unclosed semantic gaps (§3 below).

The one thing I would not endorse: the naming and doc claims that present the trait as *the* persistence seam ("the narrow persistence seam", mod.rs:10; "Semantic durable-state operations Crosslink requires", transport.rs:75-79). The trait's entire vocabulary — 40-hex commit shas, whole-tree `expected_head`, `Broker-Op:` trailer reconciliation, exact-commit `verify` — is broker v1's. Calling it Crosslink's abstraction pre-canonicalizes the very decision §4 says must still be made.

---

## 2. Unnecessary complexity (Q2)

Where +5515/-1 comes from (diff/stat.txt; sums verified against file line counts):

| Bucket | Lines | Notes |
|---|---|---|
| New production modules (client, config, error, transport, validate, mock, mod, digest, command) | ~3,284 | client 756, config 708, error 405, mock 441, transport 389, validate 316, mod 74, digest 35, command 160 |
| Tests | ~2,119 | contract 1,241 (of which ~880 is stub server + reimplemented validators), tests.rs 342, live 86, plus ~450 inline `#[cfg(test)]` in config/validate/mock/error |
| Docs/handoffs/lockfile/CHANGELOG | ~560 | design 311, handoffs 184, lock 11+2, changelog 11, docs 20 |
| Edits to existing files | ~37 | modified-existing-files.patch |

The size is dominated by *test scaffolding and a third parallel implementation of the broker's rules*, not by the client itself.

Specific removal/reduction candidates (recommendations, not patches):

1. **Triplicated contract rules.** The same validation logic exists three times: client-side (validate.rs:35-150), the stub's "independent" copy (contract.rs:730-846), and the mock via `request.validate()` (mock.rs:287). The stub copy was added to satisfy the prior review's fidelity demand (802-review-hy3.md:64-65) — reasonable motivation, but the cheaper design is golden fixtures captured from the broker's own TS tests (broker/test-state.test.ts, broker/test-api.test.ts) replayed by a thin stub. That deletes ~120 lines of validators that must now be manually kept in sync with `state.ts` — a *new* drift surface the design doc's own sync warning (validate.rs:4-8) now applies to twice as many copies.
2. **Hand-rolled HTTP/1.1 server** (contract.rs:179-273, 848-875): ~190 lines of socket parsing, `find_subslice`, `percent_decode`. Defensible as a no-new-dev-dependency choice, but it is the single largest scaffolding item.
3. **Dead or unconsumed public API.** `transport_from_env()` (transport.rs:368-373) has zero callers and zero tests (grep across packet src/tests; design doc:143 advertises it). `BrokerErrorCode::ALL` (error.rs:64-77) has no consumer (the round-trip test uses `CONTRACT_CODES`, error.rs:349-358). `StateBlob::text()` (client.rs:230-234) is test-only. `pseudo_git_sha` is test-only but ships in the lib's public `digest` module (digest.rs:13-21).
4. **Speculative selection machinery.** `StateBackend` (config.rs:311-401) is consumed only by the read-only status command; nothing routes a write or read path through it. The hook-config registration is deliberately withheld (design doc:193-195), so half the selection surface is unreachable in production. It is cheap, but it *is* machinery ahead of its wiring.
5. **Mock/stub overlap.** `MockStateTransport` (441 lines) and the stub's history/injection hooks model the same CAS semantics at two layers; the mock additionally diverges from the broker (its `verify` rejects non-head commits, mock.rs:256-261, while the real broker reads any commit, broker/state.ts:302) — so it is neither a faithful broker nor a trivial fake. One of the two layers, made faithful, would carry the suite.
6. **Lib+bin double module tree** forces the `#[allow(dead_code)]` / `#[allow(unused_imports)]` re-export block (mod.rs:52-74; main.rs patch lines 93-97). Pre-existing repo pattern, but it inflates every new module.

None of 1-6 sacrifices correctness; each is removable or shrinkable. The *irreducible* core (typed errors, redaction, client, validate, reconcile) is proportionate.

---

## 3. Semantic risks (incl. Q5, Q6)

### 3.0 A concrete client-side panic (new finding, not in the prior review)

`validate_message` slices by byte length after only a char-count check: `trimmed[..trailer.len()]` (validate.rs:119-127). For a message beginning with multi-byte chars straddling the trailer-length boundary (e.g. `"Broker:"` against `"éééé"` — 8 bytes, byte 7 is not a char boundary), this **panics** before `eq_ignore_ascii_case` runs. Reachable from any `CommitRequest::validate()` → `client.commit()` (client.rs:366-370, 600-601) with attacker- or user-influenced message text. The identical pattern is duplicated in the stub (contract.rs:771-777). The broker (TS regex, state.ts:493) cannot panic. No test uses a non-ASCII message (validate.rs:274-287). Fix direction: `trimmed.as_bytes()` prefix compare or `get(..)` with a fallback.

### Q5 — paths by which the disposable projection becomes authoritative

The "disposable" claim (mod.rs:26-31, transport.rs:27-32) is true of the *adapter's* code but is not enforced anywhere in the surrounding system:

1. **Partial projections are indistinguishable from complete ones.** `hydrate_into` writes file-by-file with non-atomic `std::fs::write` (transport.rs:188-209); a mid-loop `read_blob` failure leaves a truncated tree and returns `Err`, but nothing marks the directory incomplete, and the head commit the projection corresponds to is never written into it (only the in-memory `ProjectionReport` carries it, transport.rs:49-60). Any future consumer that checks directory existence rather than the report can hydrate SQLite from a stale *or partial* view. The existing data-loss guards (empty-state skip, hydration-excerpts.rs:361-366; #443 SQLite-only preservation, :368-371) protect against *empty*, not *partial/stale*.
2. **The projection reuses the authoritative file names.** Broker logical paths in the tests are exactly the v3/v2 layout — `checkpoint/state.json`, `meta/hub.json`, `issues/<uuid>.json` (tests.rs:308-315; validate.rs:227-234). A directory containing `issues/*.json` + `meta/counters.json` is precisely what the destructive v2 `hydrate_to_sqlite` consumes from `cache_dir` (hydration-excerpts.rs:104-134). The fail-closed gate (`v2_file_path_decision`) probes *git refs*, not the provenance of the directory being read (hydration-excerpts.rs:224-243) — so on a hub the decision believes is v2-only, pointing any cache_dir-typed parameter at a projection would be accepted. Today no code passes the projection as a cache dir; the risk is that nothing *prevents* it (no sentinel file, no refusal in the v2 readers).
3. **Two sha namespaces, one marker format.** `record_hydrated_ref` / `maybe_auto_hydrate` key freshness on `git rev-parse HEAD` of the hub cache (hydration-excerpts.rs:417, 504-528) — a 40-hex string. The broker head is also 40-hex but from a *different repository* (the private backend). A future wiring that reuses the marker format across backends can record/consume a head that the local git layer can never reproduce, silently suppressing or forcing re-hydration.
4. **Stale-local-refs-as-truth (wiring hazard).** `fetch_and_adopt_v3_refs` treats fetch failure as "local refs stand" (sync-cache.rs:440-442). If the broker became the durable store while fetch still used git, local refs would freeze and become the de-facto authority — the inverse accident of the projection problem. (Interpretation; the wiring does not exist yet.)

### Q6 — stale-state reconciliation and `commit_cas`

What it preserves (good): the op-id trailer check plus **content-digest verification** at the head (transport.rs:262-300) correctly prevents the prior major finding; a reused op-id or later overwrite surfaces as `verified: false`; the double-write is prevented (tests.rs:147-187); `stale_state` writes nothing and carries `observed_head` (state.ts:334-338), so the re-read is conservative.

What it does **not** preserve:

1. **Lost update on shared paths via blind rebase.** On a conflict whose head is not ours, `commit_cas` re-issues the *same absolute content* against the new head (transport.rs:317-318 → loop). If the competing writer changed the *same path* with newer information (a counter, any read-modify-write file), our retry silently rolls it back. "Whole-file upserts only" (transport.rs:224-226, design doc:187-189) is a doc convention, not an enforceable precondition; the only test of the rebase path uses *disjoint* paths (tests.rs:78-107). The information needed to avoid the lost update — a per-path expected value, or a transform instead of a payload — is absent from `CommitRequest` entirely.
2. **Buried-own-write escalates to a re-write.** If our commit landed and was then buried by another writer, the head no longer carries our trailer, so we re-commit — for disjoint paths benign, for shared paths it is case 1. There is no "our write landed; abort" detection beyond the immediate head.
3. **`verified:false` returned as `Ok`** on the `already_applied` branch (transport.rs:301-314) contradicts the client's rule that a non-verified commit is a protocol error (client.rs:624-629). Callers that match on `Ok` without inspecting `outcome.verified` will trust state they did not verify.
4. **Head-vanished edge.** If the ref disappears between the 409 and the re-read, `expected_head` becomes `None` (transport.rs:317) and the retry bootstraps a *fresh single-commit history containing only our files* — catastrophic if the deletion was administrative rather than total. Out of the v1 contract's scope, but the code path exists.
5. **Transport-error ambiguity is outside the reconciliation.** A timeout on `POST commit` yields a `transport` error with `retryable: true` (client.rs:667-675; error.rs:132-137), but for a *write* the honest answer is "unknown — reconcile via op-id", which `commit_cas` only does for `stale_state` (transport.rs:258). The error type cannot express "reconcile-required" (see §5).
6. `verify` during reconciliation re-reads state and can itself race (the mock rejects non-head commits, mock.rs:256-261); a landed-write detection can fail as an error even though the write is durable. Minor.

**Bottom line (Q6):** sufficient to prevent *duplicate writes* of the same op-id; **not** sufficient to prevent *lost updates* on shared paths or to handle lost write-responses; the safety rests on caller conventions the interface does not encode.

---

## 4. Per-agent refs vs whole-tree CAS (Q4)

The mismatch is a **CAS-unit mismatch**, not a data-layout mismatch: v3's concurrency invariant is *single-writer-per-agent-ref* with writer-authoritative adoption (sync-cache.rs:459-469: "the remote tip is the single writer's canonical history… we never need to merge another writer's ref"), while broker v1's unit is *one ref for the whole project* (README:93-99; state.ts:332-339; fast-forward-only `updateRef`, README:93-96). Comparing the options on semantics:

**A. Encode per-agent refs as files in the broker tree.**
- *Preserved:* content layout and the reducer contract (files map 1:1 to `agents/<id>/events.log`, `checkpoint/state.json`, `meta/hub.json` — the projection test already proves readers consume this, tests.rs:271-328). Whole-tree CAS *does* give cross-file atomicity for free (shard + index in one commit), which per-agent refs achieve only per-ref.
- *Destroyed:* per-agent concurrency. Every mutation by every agent contends on one head; throughput becomes one serialized commit chain and conflict rate grows with writer count. Correctness survives only under strict disjoint-path ownership (§3 Q6.1) — i.e., the single-writer invariant is *relocated* from the storage layer to an unenforced naming convention.
- *Loses:* git-native provenance. REQ-1's fast-forward-only history and REQ-11's non-FF prune-rewrite (sync-cache.rs:394-417) have no broker equivalent; audit becomes "trailer strings in commit messages" and deleted content can only be tombstoned (no delete op; SECURITY-NOTES.md:74-75; design doc:183-186), so prune turns into permanent tree garbage.
- *Hard ceilings:* 256 KiB/file, 1 MiB/commit, 32 files/commit, and the broker's refusal of a truncated tree (state.ts:237-241) — large hubs fail closed at the inventory read. Chunking/sharding is mandatory, not optional.
- *Verdict:* viable as an explicitly serialized stopgap for low-writer-count Codex-Cloud-style usage (the durability experiment is exactly this pattern: unique new paths per task, CODEX-CLOUD.md:80-102); unacceptable as the hub's steady-state model without an enforceable path-ownership rule.

**B. Extend the broker with finer-grained heads (per-agent refs under the broker namespace).**
- *Preserves:* v3's semantics 1:1 (per-writer CAS, FF-only own ref, writer-authoritative adoption); the adapter's `commit` would gain a per-namespace `expected_head`.
- *Costs:* a broker contract v2 (multi-ref management, per-ref scoping) — eroding the "six operations, one tree" simplicity that is the broker's security story, and re-deploying the GitHub Git Data calls per ref.
- *Risk:* the broker becomes a thin git-remote proxy; but that is precisely what Crosslink needs it to be if v3 is to be preserved.
- *Verdict:* semantically cleanest. This is the option that does not require changing either model.

**C. Compose per-agent state into one project checkpoint (driver model).**
- *Matches* broker v1 exactly, but changes Crosslink's trust/availability model: a designated writer becomes the serialization point and the writer of record; agents' own refs must still live somewhere (in-tree files → back to A's ownership problem, just with fewer writers). Display-id first-claim-wins and event signing survive; agent autonomy and the "offline writes land on your own ref" property (hub_v3-excerpts.rs:166-191) do not. This is a redesign the task forbids (design doc:20-22).
- *Verdict:* only legitimate if a driver topology is independently adopted; then it should be chosen for its own reasons, not as an adapter workaround.

**D. Move/change the adapter boundary.**
- Does not resolve the mismatch — but is necessary hygiene regardless: rename/re-scope the trait to what it abstracts today (a broker-v1-shaped CAS transport), move `hydrate_into` out of the trait into a free function over it (it is a consumer with filesystem side effects, not a transport operation), and reserve the true Crosslink seam (append-event / read-reduce / commit-with-lease) for the post-decision hub layer. Note also the fragile delegation: the trait impls call same-named inherent methods and rely on Rust's inherent-precedence (transport.rs:337-358; the comment at :339 acknowledges it) — renaming or removing an inherent method silently creates infinite recursion.
- *Verdict:* not an answer to the mismatch; a prerequisite to answering it honestly.

**Ranking:** B (with D's boundary hygiene) > A (explicit, documented stopgap with disjoint-path ownership) > C (only as a separate topology decision) > D alone. The design doc's option list (design doc:162-169) frames these as implementation variants; they are in fact three different concurrency models, and only B preserves the existing one.

---

## 5. Coupling that makes later substitution hard (Q7)

- **Git coupling (low, mostly inherited):** the adapter itself shells to no git; but the trait's currency is 40-hex commit shas and ref names (`is_acceptable_ref` accepts exactly the broker's ref forms, config.rs:281-292; `verify(commit, …)` demands exact commits, transport.rs:103-107). Any future backend must speak git-object vocabulary. Acceptable for a broker adapter; wrong for a "persistence seam" (see Q1).
- **Broker-v1 coupling (high, by construction):** whole-tree `expected_head`, `Broker-Op:` trailer reconciliation (transport.rs:326-335), the 8 wire codes frozen "MUST NOT be renamed without a contract version bump" (error.rs:29-32), and the client-side mirror of every `LIMITS` value (validate.rs:12-29 vs state.ts:25-32) — four synchronized copies of the limits (broker source, validate.rs, stub, mock via validate). The ref-name duplication (`refs/heads/projects/<uuid>/state` built independently in config.rs:271-279, mock.rs:90/189/336, contract.rs:688-694, broker/paths.ts:63-68) is a substitution tax if the namespace ever changes.
- **Crosslink-v3 coupling (covert):** the projection's usefulness is defined by the v3 file layout (`checkpoint/state.json` etc., tests.rs:304-328), so the broker namespace schema is *implicitly* v3 even though §4 defers the mapping decision; and `StateBackend` selection is process-global env, not per-project — the two coexist with `hub_mode`/`SyncManager`'s git-based mode resolution (sync-core.rs:57-69) with no cross-check that the selected broker UUID is the project this repo's hub belongs to (the whoami/state equality check exists only in the status command, commands/state_broker.rs:43-48; `transport_from_env` never authenticates identity).
- **Q3 answer:** the existing model is preserved today (no dual writes; `Local` default; git/SQLite untouched), but the *seeds* of a second model are present: a second head-sha namespace, a second backend enum no write path consumes, and a projection that shares the authoritative layout's file names. It becomes a competing model at exactly the moment §4's decision is skipped — which is why Q10's answer is "decision first."

---

## 6. Missing invariants (Q8)

1. **Path-ownership:** no representation of "this path has exactly one writer" — the precondition for `commit_cas` rebase safety (transport.rs:224-226) and for any option-A encoding. Enforceable in the type (e.g., `AgentScopedPath`) or in the broker (per-namespace heads).
2. **op_id uniqueness:** the contract permits reuse (state.ts:503-507); reconciliation trusts the trailer. A convention (agent-id + per-agent monotonic seq) is documented nowhere in code.
3. **Projection completeness:** no head-commit marker or atomic-rename in `hydrate_into` (§3 Q5.1); "disposable" requires "reconstructible and self-identifying"; today it is neither verifiable nor atomic.
4. **Inventory↔blob cross-check:** `hydrate_into` pins blobs to the inventory commit but never compares the returned `StateBlob.sha256/size/blob_sha` against the `StateEntry` it listed (transport.rs:188-190; `StateEntry` digest fields are otherwise unused), nor asserts `blob.commit == requested at` (client.rs:536-548 discards it). A broken broker can swap content at a path and the client's self-consistent digest check (client.rs:201-223) won't notice.
5. **Write-retryability typing:** `retryable: true` on a timed-out commit (§3 Q6.5) invites the exact "blind retry" the contract forbids (README:97-99); the error enum has no "reconcile-required" class.
6. **`Ok` ⇒ verified:** the `already_applied` branch can return `verified:false` as success (§3 Q6.3), inconsistent with client.rs:624-629.
7. **No-delete accounting:** v1 has no delete (SECURITY-NOTES.md:74-75); no tombstone convention, no garbage bound, no tree-size/truncation budget (state.ts:580-585 fails closed on truncation — the failure mode for a large hub is permanent read outage, unmodeled).
8. **Backend↔project binding:** selected broker UUID is not checked against the repo's project identity at transport construction (§5, third bullet).
9. **Head-namespace separation:** no rule that broker head shas and local git shas never share freshness markers (§3 Q5.3).

---

## 7. Missing tests despite a passing suite (Q9)

The suite (design doc §7; I did not run it — unverified) is strong on client-side envelope/auth/redaction and mock-level CAS. Materially untested:

1. **Lost update via rebase on the same path** — a competing writer commits a *newer value for our path* between our read and retry; the suite only exercises disjoint paths (tests.rs:78-107). This is the central semantic risk (§3 Q6.1) and has zero coverage.
2. **`validate_message` panic on non-ASCII messages** (§3.0) — no test with any multi-byte message.
3. **`hydrate_into` mid-loop failure** → partial projection; and **blob-vs-inventory mismatch** detection (invariant 4) — no test feeds a blob whose digest disagrees with the listing at the pinned commit.
4. **Timeout on `POST commit`** (response lost, write landed) — no test that the resulting error is handled as reconcile-required; `commit_cas` does not reconcile transport errors (transport.rs:258 only matches `stale_state`).
5. **Cross-host redirect behavior** — the "token only to the configured host" claim (mod.rs:37-39) is untested and, I believe, unguaranteed: no redirect policy is set (client.rs:483-492); reqwest's default follows redirects and I could not verify from this packet whether it strips `Authorization` on cross-host redirects. **[Marked: not verified — reqwest source not in packet.]** If headers are replayed, a compromised/misconfigured broker host can exfiltrate the token; setting a no-redirect policy would make the claim mechanical.
6. **Mock fidelity divergence:** the mock rejects historical-commit `verify` (mock.rs:256-261) while the broker supports it (state.ts:282-326) — mock-based tests cannot catch regressions in non-head verification.
7. **`transport_from_env`** — no tests at all (and no callers).
8. **Two-process race** against the stub (its routes serialize under one mutex, contract.rs:276-279) — real-CAS behavior (e.g., the 422→stale path, state.ts:392-401) is never exercised.
9. **Head-vanished bootstrap** after a conflict (§3 Q6.4) — untested.
10. The live probe is, by design, never run (state_broker_live.rs:22-24) — the whole contract-vs-deployment layer remains source-derived only (design doc:176-181 acknowledges this).

---

## 8. Recommended next step (Q11) and wiring order (Q10)

**Q10: A design/invariant decision is required first.** Wiring `SyncManager` to the broker before resolving Q4 would (a) silently replace per-agent-ref CAS with global serialization, (b) activate the lost-update and projection-authority paths (§3) with real user data, and (c) contradict the change's own non-goals (design doc:20-22) and handoff instruction (handoffs/issue-802-state-broker-transport.md:66-69). The read-only live smoke test (design doc §8 steps 1-3) is orthogonal, safe, and should proceed — but it cannot substitute for the decision, because it exercises reads only.

**Q11 — smallest next architectural step (no implementation):** write the decision record that §4 defers, and make it *binding on the interface*. Concretely, one ADR that fixes three things:
1. the **CAS unit** (recommend: broker v2 with per-agent heads — option B; option A only as an explicitly labeled stopgap for single-writer experiments),
2. the **path-ownership convention** (which broker paths are agent-private vs shared, and what `commit_cas` may rebase), and
3. the **op_id convention** (agent-scoped uniqueness).
Everything else in this review (inventory cross-check, projection marker, reconcile-required error class, the `validate_message` panic fix, boundary rename) is a follow-on checklist that the ADR makes cheap to sequence.

---

## 9. Confidence and main uncertainty

**High confidence (read the code):** the trait's shape and what it does/does not touch; the `already_applied` digest check and its regression test; the rebase-is-absolute-content semantics; the projection's non-atomicity and missing self-identification; the triplicated validation and the `validate_message` byte-slice panic; backend-selection defaults and the env-alone-does-not-switch rule; the prior review's dispositions being real.

**Medium confidence:** the Q4 option analysis — based on the packet's *excerpts* of `hub_v3.rs`/`sync/*`/`hydration.rs`, not the full files; the prune/adoption details could be richer than the excerpts show. The severity ranking of the lost-update risk assumes multi-writer hub usage over the broker.

**Not verified / could not check:** that any test passes (nothing was run); `is_windows_reserved_name` correctness (validate.rs:210, flagged by the prior review too, handoffs/802-review-hy3.md:54); reqwest's redirect header behavior (no dependency source in packet); `github.ts` `updateRef` force flag (file not in packet; README:93-96 is the only evidence for FF-only); whether `.crosslink/state-projection` is gitignored; full `Cargo.toml`/lockfile context beyond the patch.

**What would change my mind:** (a) evidence that hub deployments are effectively single-writer (then option A is adequate and my Q4 ranking shifts toward A-then-B); (b) a full `hub_v3.rs` showing prune/adoption paths that map onto whole-tree CAS without ownership conventions; (c) confirmation that reqwest strips `Authorization` on cross-host redirects (removes the §7.5 finding); (d) a demonstration run of the live probe showing envelope shapes matching `client.rs` deserialization exactly.

---

## 10. Answers table (Q1–Q11)

| # | One-line answer |
|---|---|
| Q1 | Operations are right, the boundary is mislabeled: it is a broker-v1 CAS transport (git-shaped vocabulary, fs side effects in the trait), not Crosslink's persistence seam; move `hydrate_into` out of the trait and reserve the real seam for post-decision hub layering. |
| Q2 | ~3.3k production / ~2.1k tests / ~0.6k docs; cuttable: third copy of validation rules in the stub (fixtures instead), hand-rolled HTTP server (~190 lines), dead `transport_from_env`, unused `BrokerErrorCode::ALL`, test-only `pseudo_git_sha`/`text()` in public API, speculative `StateBackend` accessors, mock/stub overlap. |
| Q3 | Preserved today (no dual writes, default Local, git/SQLite untouched); the second model exists only as seeds — a second head namespace, an unrouted backend enum, and a projection wearing v3's file names — and becomes real if wiring precedes the §4 decision. |
| Q4 | The mismatch is the CAS unit (per-agent vs whole-tree), not layout. B (finer heads) is the only option that preserves v3 semantics; A is a correct stopgap only under an enforced disjoint-path ownership rule, at the cost of global serialization, no-delete garbage, and hard size ceilings; C is a trust/availability redesign the task forbids; D is necessary hygiene but answers nothing by itself. Rank: B > A > C > D-alone. |
| Q5 | Yes: partial/stale projections are indistinguishable from complete ones (no head marker, non-atomic writes); the projection reuses exactly the file names the destructive v2 reader consumes, and the fail-closed gate checks refs, not directory provenance; broker head shas share format with the local git freshness marker. |
| Q6 | Prevents duplicate writes of the same op-id (post-fix digest check is sound), but does not prevent lost updates when a rebase re-issues absolute content over a newer same-path value, cannot reconcile a landed-but-timed-out write, and can return `verified:false` as `Ok`. |
| Q7 | Broker-v1 coupling is highest (frozen codes, trailer currency, four copies of limits, ref-name strings in five places); git coupling survives as the 40-hex/ref vocabulary of the trait; v3 coupling is covert (projection layout = v3 layout; process-global backend selection with no repo↔project binding). |
| Q8 | Missing: path-ownership, op_id uniqueness, projection completeness/atomicity, inventory↔blob and blob.commit cross-checks, reconcile-required error class, Ok⇒verified, no-delete/tombstone accounting, backend↔project identity binding, head-namespace separation. |
| Q9 | Untested: same-path lost-update rebase, non-ASCII `validate_message` panic, hydrate partial-failure and digest cross-check, commit timeout ambiguity, cross-host redirect token behavior, mock's historical-verify divergence, `transport_from_env`, real two-process CAS race, head-vanished bootstrap; live probe never run. |
| Q10 | Decision first. The read-only live smoke test should proceed but cannot settle the concurrency question; wiring `SyncManager` before the ADR changes the hub's model with user data. |
| Q11 | One ADR fixing the CAS unit (recommend B), the path-ownership convention, and the op_id convention — then the checklist fixes (panic, cross-checks, projection marker, error class) follow cheaply. No code in this step. |
