# ADR-802 — State broker authority and CAS-unit decision

Status: **Proposed — binding for any SyncManager wiring until superseded**
Issue: #802 · Date: 2026-09-21 · Branch: `feature/pp3g-state-broker-adapter`
Resolves: the deferred §4 mapping question in `.design/state-broker-transport.md`
Inputs: `handoffs/review-802/99-synthesis.md` + the five clean-room reviews;
broker v1 source (`state.ts`, `paths.ts`, `github.ts`, `README`, `SECURITY-NOTES`);
v3 design (`.design/hub-v3-per-agent-refs.md`) and code (`hub_v3.rs`, `sync/cache.rs`,
`compaction.rs`, `checkpoint.rs`).

---

## 0. Decision summary

1. **Adopt C now.** Broker v1 is a **derived project checkpoint/read tier** plus a
   **bounded inbox transport**. The v3 per-agent refs remain the authoritative
   concurrent journal. Credential-less mutations enter the journal only through
   the inbox and a single credentialed drainer/proxy writer.
2. **Reject option A** (per-agent refs encoded as files under the one whole-tree
   CAS) as a steady-state model — on invariant violations, not cost (§12).
3. **Track B later**: broker v2 adds per-writer heads; minimum capability in §13.
4. **Do not wire `SyncManager` now.** The exact safe condition is §16. Until then
   the broker may be used only by the C-now paths in §14.

---

## 1. Context

Broker v1 gives one whole-project ref (`refs/heads/projects/<uuid>/state`), a
whole-tree `expected_head` CAS, fast-forward-only updates, read-back digests, and
**no delete**. Limits: 32 files/commit, 256 KiB/file, 1 MiB/commit, 512-char
message, 32 verify paths. v3 gives one ref per agent
(`refs/heads/crosslink/agents/<id>`), where a plain fast-forward push **is** the
CAS (REQ-1), a deterministic checkpoint ref that is a pure cache (REQ-7), and a
prune that rewrites only the owner's ref after the covering checkpoint is pushed
(REQ-11). The two CAS units are different; this ADR fixes which one is
authoritative, what each may be used for, and what must never be composed.

Concrete size evidence (read-only, this repository): checkpoint ref
`68e72560f5628f5953919f3ae7f60a8c9482e61a`, `state.json` = **1,846,959 bytes**
for 800 issues / 1,460 comments — 7× the per-file limit and 1.8× the per-commit
limit; gzip-9 ≈ 459,884 bytes, still over the per-file limit. A single-file
derived publish is impossible even for a modest hub; chunking is mandatory (§9).

---

## 2. One concrete lifecycle, traced completely

Legend: **J** = v3 journal (authoritative); **B** = broker v1 tier (derived).
A and B are two agents starting from the same prior durable state; step 4's
"winner" is per-ref, not global — see step 5.

