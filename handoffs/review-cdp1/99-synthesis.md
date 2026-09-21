---
issue: 802
title: Clean-room review of the CDP-1 derived-checkpoint publish protocol — synthesis
status: complete
panel: 3 reviewers (all completed after same-session recovery)
review_branch: design/pp3g-802-derived-publish
spec_revision_reviewed: bc39970d8 (.design/state-broker-derived-publish.md)
authority: .design/state-broker-authority-adr.md (ADR-802)
date: 2026-09-21
method: adjudicated synthesis (no majority voting; every disputed claim re-verified against the frozen spec, the code excerpts, and ADR-802)
---

# Panel synthesis — CDP-1 derived publish

Raw individual reports (unmodified): `01-hy3.md`, `02-qwen3.8-flash.md`,
`03-glm-5.3-flash.md`; dispatch/execution record `00-dispatch.md`; usage JSON
under `usage/`.

## 0. Panel execution

| # | Reviewer | Model ID | Variant | Attempts | Outcome | Cost |
|---|---|---|---|---|---|---|
| 1 | Hy3 | `opencode-go/hy3` | high | 1 | complete, exit 0 | $0.0491 |
| 2 | Qwen 3.8 Flash | `opencode-go/qwen3.8-flash` | xhigh | 3 (2 runs + 1 session continuation) | complete after recovery | $0.1116 |
| 3 | GLM-5.3-Flash | `opencode-go/glm-5.3-flash` | high | 4 (3 runs + 1 session continuation) | complete after recovery | $0.1775 |

Total: $0.3382, all OpenCode Go, all 0-day retention / not used for training.

Qwen's attempt 1 emitted a 9,243-char report truncated mid-sentence; attempt 2
emitted a preamble only; the complete report was assembled from attempt 1 plus a
same-session continuation that finished the remaining sections. GLM's three
runs each read the whole packet and emitted no assistant text; its report was
produced by a same-session continuation of attempt 2 with the evidence already
in context. Both recoveries were same-model, same-task, same-session; no
reviewer saw any other reviewer's output. Failure policy (one same-model retry
before substitution) was applied as frozen; no substitution was made. See
`00-dispatch.md` for the full execution history.

All three reviewers independently recomputed the capacity arithmetic and
confirmed the core safety properties (source authority, watermark monotonicity,
old-slot inertness, digest ladder, single-commit atomicity). No reviewer found a
case where an older or incomparable checkpoint could replace a newer valid
publish. The findings below are correctness and completeness defects at the
edges.

## 1. Findings adjudication

Legend: **H** = Hy3, **Q** = Qwen, **G** = GLM. Support: *supported* = verified
against the frozen spec/code/ADR; *partial* = the claim is directionally right
but its stated consequence is wrong; *unsupported* = not verifiable. Class:
*protocol defect* = would change runtime behavior if implemented as written;
*spec defect* = contradictory/incomplete normative text; *implementation
detail* = already gated in §12 or belongs to the implementation issue.

### S1 — Semantic identity is inconsistent across §2.4, §5.3, §5.7 (H-F1, Q-N2, G-B2) — **supported · protocol defect · blocking**

Two related contradictions, both verified in the frozen text:

- **Head classification:** §5.3 requires `head.W == W` **and**
  `source.commit == S` **and** `state_sha256 ==` for `AlreadyCurrent`;
  otherwise `Diverged`. §2.4 says "a differing payload with equal `W` and equal
  `state_sha256` is `AlreadyCurrent`, not divergence". The reachable case is a
  content-identical re-commit: a `state.json`-only bootstrap/migration
  checkpoint `S1`, then the first `compact_v3` adds the browse tree and produces
  `S2` with byte-identical `state.json` and the same watermark (the idempotency
  guard only fires when the browse tree is already present —
  `hub_v3-excerpts.rs`, checkpoint step). Publishing `S2` over a broker head
  carrying `S1` would be classified `EqualWatermarkDifferentContent` ⇒
  **terminal `Diverged`**, wedging the publisher on a legitimate transition.
- **Reconciliation:** §5.7 requires `manifest.payload_sha256 == payload_sha256`
  for `LANDED`, "otherwise ⇒ `DIVERGED`". §2.4 explicitly says a different
  compressor build yields different bytes with the same semantic identity and
  is `AlreadyCurrent`. A crash-recovery replay across a compressor/dependency
  change would therefore hard-block on a benign replay.

