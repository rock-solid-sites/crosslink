---
issue: 802
title: Clean-room architecture review panel — synthesis
status: complete
panel: 5 reviewers (1 substitution)
review_branch: feature/pp3g-state-broker-adapter
baseline: d323f020dd1a592113d71ebdb1facefb4b426a18
method: adjudicated synthesis (no majority voting; disputed claims verified against code)
---

# Panel synthesis — state broker transport adapter (#802)

Raw individual reports (unmodified): `01-glm-5.3-flash.md`,
`02-qwen3.8-flash.md`, `03-big-pickle-substitute-mimo.md`,
`04-nemotron-ultra.md`, `05-muse-spark-1.3.md`; dispatch record
`00-dispatch.md`; usage JSON under `usage/`.

## 0. Panel execution, failures, and integrity

| # | Reviewer | Model ID | Variant | Exit | Duration | Tokens (total) | Cost | Notes |
|---|---|---|---|---|---|---|---|---|
| 1 | GLM-5.3-Flash | `opencode-go/glm-5.3-flash` | high | 0 | ~9 min | 2,716,387 | $0.1204 | complete |
| 2 | Qwen 3.8 Flash | `opencode-go/qwen3.8-flash` | xhigh | 0 | ~33 min | 4,108,148 | $0.1227 | complete |
| 3 | Big Pickle → MiMo V2.5 Free | `opencode/big-pickle` → `opencode/mimo-v2.5-free` | default | 1 | ~1 min ×2 + ~4.5 min | 912,417 | $0 | **Big Pickle failed twice** (`403: OpenCode's free tier can only be used from within OpenCode`, attempts 1 and retry). Substituted per the stated policy after the retry. MiMo emitted its full report, then hit the same 403 on a post-report request; report recovered from the event stream. |
| 4 | Nemotron Ultra | `opencode/nemotron-3-ultra-free` | default | 0 | ~6 min | 1,205,010 | $0 | **Rule violation, contained**: wrote `REVIEW.md` into its own packet copy (read-only rule). Report recovered from that file; no repo or shared state was touched. |
| 5 | Muse Spark 1.3 | `opencode-go/muse-spark-1.3-contributor` | high | 0 | ~9 min | 731,590 | $0.0180 | complete |

Total ~9.67M tokens, ~$0.26 billed.

- All five received the **same frozen `TASK.md`** (Q1–Q11 verbatim) and the
  **same sanitized packet**; no reviewer saw another review before submitting.
- Sanitization applied uniformly because Big Pickle, Nemotron, and Muse provider
  policies permit data use beyond ZDR/training; the packet removes absolute
  paths, org/repo/project names, a project UUID, a baseline sha, and prior
  operational metadata, while retaining every architecturally relevant file and
  the unmodified questions. See each packet's `SANITIZATION-NOTES.md`.
- No live broker operations occurred. Read-only enforcement was 4/5 at the file
  layer; the one write was contained to the reviewer's own disposable copy.
- No reviewer was asked to implement fixes; none did.

## 1. Findings agreed by most or all reviewers

1. **The adapter is additive and preserves existing behavior today.** Default
   `Local`, no production write call sites, read-only CLI only. (All 5.)
2. **`ProjectStateTransport` is a faithful port of broker v1, not a
   Crosslink-domain persistence seam.** Its vocabulary — one whole-tree
   `expected_head`, 40-hex commit shas, `Broker-Op:` trailer reconciliation,
   exact-commit `verify` — encodes the broker's model and implicitly
   pre-answers the deferred mapping decision. The naming/docs overstate the
   abstraction. (GLM, Qwen, Muse, Nemotron explicit; MiMo: right location,
   over-abstracted trait.)
3. **Do not wire `SyncManager` after the live smoke test.** The smoke test is
   read-only and proves transport conformance only; the mapping/invariant
   decision must come first. (All 5.)
4. **Option A (per-agent refs as files in the one tree) is not a viable
   steady-state model**: whole-tree CAS serializes all writers, the 32-file /
   256 KiB / 1 MiB limits dead-end mature event logs, and v1 has no delete, so
   REQ-11 prune is unrepresentable. (GLM, Qwen, Muse, Nemotron explicit; MiMo
   equivalent analysis.)
5. **`commit_cas` is not safe for append/shared-path semantics as-is** — see
   finding 2 of §2. The "whole-file upserts only" precondition is documentation,
   not enforcement, and the one competing-writer test uses disjoint paths.
   (GLM, Qwen, Muse independently; Nemotron's checkpoint-only framing avoids it
   by design.)