| # | Step | Authoritative state | CAS unit | Path ownership | Op-id semantics | Safe rebase | Must fail | Landed-proof | Projection freshness |
|---|---|---|---|---|---|---|---|---|---|
| 1 | A appends work | **J:** A's own ref `agents/A` is the sole authority for A's events. **B:** nothing yet | **J:** per-ref `update-ref` (MustMatch tip) then FF push. **B:** (credential-less A) whole-project CAS of `inbox/A/pending.json` | **J:** A owns `agents/A`. **B:** A owns `inbox/A/**` (append role) | **J:** `(agent_id, agent_seq)`, monotonic per agent. **B:** `op_id` = durable receipt id; item carries `(A, seq, ts, envelope_sha256)` | **J:** none needed (own ref). **B:** unique-path item → blind re-issue safe; shared pending file → re-read/re-merge only | **J:** own-ref CAS mismatch or non-FF push (identity collision) → hard error. **B:** stale with an unproven overlapping path | **J:** local ref tip after `update-ref`; re-push idempotent. **B:** head `Broker-Op:` trailer + per-path digest verify | **B:** manifest watermark unchanged until publish; readers must accept documented lag |
| 2 | B concurrently appends different work | **J:** both refs; the reduced union is project state | **J:** per-ref, disjoint — zero contention | **J:** B owns `agents/B`. **B:** B owns `inbox/B/**` | **J:** `(B, seq)`, independent of A | none | only same-ref identity collision | local ref tip | both events absent until next publish |
| 3 | Both begin from the same prior durable state | **J:** prior union = each agent's fetched refs + adopted checkpoint watermark | **J:** each write is a child of the writer's own ref tip; there is no shared parent CAS | unchanged | **J:** seq allocated from the writer's own high-water mark (`read_max_event_seq_from_ref`) | n/a | n/a | n/a (no shared write) | common read snapshot = last published manifest; record its watermark |
| 4 | One wins first | **J:** the winner's ref tip moves. There is no global winner; the other's ref is unaffected | **J:** FF push on the winner's own ref only | unchanged | winner's seqs land; loser's unaffected | none | only the winner's own-ref move | local tip == remote tip; re-push is a no-op | winner's event appears only in a later publish whose reduce sees its ref |
| 5 | The other encounters stale state | **J:** **no global stale.** The loser's write still succeeds on its own ref. Staleness exists only (a) on the same ref (identity collision → hard), or (b) as a stale *decision* (lock/display-id confirm). **B:** whole-tree CAS loser gets 409 `stale_state`, nothing written | **J:** per-ref. **B:** whole-tree | loser's paths only | **J:** none. **B:** loser's op not applied; reconcile via trailer | **J:** none. **B:** only owned + byte-identical-at-new-head paths; shared pending → re-read/re-merge | blind rebase over an overlapping path; acting on a lost lock/display-id decision | **B:** re-read head; trailer absent + no overlap proof ⇒ not landed ⇒ re-issue; otherwise fail | unchanged; derived reads are advisory and **not sufficient for exclusivity confirm** |
| 6 | Replay of an already-accepted operation | **J:** identity `(agent_id, agent_seq)`. **B:** identity `op_id` + payload digest. **B(inbox):** identity `(writer_id, op_id)` | a replay must not move any ref/head if already applied | unchanged | retries reuse the same `op_id`; a new logical mutation gets a new one; never reuse for divergent content | no rebase on a landed replay | same `op_id` + different digest (must be a distinct hard error, **never** `Ok{verified:false}`); duplicate `(agent_id, seq)` append (writer must check the high-water mark — `append_event_to_ref` does not enforce this today); double-drain (dedupe by watermark/key) | **J:** event present in the ref, or key ≤ checkpoint watermark. **B:** trailer + digests; push replay = ref-tip equality | replayed publish must not regress the watermark; identical manifest digest ⇒ no-op success |
| 7 | Response lost after a remote commit | **J:** the local ref already holds the commit *before* push, so the local ref is the proof; remote is repaired by an idempotent re-push. **B:** outcome unknown — the commit may have landed | re-issue only after reconcile proves not-landed | unchanged | `op_id` must be persisted locally **before** the attempt (write-ahead), so a crashed client can reconcile | only after proof; never blind | retrying while the outcome is unknown; treating timeout/502 as failure | **B:** read head; trailer + digests match ⇒ landed; trailer absent + expected head observed ⇒ not landed; transport still failing ⇒ **unknown → typed reconcile-required, blocks further writes to owned paths** | publish reconciles; marker written only after the landed commit verifies |
| 8 | Prune / delete / tombstone | **J:** prune rewrites the owner's `events.log` dropping keys ≤ the pushed checkpoint's watermark; issue deletion is a tombstone event; `deleted_issues` wins forever. **B:** v1 has **no delete** | **J:** non-FF rewrite of the owner's own ref (single writer), gated on durable coverage. **B:** fixed-slot manifest/chunk overwrite is an upsert; inbox shrink is an upsert under CAS | prune only the owner's ref (or its proxy writer); **B:** `checkpoint/**` publisher, `inbox/<writer>/**` writer+drainer | prune/publish have their own `op_id`s; tombstones are ordinary journal events with `(agent_id, seq)` | prune never rebases; publish may supersede only a lower/equal watermark | prune without a pushed covering checkpoint; any broker delete attempt; treating a missing broker path as deletion; lower-watermark publish; browse-tree deletion mirroring (unrepresentable → out of scope) | **J:** checkpoint watermark ≥ pruned keys **and** checkpoint pushed. **B:** manifest digest | a projection must never justify a prune; the prune gate is the pushed git checkpoint only |
| 9 | State exceeds one broker-v1 commit | **J:** unaffected (git has no such limit; per-ref growth is bounded by prune). **B:** publish fails closed | one logical publish = one whole-tree commit; never split without a manifest | publisher only | publisher `op_id` per publish; readers key on manifest digest | above budget → no rebase, fail | >32 files, >256 KiB/file, >1 MiB total, partial publish, silent truncation, oversized inbox pending | manifest names the exact chunk set + digests; missing/mismatch ⇒ read error; commit trailer + verify ⇒ landed | only a fully digest-verified manifest projection may be hydrated |