Severity disagreement: H rates this blocking; Q rates it non-blocking ("it
self-corrects on the bounded retry"). Q's mitigation is **wrong**: `Diverged` is
terminal by §5.7 and is never retried. G independently rates the same class
blocking. **Adjudicated: blocking.**

### S2 — Appendix B contradicts §6.2 on the pinned-commit and inventory checks (H-F2, G-N4) — **supported · spec defect · blocking-for-implementation**

§6.2 mandates `m_raw.commit != C ⇒ ProtocolMismatch` and the inventory↔blob
cross-check for the manifest and every chunk; the normative Appendix B
pseudocode omits both. An implementer following Appendix B would ship exactly
the TOCTOU/inventory-tear gap that T27/T39 claim to cover. Two reviewers found
it independently. **Adjudicated: blocking-for-implementation; small fix.**

### S3 — Repo↔project-UUID binding is deferred to a possible waiver (G-B1) — **supported · policy defect · blocking for L1**

ADR-802 §16 permits the §14 paths "only when all of the following are
implemented and enforced in code: … backend identity binding (project UUID ↔
repository identity)". CDP-1 is a §14 path, and §12 gate 9 currently says the
binding "must be resolved **or explicitly waived** for the first write". A
waiver is not "implemented and enforced in code"; the spec cannot authorize L1
under the binding ADR without an amendment. Manifest `project_uuid`/`state_ref`
are checked only against configuration, so a wrong-project publish passes every
check. **Adjudicated: supported; the waiver must be removed and the binding made
a hard pre-L1 gate (or ADR §16 formally amended).**

### S4 — Broker-only (Advisory) state can reach the hydration seam (Q-B1, G-N3) — **supported · spec defect · blocking-class correction**

§6.1 states the advisory rule in prose, but §6.2 returns an identical
`VerifiedCheckpoint` for both profiles, §9.3 step 7 (git cross-check) is
optional, and §9.1 feeds `checkpoint/state.json` to the existing
`read_checkpoint`/`hydrate_from_state` path. A broker-only, self-consistent
reconstruction can therefore drive authoritative SQLite (display IDs, locks,
tombstones). G independently observes that the manifest is unauthenticated and
that a forged self-consistent manifest passes all reader checks for a
broker-only consumer. Severity disagreement: Q blocking; G non-blocking
(ADR-802 bounds derived reads to advisory use). The damage *is* bounded in
principle, but the spec currently **authorizes** the unsafe path, so the fix is
required. **Adjudicated: required correction; small (provenance field + a
mandatory git check for hydration-eligible projections).**

### S5 — Wire-accounting fallback is wrong, and its resolution path is invalid (Q-B2, H-N1, G-assumption) — **partial · spec defect · non-blocking for the implementation issue**

- **Supported:** under a wire-based per-file cap, a 262,144-byte decoded chunk
  encodes to 349,528 wire bytes and violates the cap. §8.5 rescales only the
  commit budget (786,432 decoded / 782,336 payload) while keeping
  `SLOT_BYTES = 262,144`, so the fallback as written is internally inconsistent.
- **Supported:** the cited "§10 H6" does not exist (§10 is T01–T39), and T21 is
  a unit test; the loopback stub mirrors the client's decoded assumption
  (`validate.rs`, stub) and therefore cannot falsify it — only the deployed
  broker source or a live probe can.
- **Unsupported:** Q's claim that "the current 457 KiB hub is unpublishable
  under Wire". With the corrected wire model the hub still fits: decoded slot
  cap `196,608`, payload cap `782,336`, 4 slots; the current payload is 3 slots
  and ≈611.7 KB wire. H's "safe either way" is imprecise under the spec's
  *uncorrected* fallback; G's "internally coherent" is true only of the
  commit-budget line, not the slot size.
- **Not a safety defect:** the primary (decoded) model is supported by the
  client mirror derived from the broker source; the runtime failure mode is a
  clean `invalid_input` reject; the spec already gates near-boundary publishes.

**Adjudicated: correct §8.5 (shrink the wire slot size, recompute, remove the
dangling reference, point resolution at the deployed source or a reviewed live
boundary probe), and state §8.3 as decoded-contingent.**

### S6 — Definitive refusals collapse into `ReconcileRequired` (Q-N4, G-N1) — **supported · spec defect · non-blocking**

Appendix A maps every `Refuse(reason)` to `ReconcileRequired`, whose §5.6
definition is "unknown or unprovable; writes blocked". `CandidateStale`,
`WrongProject`, `HeadManifestUnreadable`, and `OverlapUnprovable` are provable
refusals, not unknowns; reporting them as blocked can spuriously gate subsequent
publishes. No write occurs either way. **Adjudicated: add a distinct `Refused`
outcome.**

### S7 — Reconcile verifies the whole owned namespace instead of the active paths (Q-N1) — **supported · protocol defect · non-blocking**

§5.7/Appendix A use `verify(carried_commit, owned_paths)` where the owned set is
the manifest plus slots `0000..0003`. For a publish with `n < 4` chunks, the
unlisted slots are absent at the superseded commit `X`, so "all match" fails and
a genuinely landed-and-superseded publish is mislabeled `NotLanded`. It
self-corrects on the bounded retry and loses no data, but the outcome label is
wrong. **Adjudicated: verify the request's active paths.**

### S8 — Slot namespace bound `0000..0031` (Q-N3, G-N5) — **supported · editorial · non-blocking**

§7 point 7 says "fixed at `0000..0031` (practically `0000..0003`)" while
`MAX_SLOTS = 4`. The 32-file commit cap is not the slot bound. **Adjudicated:
state `0000..0003` once.**

### S9 — Head manifest with a null watermark (H-N5) — **partial · clarification · non-blocking**

The manifest schema makes `source.watermark` mandatory and §4.3 M13 rejects a
null value, and §5.3 refuses invalid head manifests, so the gap is closed by
composition — but §5.3 does not say so explicitly. **Adjudicated: add one
sentence (validate the head manifest before comparing watermarks) plus a test.**

### S10 — Historical-commit reads are load-bearing but live-unverified (Q-N5, G-N2) — **supported · evidence gap · non-blocking**

Reader pinning and `LandedSuperseded` depend on the deployed broker serving
blobs/verify at non-head commits. Mock/stub cover it; the live smoke read at the
head only. If the deployment only serves head-pinned reads, `LandedSuperseded`
degrades to `NotLanded` — benign, but the taxonomy assumes the stronger
primitive. **Adjudicated: list as an explicit pre-L1 evidence gate.**

### S11 — Commit message template unspecified (G-N6) — **supported · spec completeness · non-blocking**

Appendix A calls `commit_message(S, W, chunks)` without a definition;
`validate_message` requires a single non-empty ≤512-char line with no C0/DEL
and no leading broker-trailer key. A naive multi-line serialization of `W` would
fail preflight (zero broker calls, but the spec claims complete preflight
coverage). **Adjudicated: define the template.**

### S12 — Equal-watermark divergence under version skew (G-N7) — **supported · operational note · non-blocking**

Two clients on different Crosslink versions publishing different `S` at the
same `W` genuinely differ in content, so the hard block is correct; but it
mutually wedges them until an operator reconciles. **Adjudicated: document as an
operational expectation.**

### S13 — Missing adversarial tests (H, Q, G) — **supported · test-plan gap · non-blocking**

Consolidated additions: content-identical re-commit ⇒ `AlreadyCurrent`;
same op-id with equal semantic identity but differing `payload_sha256` ⇒
landed/no-op; `LandedSuperseded` with `n < 4` active paths; vanished ref with
`expected_head != null` ⇒ unknown, never bootstrap; `Advisory` projection cannot
hydrate; null-watermark head manifest ⇒ refused; concurrent bootstrap / equal
watermark race with different `publisher_id`; wire-parameterized boundaries;
zip-bomb bound; projection path never re-publishable as a source; forged
self-consistent manifest is advisory-only; watermark ordering across agents;
manifest 4096/4097 at loopback level; op-id grammar boundary; commit-message
boundaries; T18 disambiguated by whether the error carried a commit sha.

### S14 — Evidence packet mixed two revisions (Q-N6) — **supported · packet limitation · no spec change**

The packet's `tests-state_broker_contract.rs` came from the base branch while
the adapter sources came from the hardening branch. Confirmed by diff (the
hardening contract test is history-aware and enum-shaped). This is a packet
construction limitation, not a CDP-1 defect; it does not change any finding
above (the spec defects are verified against the frozen spec text, and the
hardening sources used for code-fidelity claims were consistent).

## 2. What the reviewers confirmed

- **Source authority (focus 1):** all three high confidence. Copying the exact
  pushed blob keeps the broker strictly derived; §1.4's forbidden sources and
  P1–P5 make the derivation a pure function of one pushed git commit. No path
  from broker state back into publish inputs.
- **Watermark safety (focus 2):** no reviewer found a case where an older,
  incomparable, or incorrectly attributed checkpoint could replace a newer
  valid publish. The only defect is the false-`Diverged` direction (S1), which
  fails closed.
- **Capacity (focus 7):** independently recomputed by H and G and confirmed:
  `1,048,576 − 4,096 = 1,044,480`; `ceil(1,044,480 / 262,144) = 4`; slot 3 max
  258,048; 5 files ≤ 32; `262,144 → 1` chunk, `262,145 → 2`; `+1` fails; no
  off-by-one.
- **Chunking/old slots (focus 4), manifest detection (focus 5), reader pinning
  and digest ladder (focus 9), single-commit atomicity:** high confidence.
- **Hardening interaction (focus 10):** §12's gates correctly carry ADR-802 §17;
  the generic `commit_cas` is unsuitable and the spec says so.

## 3. Disagreements (explicit)

| # | Disagreement | Adjudication |
|---|---|---|
| D1 | S1 severity: H blocking vs Q non-blocking | **Blocking.** Q's "self-corrects" claim is wrong: `Diverged` is terminal. |
| D2 | S4 severity: Q blocking vs G non-blocking | **Required correction.** Damage is bounded by ADR, but the spec authorizes the unsafe path; fix is small. |
| D3 | Wire feasibility: Q "unpublishable" vs H "safe either way" vs G "internally coherent" | All three imprecise; corrected model fits the hub in 3 slots (~611.7 KB wire); the spec's fallback text is nonetheless wrong. |
| D4 | Core source-authority question | **No disagreement:** all three pass it. |

## 4. Verdict

**B — spec ready after named small corrections.** No redesign is required. All
three reviewers independently returned "ready after named corrections"; the
defects are contradictory or incomplete normative text (S1, S2, S5, S6, S7, S8,
S9, S11), one missing invariant (S4), one authority-gate error (S3), and test
gaps (S13). None changes the protocol's architecture: source-by-copy,
watermark-monotonic CAS, fixed-slot chunking, single-commit atomicity, and the
landed/not-landed/unknown trichotomy all survive review.

### Named corrections applied to the frozen protocol

| # | Correction | Findings |
|---|---|---|
| C1 | Define semantic identity as `(watermark, state_sha256)`; `source.commit`, `state_bytes`, `payload_sha256` are provenance. `AlreadyCurrent`/landed on semantic match; `Diverged` only on equal watermark + different `state_sha256`; a payload-digest difference alone is not divergence. | S1 |
| C2 | Bring Appendix B in line with §6.2: pinned-commit (`blob.commit == C`) and inventory↔blob checks for the manifest and every chunk. | S2 |
| C3 | Encode provenance (`JournalAnchored`/`Advisory`) on `VerifiedCheckpoint` and the v2 marker; make the git cross-check mandatory for any projection that feeds `hydrate_from_state`; Advisory projections are diagnostics-only and never decision-bearing. | S4 |
| C4 | Correct the wire fallback: `SLOT_BYTES_wire = 196,608`, payload max 782,336, 4 slots, 5 files; current hub 3 slots/≈611.7 KB wire; remove the nonexistent "§10 H6"; resolution = deployed broker source or a reviewed live boundary probe (L2), not the loopback stub; state §8.3 as decoded-contingent; parameterize M11/Appendix D. | S5 |
| C5 | Add a distinct `Refused` outcome; stop mapping definitive refusals into `ReconcileRequired`. | S6 |
| C6 | Reconcile verifies the request's active paths, not the whole owned namespace. | S7 |
| C7 | Slot namespace is `0000..0003` (`MAX_SLOTS`); the 32-file cap is separate. | S8 |
| C8 | State that the head manifest must validate per §4.3 (non-null watermark) before watermark comparison. | S9 |
| C9 | Repo↔project-UUID binding is a hard pre-L1 gate; remove the waiver option (ADR-802 §16). | S3 |
| C10 | Add the pre-L1 evidence gate for historical-commit reads; add the consolidated adversarial tests (T40–T55); disambiguate T18. | S10, S13 |
| C11 | Define the commit-message template (single line, ≤512 chars, no control characters, no leading trailer key). | S11 |
| C12 | Record the version-skew operational expectation; mark the resolved open questions (Q2, Q3, Q8) in §11. | S12 |

## 5. Residual uncertainty

- Deployed-broker limit accounting, historical-commit serving, and trailer
  behavior under contention remain unverified live (no reviewer could test
  them; the spec gates them before L1/L2).
- Compression byte-determinism across runtimes is asserted, not evidenced; the
  design does not depend on it after C1 (semantic identity governs).
- No concurrent-publisher evidence exists (ADR-802 §18); the equal-watermark
  race is addressed by policy and tests, not by measurement.
- The synthesis author (this document) is the same agent that authored the
  protocol; the independent verdict comes from the three reviewers, whose raw
  reports are preserved unmodified in this directory.

## 6. Post-synthesis action

1. C1–C12 were applied to `.design/state-broker-derived-publish.md`
   (documentation only) and committed with this synthesis as the frozen
   reviewed protocol at revision `7e3f71f94`.
2. The bounded implementation issue was opened as **#804** under #802 from that
   exact revision, carrying the §12 gate list and the T01–T55 test plan.
3. No implementation starts in this task.