6. **The projection is not self-identifying or freshness-bound.** Nothing
   persists the broker head beside the projected files; the existing SQLite
   guards protect against emptiness, not staleness/partiality. Any future
   consumer that treats the directory as a cache dir can silently regress
   SQLite. (GLM, Qwen, Muse; Nemotron notes the projection path is read-only
   today.)
7. **Unnecessary machinery exists**: mock + HTTP stub both reimplement broker
   CAS; client-side validators are re-derived in the stub (a third copy);
   several public items are unused (`transport_from_env`, `BrokerErrorCode::ALL`,
   `StateBackend::into_broker`, `StateBlob::text` in production); the lib/bin
   double tree forces the re-export allow-block. (GLM, Qwen, Muse; MiMo partial.)
8. **No-delete accounting is missing** (tombstones, garbage bound, prune).
   (GLM, Qwen, Muse, Nemotron.)

## 2. Findings raised independently by multiple reviewers

1. **Lost update on same-path rebase in `commit_cas`** (GLM §Q6.1, Qwen §Q6.1,
   Muse G1). On `stale_state` without our op id, the retry re-issues the same
   absolute payload against the new head; if the competing commit touched the
   same path, the retry overwrites it and reports `verified: true`. The single
   rebase test writes a *different* path and asserts survival — it dodges the
   lossy case. **Adjudication: supported** (`transport.rs` retry loop re-issues
   `current` with only `expected_head` changed; `CommitRequest` carries no
   per-path precondition).
2. **`already_applied: true` can be returned with `verified: false` as `Ok`**
   (GLM §Q6.2, Qwen §Q6.3). Callers that match on `Ok`/`already_applied` can
   wrongly conclude success; `client.commit` itself treats `verified: false` as
   a protocol error, so the two paths disagree. **Adjudication: supported**
   (`CasResolution` has no distinct "op-id seen but content differs" variant).
3. **Projection-authority hazards** (GLM §Q5.1–3, Qwen §Q5.1–3, Muse P1–P3):
   non-atomic writes with no completeness marker; reused authoritative file
   names; possible reuse of the `record_hydrated_ref` marker format across two
   different sha namespaces; unconstrained `hydrate_into` target directory.
   **Adjudication: supported as latent hazards** (no current caller does this;
   nothing prevents it).
4. **Silent backend revert on corrupt `hook-config.json`** (GLM §Q5.2, Muse P3):
   unparsable config → `None` → `Local` with only a warning, contradicting the
   design record's "no silent fallback". **Adjudication: supported**
   (`read_hook_config_backend`), and the design doc's wording is the thing to
   fix (either hard-error when a selection key is suspected, or state the
   fail-safe-to-local rule explicitly).