**Trace conclusion.** Under C, steps 1–4 are journal-only and contention-free;
step 5's "stale" belongs to the broker tier (or to same-identity contention), not
to v3; step 6 needs a distinct replay outcome; step 7 needs the
reconcile-required class; step 8 requires the delete-free conventions below; and
step 9 forces the chunked, single-commit publish rule. Every one of those is a
condition on the broker tier, never a change to the journal.

---

## 3. Authority model

- **Journal of record:** the union of v3 per-agent refs, plus the pushed
  checkpoint as the authoritative reconstruction base for keys ≤ its watermark
  (REQ-11). Authority for an agent's events is that agent (or its designated
  proxy writer) as the single writer of its ref.
- **Derived tier:** broker v1 holds only snapshots that are pure functions of a
  durably pushed git checkpoint plus the refs it covers. It is never the sole
  durable record of any mutation.
- **No Crosslink decision may depend solely on broker state**: prune safety, lock
  winner, display-id freeze, and hydration correctness are defined by the journal.
- **Credential-less mutations** are intents until a credentialed proxy journals
  them. Broker receipt is a durable *receipt*, not journal application.
- The broker's own ref remains authoritative **for what the broker contains**
  (its CAS target); this ADR constrains what may be published there, not how the
  broker stores it.

## 4. CAS unit

| Tier | Unit | Conflict meaning |
|---|---|---|
| v3 journal | one ref: local `update-ref` CAS + FF push of that ref | same-ref move = identity collision/tampering → hard error, never rebased |
| v3 checkpoint | one ref, `--force-with-lease`, deterministic content | lease loss = benign (identical content) |
| Broker C publish | one whole-tree commit: manifest + fixed-slot chunks | `stale_state` → watermark comparison, not blind rebase |
| Broker C inbox | one whole-tree commit of `inbox/<writer>/pending.json` | `stale_state` → re-read/re-merge for a shared file; blind only for unique paths |
| Broker B (later) | one named head per writer (+ checkpoint/meta heads) | per-head conflict only; must not fail other heads |

**Rule:** the broker's CAS unit (tree) is coarser than its ownership unit (path).
Safety therefore comes from §5 and §6, never from the CAS alone.

## 5. Path ownership invariant

For every broker logical path `p` there is exactly one owner class; no path is
owned by two classes, and no writer CASes a path it does not own.

- `checkpoint/manifest.json`, `checkpoint/chunks/<i>` — the derived publisher
  (one active publisher per project, serialized by CAS + watermark).
- `inbox/<writer_id>/pending.json` — `writer_id` appends; the drainer shrinks.
  Both roles are read-modify-write under CAS; the drainer is the only shrinker.
- Reserved for B: `heads/**`, `meta/**`; nothing writes them under C.
- `inbox/<writer_id>/ops/<op_id>.json` (unique immutable items) is permitted only
  where growth is acceptable; it cannot be drained on v1 (no delete).

**Rebase rule (all tiers).** A re-issue after `stale_state` is permitted only if
all of:
 1. every path in the request is owned by the caller;
 2. each owned path is byte-identical at the observed new head to the caller's
    read base (proved by `verify` at both commits);
 3. for a shared read-modify-write path, the caller re-reads and re-applies its
    modification to the new content (never re-issues the stale absolute payload);
 4. for the publisher, additionally `candidate.watermark >= head.manifest.watermark`.
If any condition cannot be proved, **fail the operation and write nothing.**

## 6. Operation identity / replay invariant

- **Journal:** identity is `(agent_id, agent_seq)`; `agent_seq` is monotonic per
  agent and assigned by the origin before durable publication. A writer must
  never append the same key twice; push replay is idempotent by construction.
  Reducer idempotency for duplicate identical envelopes exists for today's event
  types but is a safety net, not the contract; `append_event_to_ref` must gain
  the high-water-mark check as a hardening gate.
