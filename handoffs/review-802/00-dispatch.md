---
issue: 802
title: Clean-room architecture review panel — dispatch record
status: dispatched (reviews in flight)
branch: feature/pp3g-state-broker-adapter
baseline: d323f020dd1a592113d71ebdb1facefb4b426a18
---

# Purpose

Independent clean-room architecture review panel for the state broker transport
adapter (issue #802), per operator request. Five models answer the same frozen
task in isolation; no reviewer sees another review before submitting.

# Target

- Branch `feature/pp3g-state-broker-adapter` vs baseline
  `feature/pp3g-Zbk6-cleanup-rescope-349` @ `d323f020dd1a592113d71ebdb1facefb4b426a18`.
- Broker contract v1 source included as evidence.

# Panel and dispatch metadata

Catalog refreshed `2026-09-21` (`opencode models --verbose --refresh`); all
models confirmed present and `status: active` before launch. Privacy/retention
facts as published on 2026-09-21 (Go and Zen pricing/privacy tables).

| # | Model (as requested) | Model ID | Provider | Variant | Privacy/retention |
|---|---|---|---|---|---|
| 1 | GLM-5.3-Flash | `opencode-go/glm-5.3-flash` | OpenCode Go | `high` | 0-day retention, not used for training |
| 2 | Qwen 3.8 Flash | `opencode-go/qwen3.8-flash` | OpenCode Go | `xhigh` | 0-day retention, not used for training |
| 3 | Big Pickle | `opencode/big-pickle` | OpenCode Zen | default | Free period: collected data may be used to improve the model |
| 4 | Nemotron Ultra | `opencode/nemotron-3-ultra-free` | OpenCode Zen | default | NVIDIA trial terms: logged, used to improve products/services |
| 5 | Muse Spark 1.3 | `opencode-go/muse-spark-1.3-contributor` | OpenCode Go | `high` | Contributor tier: prompts/completions may train future Meta models; not ZDR |

Runtime: `opencode run --model <id> [--variant <v>] --dir <packet> --format json`
(runs autonomously to completion; tools confined to the packet directory).

# Clean-room and safety controls

- **Same frozen task** `TASK.md` (Q1–Q11) and the **same evidence set** for
  every reviewer.
- **Sanitized packet** used uniformly because of the mixed provider data-use
  policies above: absolute paths, organization/repository/project names, a
  project UUID, a baseline sha, and prior-review operational metadata removed.
  The full architecture, interfaces, code, tests, diff, and questions were
  retained; nothing was weakened. See `SANITIZATION-NOTES.md` in each packet.
- Each reviewer gets its **own copy** of the packet (`reviewer-N/packet/`) so
  sessions cannot interfere.
- **Read-only**: prompts forbid file modification; non-interactive permission
  handling auto-denies writes; no reviewer is asked to implement fixes.
- **No live broker writes or reads**: prompts forbid live broker operations;
  the live probe test is `#[ignore]`d and reviewers are told not to run it.
- Earlier prior review (`handoffs/802-review-hy3.md`) included as a claim set to
  confirm or refute independently, not as ground truth.
- **Failure policy**: on failure, retry once with the same task before any
  substitution; failures and disagreements are reported, not smoothed.

# Artifacts

- Packet: `/tmp/opencode/review-802/packet/` (identical copies under each
  `reviewer-N/`).
- Raw run output per reviewer: `out.jsonl` (JSON events), `err.txt`, `exit.txt`,
  `meta.txt` (timestamps), `review.md` (extracted final report), `usage.json`
  (model, tokens, cost).
- Individual reports and the synthesis will be stored under
  `handoffs/review-802/` in the review worktree.

# Status

- [ ] reviewer-1 GLM-5.3-Flash
- [ ] reviewer-2 Qwen 3.8 Flash
- [ ] reviewer-3 Big Pickle
- [ ] reviewer-4 Nemotron Ultra
- [ ] reviewer-5 Muse Spark 1.3
- [ ] synthesis