5. **Mock fidelity diverged from the broker after the stub was fixed**
   (Muse §2, Qwen §Q9.6): the mock still rejects `verify`/`read_blob` at
   non-head commits, unlike broker `state.ts`, and skips duplicate-path/limit
   checks. **Adjudication: supported** (mock.rs non-head rejection is by design
   but now inconsistent with the stub's history model).
6. **Missing read-back-mismatch / lost-write-response reconciliation**
   (GLM §Q6.3, Qwen §Q6.5): broker §502 "write may have landed" and transport
   timeouts on `POST` all require op-id reconciliation, but `commit_cas` only
   reconciles `stale_state`. **Adjudication: supported** (contract requirement
   without provided machinery).
7. **Path-ownership / op-id uniqueness invariants unstated and unenforced**
   (GLM §Q8.3, Qwen §Q8.1–2, Muse §Q8.3). **Adjudication: supported**;
   these are the preconditions that make rebase safe at all.

## 3. Important unique findings

- **Qwen: `validate_message` panics on non-ASCII messages (blocker).** The
  trailer check slices by byte length after a char-count check
  (`trimmed[..trailer.len()]`), so any message whose first 7 bytes end inside a
  multi-byte character panics before the case-insensitive compare. I reproduced
  the exact logic with `rustc`: `"éééé"` and `"€€€ broker note"` both **panic**.
  Reachable from any caller-supplied message through `CommitRequest::validate`
  → `client.commit`. The stub contains the same pattern. No test uses non-ASCII
  messages. (Neither Hy3 nor my own prior review found this.)
- **Qwen: inventory↔blob cross-check absent.** `hydrate_into` pins blobs to the
  inventory commit but never compares the returned `sha256`/`size`/`blob_sha`
  against the `StateEntry` it listed, and the client discards `blob.commit`.
  A broken broker can swap content at a path without detection (the client's
  self-consistent digest check cannot notice).
- **Qwen: head-vanished bootstrap escalation.** If the ref disappears between
  the 409 and the re-read, the retry sends `expected_head: null` and creates a
  fresh history containing only the caller's files. Real code path; severity
  depends on administrative deletion semantics.
- **Qwen: redirect behavior concern (adjudicated unsupported).** The claim that
  a cross-host redirect could replay `Authorization` is **not supported**:
  reqwest 0.12.28 strips `Authorization`/`Cookie`/`Proxy-Authorization` on
  cross-host redirects (vendored `redirect.rs:239-251`). Residual: same-host
  redirects retain the header, and an explicit no-redirect policy would make the
  property mechanical.
- **GLM: Windows-reserved-name divergence.** `projection_relative_path` rejects
  reserved names (`aux`) that the broker's path grammar accepts, so a
  broker-legal state file can be un-hydratable by the client.
- **GLM: `UpstreamError` client default is retryable=true while the broker's own
  default is false.** Masked today because envelopes always set the flag, but
  the client's fallback errs in the unsafe direction for writes.
- **GLM: torn-projection claim (adjudicated unsupported).** `hydrate_into` pins
  every blob to the head commit captured at the start (`transport.rs:189`), so a
  concurrently moving head cannot tear the projection. What is missing is a test
  asserting that pinning, not the guarantee itself.
- **Muse: verify-after-read TOCTOU (G3)** — `verify` at the old head can return
  `already_applied` for a commit that has since been superseded; no head
  re-check.
- **Muse: fabricated `previous_head: None` (G2)** in the reconciled outcome
  loses lineage information.
- **Muse: no-delete/tombstone gap, retry budget, narrow verify scope (G4/G5).**
- **Nemotron: option C as the checkpoint-mirror recommendation** (see §5 for
  adjudication), including "broker = derived view, not authoritative write
  path" and the lag/compaction-cycle consequences.

## 4. Direct disagreements and adjudication

### D1. Which option resolves the mismatch (the central disagreement)

| Reviewer | A | B | C | D | Recommendation |
|---|---|---|---|---|---|
| GLM | reject (corrosive; limits/prune dead-end) | end-state | **C now** as "read projection + write inbox" | too early (forces mapping under no-redesign) | C now, B tracked as v2 |
| Qwen | only as explicitly labeled single-writer stopgap | **B is cleanest** | only as a separate driver/topology decision | necessary hygiene, not an answer | B (with D hygiene) |
| Muse | reject | tracked long-term | read-tier only, never the journal | **D now**; writes stay on git until B | D now, B later |
| Nemotron | reject | speculative | **C now** (checkpoint mirror) | blocked by v1 | C |
| MiMo | correct only for ≤1 active writer | needs contract v2 | changes hub to snapshot-based at the broker boundary | — | decision document first |

**Adjudication (code-grounded).** These positions are less contradictory than
they appear; they differ on two axes.

- *Axis 1 — is C acceptable as policy?* Muse's "C read-tier only" and GLM's
  "read projection + inbox" are the same policy: the broker must never be the
  event log of record. Nemotron's "checkpoint mirror" is that policy without
  specifying worker-originated writes; GLM correctly identifies that gap (an
  inbox or a credentialed drain step is required). Qwen's objection to C ("a
  designated writer becomes the serialization point and writer of record") is
  the strongest point: C is only safe while agents keep writing git refs and the
  broker receives *derived* state. Verified: v3 retains per-agent writes and
  REQ-11 prune in the current tree, and the adapter has no code that would
  change that.
- *Axis 2 — should D be done now?* GLM calls D premature because it would force
  the mapping decision and refactor `SharedWriter`/`SyncManager` under the
  no-redesign constraint. That is true for D *as full routing*, but not for the
  hygiene half (rename/re-scope the trait, move `hydrate_into` out of the
  transport trait) — which is verified to be a small, local change
  (`hydrate_into` only composes `read_state` + `read_blob` + `std::fs::write`;
  the provided `commit_cas` embeds broker trailer parsing in the trait,
  `transport.rs` reconciliation).

**Verdict:** all four options are being proposed for different layers. The
coherent composite — and my recommendation — is: **C as the interim semantic
policy (broker = derived read projection, optional inbox for credential-less
writers), D-hygiene as a scoping correction (trait is a broker-v1 transport,
not the Crosslink persistence seam), B as the only faithful end-state before
any hub write routing, A rejected as steady state and permitted only as an
explicitly labeled single-writer experiment.** This is not a compromise: each
element is supported by the verified v3 semantics (per-agent single-writer
authority, prune, event-sourced journal) and by broker v1's actual contract.

### D2. Is `ProjectStateTransport` the right boundary?

MiMo: right location, over-abstracted. GLM/Qwen/Muse/Nemotron: it is broker-v1
renamed. **Adjudication: both are partially right, and they are compatible
once the trait is re-scoped.** The *location* (one seam, one client, default
off) is correct and verified (only the read-only CLI consumes it). The *claim*
(wraps `mod.rs`/`transport.rs` doc comments) is not: the provided methods embed
broker-specific conventions, so any substitute backend must reproduce them.
Renaming/re-scoping resolves the disagreement without redesign.

### D3. Projection pinning/torn reads (GLM vs code)

GLM claims the projection can tear because the head may move between blobs.
**Adjudication: unsupported as a defect.** `hydrate_into` reads the state once
and pins every `read_blob` to `Some(&head.commit)`. The valid residue is a
missing pinning test.

### D4. Cross-host redirect token exposure (Qwen vs reqwest source)

**Adjudication: unsupported.** reqwest strips sensitive headers cross-host
(verified in the vendored source). Residual: set an explicit no-redirect policy
or document the dependency behavior.

## 5. Findings already covered by the prior review (Hy3) vs genuinely new

**Hy3-covered, now independently re-verified fixed by three reviewers (GLM,
Qwen, Muse):** presence-based `verified` (major) → digest compare; stub
fidelity; `expose()` visibility; C1 control-char rule. No reviewer disputed the
fixes.

**Genuinely new in this panel (not in Hy3):**

1. `validate_message` panic on non-ASCII input (Qwen; blocker).
2. Same-path rebase lost update in `commit_cas` (GLM, Qwen, Muse; highest
   semantic severity).
3. `Ok` + `verified:false` outcome incoherence (GLM, Qwen).
4. Read-back-mismatch / transport-timeout "reconcile-required" gap (GLM, Qwen).
5. Projection authority/freshness/completeness class (GLM, Qwen, Muse).
6. Silent backend revert on corrupt hook-config (GLM, Muse).
7. Mock-vs-stub fidelity divergence (Muse, Qwen).
8. Head-vanished bootstrap escalation (Qwen).
9. Inventory↔blob cross-check absent (Qwen).
10. Windows-reserved-name projection divergence (GLM).
11. Retryability-default direction (GLM).
12. Verify-after-read TOCTOU and fabricated `previous_head` (Muse).
13. Unused public API surface and triplicated validation (GLM, Qwen, Muse).

## 6. Is +5515/−1 justified, reducible, or over-abstraction?

- **Justified core (~55–60%)**: HTTP client, typed errors/redaction, config and
  secret handling, contract validators, the transport seam, and the contract
  test suite's broker-side modeling. The change is additive; the diffstat is
  dominated by new files, and existing code changed by ~37 lines.
- **Reducible (~800–1,200 lines, no correctness loss)**: one of the two fakes
  (mock vs HTTP stub) made faithful and kept; the stub's third copy of broker
  validators replaced by shared test fixtures derived from the broker's own TS
  tests; unused public items removed (`transport_from_env`,
  `BrokerErrorCode::ALL`, `StateBackend::into_broker`); `digest.rs` folded;
  `VerifiedFile`/`VerifiedEntry` unified; doc prose trimmed.
- **Over-abstraction verdict: not in size, but in naming/scope.** The trait's
  docs claim a general persistence seam while implementing broker v1. That is a
  framing defect, not an architecture defect, and it is cheap to correct —
  which matters, because the framing is what would calcify option A if a write
  path were wired on top of it.

## 7. Focused comparison of A/B/C/D (semantic consequences)

| | Concurrency | Prune/delete | Limits | Broker authority | Fit |
|---|---|---|---|---|---|
| **A** refs-as-files | all writers serialize on one head | impossible (no delete; logs grow) | event logs exceed 256 KiB/file; 32-file/1 MiB choke | becomes event-log of record | hostile to v3; only labeled single-writer stopgap |
| **B** per-agent heads (v2) | per-writer CAS preserved | per-agent history rules preserved | limits per ref, not per tree | faithful | only option preserving v3 1:1; needs contract v2 + redeploy |
| **C** checkpoint mirror (+inbox) | git stays concurrent; broker receives derived snapshots | agent refs prune locally; broker = snapshot | single checkpoint file(s) fit limits | derived, not authoritative | implementable on v1 today; requires inbox convention for credential-less writes |
| **D** boundary re-scope | neutral (hygiene) | neutral | neutral | hides broker behind Crosslink ops | prerequisite for honest decision; full routing requires the decision |

Refinement that the panel converged on (GLM explicitly, Muse implicitly):
**C and B are not exclusive** — C is the v1-compatible derived tier and B is
the v2 end-state; D-hygiene is orthogonal. Nothing in this composition requires
redesigning hub v3.

## 8. Invariants that must be settled before any `SyncManager` wiring

1. **CAS unit and path ownership**: which paths are agent-private, which are
   shared, and whether `commit_cas` may rebase a path at all (refuse on
   overlap, or require per-path preconditions).
2. **Op-id convention**: uniqueness scope (agent + monotonic sequence),
   meaning of `already_applied` and of `verified: false` on that path, and a
   distinct outcome for "our op id, different content".
3. **Reconcile-required class**: writes (including timeouts and read-back
   mismatches) must be expressible as "landed / not landed / unknown — reconcile
   by op id", and `commit_cas` must cover all three, not only `stale_state`.
4. **Projection identity/freshness**: persist the broker head (and completeness
   marker) beside a projection; hydration must refuse on stale/partial input.
5. **Delete/tombstone accounting**: what a deletion means in a no-delete
   namespace, and the size/prune budget for broker state.
6. **Backend identity binding**: the selected broker project UUID must be
   checked against the repository's project identity at transport construction,
   and freshness markers must be namespaced per backend.
7. **Backend-selection stability**: one backend per invocation; corrupt config
   when a backend was selected must hard-fail (or the fail-safe-to-local rule
   must be explicit and surfaced).
8. **Mutation atomicity at the logical layer**: multi-commit sequences (limits
   force them) need a continuation/torn-write rule or an explicit non-goal.

## 9. Recommended next step for the operator

**No `SyncManager` wiring. No new write path.** In order:

1. **Immediate hardening (small, independent of the mapping decision):**
   fix the `validate_message` panic (blocker); make same-path rebase refusal or
   merging explicit in `commit_cas`; add a distinct outcome for
   op-id-seen/content-differs; add projection head/completeness markers;
   hard-fail on corrupt config when a backend was selected; add the missing
   mock/stub parity and the overlap/replay tests; flip the `upstream_error`
   retryability default. These were all identified by the panel and are
   independently verifiable.
2. **Run the read-only live probe** (`state-broker status --json` + the ignored
   live test) — orthogonal, safe, closes the largest evidence gap.
3. **Write the one-page mapping decision record** (ADR) that this change
   deferred: choose the composite above (C as interim derived tier + inbox,
   D-hygiene now, B as the v2 goal, A rejected as steady state), and settle the
   §8 invariants. The decisive test in the ADR should be the concrete
   write-path walkthrough GLM proposed: how one agent appending one event
   becomes a broker commit, including conflict, replay, limits, and prune — its
   predictable failure rules out A and forces C-vs-B explicitly.
4. Only after that ADR: any `SyncManager` routing, starting with per-agent
   head support (B) or the derived-checkpoint publish step (C), with the
   panel's overlap/replay/lost-response tests as the gate.

## 10. Confidence and limits of this synthesis

- I verified every disputed/uncertain claim I could: the panic (reproduced with
  the exact logic via `rustc`), reqwest redirect behavior (vendored source),
  hydrate pinning, mock non-head rejection, unused API surface, Windows-reserved
  divergence, and the baseline-gating gap.
- I did not re-run the panel or modify code; all reviewer outputs are stored
  verbatim as linked above. Reviewer reports were read in full or in the cited
  sections; anything a reviewer marked unverified remains so.
- Remaining uncertainty: the v3 excerpts the reviewers used were curated by me,
  and the reviewers flagged this (Qwen medium confidence on Q4 details). The
  mapping decision should be taken with the full `hub_v3.rs`/`sync/*` sources,
  not the excerpts.