- **Broker write:** identity is `op_id` + exact payload digests. A new logical
  mutation always gets a new `op_id`; retries and reconciliation reuse it.
  `op_id` uniqueness scope is `(project_uuid, writer_id, op_id)`; the broker does
  not enforce uniqueness.
- **Inbox item:** identity is `(writer_id, op_id)`, carrying
  `(agent_id, agent_seq, timestamp, envelope_sha256)`. Drain is idempotent: an
  item whose key ≤ the checkpoint watermark, or already present in the target
  ref, is a verified no-op.
- **Divergence is a first-class outcome:** "our op id at the head with different
  content" must be a distinct hard failure, never a success with
  `verified: false`.

## 7. Reconciliation invariant

Every mutation attempt resolves to exactly one of **landed-verified**,
**not-landed**, or **unknown/reconcile-required**:

- *Journal:* the local ref tip after `update-ref` is the proof of append; the
  remote is proved by ref-tip equality (re-push is a no-op). No `op_id` needed.
- *Broker:* read the head; landed iff the head commit's `Broker-Op:` trailer
  records our `op_id` **and** `verify(head, owned_paths)` digests equal the
  intended payload. If the trailer matches but digests differ → diverged (fail).
  If the trailer is absent and the observed head descends from our expected head
  → not landed. If the head cannot be read → **unknown**, and the client must
  block further writes to those paths until resolved.
- *Unknown* is only cleared by a successful re-read; timeouts and 502 read-back
  failures are reconcile-required, never retryable in place.
- The `op_id` must be persisted locally before the first attempt.

## 8. Projection identity / freshness invariant

A projection is a set of files materialized from **one exact broker commit**,
plus a marker recording: backend identity (host + project UUID), state ref,
commit, watermark, `state_sha256`, per-file digests, and `complete: true`.

- The marker is written atomically (temp + rename) only after every file verifies.
- Hydration must refuse a projection whose marker is missing, incomplete,
  mismatched to the backend identity, or whose recorded watermark is older than
  the local last-hydrated watermark. Emptiness is not a freshness check.
- The marker namespace is distinct from git freshness markers
  (`record_hydrated_ref`); a broker commit sha must never be stored as, or
  compared with, a git sha.
- The projection directory must be disjoint from `.crosslink/.hub-cache` and
  authoritative dirs; hydration takes the marker/commit, never a bare directory.
- A projection is fresh for a consumer iff its commit equals the head read at
  consumption, **or** the consumer explicitly accepts a recorded older watermark
  for advisory use only — never for exclusivity decisions.

## 9. Delete / tombstone semantics

- **Journal:** issue deletion = `IssueDeleted` tombstone; `deleted_issues` wins
  forever; prune = owner-only rewrite of `events.log` dropping covered keys,
  allowed only after the covering checkpoint is committed **and pushed**.
- **Broker v1 has no delete.** Under C, file deletion is never used:
  - the published state is a fixed-slot manifest/chunk set, overwritten in
    place; readers must not interpret a missing path as deletion;
  - tombstones live inside the state document (`deleted_issues`), which is
    authoritative for deletion within the derived tier;
  - the browse tree (`issues/*.json`) is **not** mirrored under C — its
    deletions are unrepresentable on v1;
  - inbox items are removed by rewriting (shrinking) the pending file under CAS;
    if the shrink cannot be CAS'd, fail closed.
- A projection or broker head may never be used as the prune gate; prune safety
  is defined solely by the pushed git checkpoint.

## 10. Size / atomicity rule

- v1 budget: ≤32 files, ≤256 KiB/file, ≤1 MiB total per commit.
- **One logical publish = one commit.** The publisher writes fixed-slot chunks
  `checkpoint/chunks/<i>` plus `checkpoint/manifest.json` in a single whole-tree
  CAS. The manifest names exactly the chunk set, order, sizes, and digests; the
  reader concatenates, verifies, and only then parses.
- If the canonical state exceeds the budget (even compressed — measured: 460 KiB
  gzip for this repo), the publish **fails closed**; it is never split across
  commits without a manifest generation rule, and it never truncates.
- Inbox `pending.json` ≤256 KiB; writers fail closed (or use bounded numbered
  pages with a manifest) when full; the drainer shrinks.
- B (later): limits apply per head/commit; per-agent logs may be chunked within
  a head; no cross-head atomicity is required.

---

