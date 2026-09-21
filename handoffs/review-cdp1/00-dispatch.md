---
issue: 802
title: Clean-room review of the CDP-1 derived-checkpoint publish protocol — dispatch record
status: in progress (reviews in flight)
branch: design/pp3g-802-derived-publish
spec_revision: bc39970d8 (.design/state-broker-derived-publish.md)
authority: .design/state-broker-authority-adr.md (ADR-802)
date: 2026-09-21
---

# Purpose

Independent clean-room review of the proposed CDP-1 derived-checkpoint publish
protocol before its implementation issue is opened. Three models answer the same
frozen task in isolation; no reviewer sees another review before submitting.

# Target (frozen)

- Spec: `.design/state-broker-derived-publish.md` @ `bc39970d8`
  (branch `design/pp3g-802-derived-publish`, base
  `feature/pp3g-state-broker-adapter` @ `48b503ec6`).
- Authority: `.design/state-broker-authority-adr.md` (ADR-802).
- Evidence packet (sanitized, identical copies):
  `/tmp/opencode/review-cdp1/reviewer-{1,2,3}/packet/` — aggregate content hash
  `16e66c7864324f9e6685140919c9fde8085efcaa9c6ee67b6d393fa09c08db6b`
  (per-copy, paths excluded). Contains: CDP-1 spec, ADR-802, transport design,
  live read-only smoke record, v3 requirements, hardening-branch
  `state_broker/*` sources, `checkpoint.rs`, `hub_v3.rs` excerpts
  (`compact_v3`, checkpoint commit core, prune, tree core), `sync/cache.rs`
  excerpts, broker contract tests, and measured checkpoint size evidence.

# Panel and dispatch metadata

Catalog refreshed 2026-09-21 (`opencode models --verbose --refresh`); all three
confirmed `status: active`. Pricing/privacy from the official Go table
(<https://opencode.ai/docs/go>, fetched 2026-09-21); the local catalog matched
the table for all three.

| # | Model (as requested) | Model ID | Provider | Variant | In / Out / Cache-read / Cache-write | Monthly | Privacy |
|---|---|---|---|---|---|---|---|
| 1 | Hy3 | `opencode-go/hy3` | OpenCode Go | `high` | $0.14 / $0.58 / $0.035 / — | $60 | not used for training, 0-day retention |
| 2 | Qwen 3.8 Flash | `opencode-go/qwen3.8-flash` | OpenCode Go | `xhigh` | $0.15 / $0.47 / $0.016 / $0.20 | $30 | not used for training, 0-day retention |
| 3 | GLM-5.3-Flash | `opencode-go/glm-5.3-flash` | OpenCode Go | `high` | $0.15 / $0.50 / $0.03 / — | $60 | not used for training, 0-day retention |

Operator approval: **per-launch, requested via the question tool before
dispatch** (model, effort, deliverable, costs, privacy). Approved: Hy3 `high`,
Qwen 3.8 Flash `xhigh` (operator re-confirmed `xhigh` after being told the
model exposes no `high` variant), GLM-5.3-Flash `high`. No substitution or
escalation without a fresh approval.

Runtime: `opencode run --model <id> --variant <v> --dir <packet> --format json`
(non-interactive; writes are auto-denied by default, prompts additionally forbid
them). One process per reviewer, launched in parallel with isolated packet
copies; no reviewer session is continued into another.

# Clean-room and safety controls

- Same frozen `TASK.md` and the same evidence set for every reviewer.
- Uniform sanitization (project UUID, shas, token id, broker host, org names,
  absolute paths replaced; structure/sizes/code/questions retained). See each
  packet's `SANITIZATION-NOTES.md`.
- Each reviewer has its own packet copy under `reviewer-N/`.
- Prompts forbid file modification, builds, tests, git commands, network calls,
  and any broker operation; this is a design review, not an implementation task.
- No reviewer sees another review before submitting; the synthesis runs only
  after all three complete.
- Failure policy: on failure, one retry with the same task and model before any
  substitution; failures and disagreements are reported, not smoothed.

# Artifacts

- Packet: `/tmp/opencode/review-cdp1/reviewer-{1,2,3}/packet/`.
- Raw run output per reviewer: `out.jsonl` (JSON events), `err.txt`, `exit.txt`,
  `meta.txt`, `review.md` (extracted final report), `usage.json`.
- Individual raw reports in this directory: `01-hy3.md`, `02-qwen3.8-flash.md`,
  `03-glm-5.3-flash.md`; synthesis `99-synthesis.md`; usage under `usage/`.

# Status

- [x] reviewer-1 Hy3 — complete, exit 0, single attempt
      (8694 chars; 164,142 in / 3,216 out / 693,568 cache-read; $0.0491)
- [x] reviewer-2 Qwen 3.8 Flash — complete after recovery
      (attempt 1 emitted a 9,243-char report truncated mid-sentence; attempt 2
      emitted only an 86-char preamble; the complete report was assembled from
      attempt 1 plus a same-session continuation of attempt 1 that finished the
      remaining sections: 16,589 chars total; combined 132 in / 11,587 out /
      1,618,509 cache-read / 165,876 cache-write; $0.1116)
- [x] reviewer-3 GLM-5.3-Flash — complete after recovery
      (attempts 1–3 each read the full packet but emitted no assistant text;
      the report was produced by a same-session continuation of attempt 2 with
      the evidence already in context: 12,329 chars; combined 560,845 in /
      12,819 out / 2,899,456 cache-read; $0.1775; attempt 3 ran under
      `--format default` and its token accounting is unavailable)
- [x] synthesis — `99-synthesis.md`

Total panel cost: $0.3382 across the three reviewers (all OpenCode Go, all
attempts included).

# Execution history (failures and recoveries)

| Reviewer | Attempt | Mechanism | Outcome |
|---|---|---|---|
| Hy3 | 1 | `--format json`, high | complete report |
| Qwen | 1 | `--format json`, xhigh | report emitted, truncated mid-sentence |
| Qwen | 2 | `--format json`, xhigh, concise instruction | preamble only |
| Qwen | 1-cont | `--session` continuation of attempt 1 | completed the report |
| GLM | 1 | `--format json`, high | no assistant text (23 reads) |
| GLM | 2 | `--format json`, high, retry | no assistant text (27 reads) |
| GLM | 3 | `--format default`, high (operator-approved) | no assistant text (23 reads) |
| GLM | 2-cont | `--session` continuation of attempt 2 | complete report (`reason: stop`) |

Failure policy applied as frozen: one same-model retry before any substitution;
no substitution was made; both recoveries were same-model, same-task,
same-session continuations, so no reviewer saw any other reviewer's output and
no clean-room boundary was crossed. Raw event streams for every attempt remain
under `/tmp/opencode/review-cdp1/reviewer-N/` (`out*.jsonl`, `out-default.txt`,
`review-attempt*.md`, `meta*`, `exit*`).

Raw reviewer outputs: `01-…`–`03-…` in this directory; usage JSON under
`usage/`. Packet aggregate content hash:
`16e66c7864324f9e6685140919c9fde8085efcaa9c6ee67b6d393fa09c08db6b`.

No reviewer saw another review before submitting. No live broker operation was
performed. No repository file was modified by any reviewer (all three packet
copies re-hashed to the frozen value after the runs).