## 11. Is "C now + B later" semantically sound?

**Conditionally yes.** The composition is coherent because each tier keeps its
own CAS unit and authority, and B can be added without changing the journal. It
fails at exactly these points if any condition is dropped:

1. **Derived tier used as a durability or prune gate** → breaks REQ-11 prune
   safety (events could be pruned while only the broker has the covering state).
2. **Projection consumed without the §8 marker** → stale/partial state can
   regress SQLite (display-id freeze, tombstone visibility).
3. **Inbox without `(writer_id, op_id)`, monotonic seq, drain dedupe, and a
   bound** → double-apply, lost update on shrink, or unbounded growth (v1 cannot
   delete).
4. **Derived reads used for exclusivity confirm** → a credential-less writer can
   believe it holds a lock that the journal awarded to an earlier key. Derived
   reads are advisory; confirmations must come from the drainer/journal.
5. **Blind re-issue on overlapping paths** (the current `commit_cas` behavior)
   → same-path lost update, or a lower-watermark manifest clobbering a newer one.
6. **Journal replay not enforced** (duplicate `(agent_id, seq)`) → reducer
   idempotency is a safety net, not a guarantee; enforce the high-water-mark
   check at append.
7. **Publisher reducing from refs alone after a prune** → silently drops
   pruned-but-covered state; the publisher must adopt the pushed git checkpoint
   first and reduce from (checkpoint + refs).
8. **B reusing C's whole-tree CAS as the per-writer unit** → per-writer heads are
   unrepresentable; reserve namespaces and re-scope the trait (§13–§15).

It is **not** a redesign of hub v3, **not** broker authority, and **not**
git-equivalent concurrency for credential-less writers: it adds a drain latency
and a hard dependency on a credentialed proxy/publisher.

## 12. Option A — rejected (invariant basis)

Option A = per-agent refs encoded as files under the single broker tree with
whole-tree CAS per mutation. Rejected as a steady-state model because it violates,
in order of severity:

- **A1 — single-writer-per-ref CAS replaced by global serialization.** The v3
  invariant "a plain push to your own ref IS the CAS" is deleted; correctness
  survives only under an unenforced disjoint-path convention.
- **A2 — rebase is lossy.** Whole-file upsert over a concurrent same-path append
  loses the other writer's bytes; v1 offers no per-path precondition, so the only
  safe transform is client-side re-read/re-merge.
- **A3 — REQ-11 prune unrepresentable.** v1 is FF-only with no delete; agent logs
  grow monotonically into the 256 KiB/file limit — a permanent dead end.
- **A4 — no delete.** Browse-tree tombstones and pruned-log garbage have nowhere
  to go; the tree grows forever.
- **A5 — limits apply to the union of all logs + checkpoint** (32/256 KiB/1 MiB),
  and chunking turns one logical append into a multi-commit sequence with no
  atomicity.
- **A6 — provenance degraded** from per-ref git history to commit-message trailers.
- **A7 — the broker becomes the journal of record**, contradicting §3.

Permitted only as an explicitly labeled single-writer experiment with enforced
disjoint ownership and a hard fail on prune/limits — and it offers nothing C's
inbox does not, while carrying the authority hazard.

## 13. Minimum broker-v2 capability for B

MUST:

1. **Multi-head project state**: named heads (at least `agents/<id>` per writer,
   plus `checkpoint`, `meta`), each with its own head sha.
2. **Per-head CAS**: `expected_head` scoped to one named head; a conflict on one
   head must not fail commits to another; fast-forward-only for owned heads.
3. **Per-head bootstrap**: `expected_head: null` creates that head only.
4. **Own-head rewrite** for REQ-11 prune, under CAS and atomic w.r.t. that head
   (an explicit replace operation, not an unconditional force).
5. **Per-head read**: state/inventory/blob/verify at exact commits.
6. **Authorization scoped to head prefixes**: a writer token may CAS only its own
   head(s).
7. **Per-head limits** and truncation behavior.
8. **Per-head op-id trailers + read-back verification** (the §6–§7 semantics).
9. **v1 compatibility or a migration path** for the derived `checkpoint` head.

NOT needed: cross-head transactions, per-path CAS, file deletes, global ordering,
multi-head atomicity.

## 14. C-now boundary

What may be used before B:

- **Read:** `read_state`/`read_blob`/`verify` for the derived tier; hydration only
  through a §8 freshness-marked, digest-verified projection.
- **Publish:** a separate post-journal step that, **after** the git checkpoint is
  committed and pushed, publishes the manifest + fixed-slot chunks in one CAS;
  idempotent by `op_id` + digest; never gates prune; fails closed on size.
- **Inbox:** a bounded `pending.json` per writer, unique writer ids, monotonic
  seqs, dedupe by watermark/key; drained by a single credentialed proxy that is
  the sole writer of the target agent ref (or a dedicated proxy ref) and prunes
  under §9. Receipt ≠ application; application is acked.
- **Not in C:** SyncManager write routing; broker reads for exclusivity; broker as
  prune gate; any delete; blind rebase over overlapping paths; multi-commit
  publishes; browse-tree mirroring.

## 15. B-later boundary

- `SyncManager` fetch/adopt and `push_agent_ref` may route to per-writer heads
  once §13 is deployed and §16's parity gate passes.
- The derived checkpoint head may remain as the read tier; the inbox may be
  retired or kept only as a fallback for writers without broker write scope.
- All C invariants (§3–§10) remain in force; B changes the CAS unit, not the
  authority model.

## 16. Exact condition under which SyncManager wiring becomes safe

**C-now:** `SyncManager` must not be wired to the broker at all for journal
writes. The broker may be used only by the §14 paths, and only when all of the
following are implemented and enforced in code: §5 ownership + rebase proof; §6
distinct replay-divergence outcome; §7 reconcile-required class covering stale,
timeout, and 502; §8 projection marker with fail-closed hydration; §9
delete-free conventions; §10 single-commit size fail-closed; backend identity
binding (project UUID ↔ repository identity); and the trait re-scoped/renamed as
a broker-v1 transport (not the Crosslink persistence seam) so the seam stays
unfrozen for B.

**B-later:** `SyncManager` may be wired to the broker exactly when broker v2
provides the §13 per-writer head CAS with own-head rewrite, **and** a parity gate
proves, per head: (a) writer-authoritative adoption of other agents' heads (own
head never moved by fetch) and watermark-based checkpoint adoption, matching
`fetch_and_adopt_v3_refs`; (b) `push_agent_ref` failure classes map exactly to
`PushOutcome::{Pushed, NonFastForward, NoRemote, Failed}`; (c) prune rewrites only
the owner's head under CAS and only after the git checkpoint push; (d) every write
outcome is landed / not-landed / unknown with `op_id` reconciliation; (e) identity
binding is enforced. Until both hold, any routing of journal reads or writes
through the broker is unsafe and out of scope.

## 17. Hardening gates (conditions, not implementation)

Before any broker write path is enabled: fix the `validate_message` non-ASCII
panic; add the reconcile-required class; make op-id-seen/content-differs a hard
outcome; make the projection marker mandatory; hard-fail corrupt backend config;
replace blind same-path rebase with the §5 proof; add the inventory↔blob and
`blob.commit` cross-checks; bind backend identity at construction; enforce the
journal high-water-mark check at append. These are independent of the mapping
decision but are prerequisites to exercising it.

## 18. Evidence, certainty, not-tested

- **WHY:** C is the only composition that preserves v3 per-agent concurrency,
  checkpoint determinism, and REQ-11 prune on the deployed broker, while still
  giving credential-less workers a mutation path; A deletes two v3 invariants,
  and B cannot exist until the contract changes.
- **WHAT:** broker v1 source (`state.ts`, `paths.ts`, `github.ts`, `README`,
  `SECURITY-NOTES`); v3 design + code (`hub_v3.rs`, `sync/cache.rs`,
  `compaction.rs`, `checkpoint.rs`, `events.rs`); the panel synthesis and all five
  reviews; measured local checkpoint size (1,846,959 bytes / 460 KiB gzip).
- **HOW CERTAIN:** high for broker v1 limits, CAS, no-delete, and v3 ref/CAS/
  prune/replay semantics (read from source); medium for the operational
  assumption that a credentialed publisher/drainer runs (not verified); medium
  for size headroom (one project measured).
- **WHAT-NOT-TESTED:** no live broker call; no inbox or publish implementation;
  no concurrent-publisher test; the panel's `validate_message` panic is
  reproduced but unfixed here; the §16 parity gate is defined, not executed.
