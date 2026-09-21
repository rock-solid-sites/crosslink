---
issue: 802
title: Independent review — state broker transport adapter (Hy3)
status: complete — findings triaged and resolved
reviewer_model: opencode-go/hy3
reviewer_provider: opencode-go
reasoning_effort: default
catalog_refreshed: 2026-09-21
pricing_checked: 2026-09-21 (https://opencode.ai/docs/go)
pricing_compared: |
  hy3 $0.14 in / $0.58 out / $0.035 cache-read, monthly $60, 0-day retention (selected)
  mimo-v2.5-pro $0.435 / $0.87 / $0.003625 cache-read, monthly $15
  kimi-k2.7-code $0.95 / $4.00 / $0.19 cache-read, monthly $60
  nemotron-verifier free tier ($0)
approval: operator selected "Hy3" via the question tool (per-launch verbal approval)
dispatch: |
  opencode run --model opencode-go/hy3, read-only copy of the worktree at
  /tmp/opencode/hy3-review with the broker source vendored in; prompt forbade
  writes and builds; two attempts (first was blocked by external-directory
  permissions, corrected by vendoring the broker sources; same model/task).
---

# Independent review (Hy3) — raw report

I'll review this systematically. Let me start by reading the core source files and the authoritative contract in parallel.
Now let me read the transport, mock, digest, and tests files, plus the authoritative contract files.
Now the authoritative contract files and remaining scope.
Let me read the tests, commands, design doc, and remaining vendor contract files.
Now the design doc, vendor path/error files, commands, and the integration-point files (cache.rs, hub_v3.rs).
Let me verify the design doc's integration-point claims against the actual code, and check the wiring/CHANGELOG/docs.
Let me check the test counts claimed in the design doc and the main.rs command wiring for any leak.
## Verdict
HAS FINDINGS

## Findings

**major — `commit_cas` `already_applied` reports `verified` from presence, not content**
`crosslink/src/state_broker/transport.rs:265-287` (line 275: `verified: entry.present`). When a `stale_state` is reconciled as `already_applied`, the code builds `VerifiedFile` entries with `verified` = whether the path *exists* at the head. It never compares the returned `sha256`/`size` against the content the caller actually intended to write. The real broker's read-back compares `digest === file.sha256 && bytes.length === file.size` (`vendor/.../src/state.ts:455`); the client's reconcile path does not. An `op_id` reused for a different payload (explicitly possible per the design doc's "single-writer-per-op-id is assumed") or any broker-side content divergence would be reported as a verified successful commit.
*Why it matters:* claim #2 says reconciliation "must correctly detect its own `Broker-Op:` trailer" — the double-write is prevented by the broker's CAS, but the returned `verified` verdict is not content-truthful, which can mislead a caller into trusting state it never wrote.
*Fix:* in the `already_applied` branch, compute `sha256_hex(&request_file.content)` and `len`, and set `verified = entry.sha256 == Some(expected) && entry.size == Some(expected_len)`.

**minor — loopback stub `verify` is unfaithful to the broker (weakens the test)**
`crosslink/tests/state_broker_contract.rs:388-397`. The stub returns 404 "unknown commit" unless `commit == guard.head`, whereas the real broker's `state.ts::verify` reads *any* historical commit tree via `getTreeAtCommit(commitSha)`. Consequence: the contract suite never exercises `verify`/hydration `at` a non-head commit, so a regression in commit/ref handling for historical commits passes the stub silently. Same gap for `commit`: the stub does not re-validate `content_base64` strictness, the 256 KiB/1 MiB/32-file limits, or the "message must not begin with a broker trailer key" rule (`state.ts:478-555`) — it trusts client input (`base64_decode(...).unwrap_or_default()` at `:504`). This is acceptable for an end-to-end client test (client validates first) but means the contract test does not prove the client's wire shape is accepted by the broker's validation, only that it behaves against a lenient peer. (Claim #5.)
*Fix:* store commit→tree snapshots in `StubState`, or at minimum accept any 40-hex commit previously produced; replicate the broker's input validation.

**minor — stub does not enforce path grammar / max-paths / duplicate-path**
`crosslink/tests/state_broker_contract.rs:388-429, 481-514`. The stub's `verify`/`commit` never call `assertLogicalPath`, never cap `paths` at 32, and never reject duplicate paths. If client-side validation regressed, the stub would accept input the real broker rejects. Faithfulness gap, not a client bug.

**nit — `SecretToken::expose()` is `pub`, not `pub(crate)`**
`crosslink/src/state_broker/config.rs:79`. The module doc (config.rs:7, mod.rs:35) claims the token "exposes its value only to the HTTP client." It is reachable from any crate code. No live leak (only `config.token()`/`redact` call it), but the stated encapsulation boundary is weaker than documented.
*Fix:* `pub(crate) fn expose`.

**nit — `validate_message` is stricter than the broker (silent divergence)**
`crosslink/src/state_broker/validate.rs:108` uses `char::is_control()`, which includes C1 controls U+0080–U+009F; the broker rejects only `[\u0000-\u001F\u007F]` (`state.ts:487`). Harmless (client rejects a superset) but contradicts the "mirror of the broker rules" claim and can locally reject a message the broker would accept.

## Claims verified clean (no findings)
- **#1 Contract conformance:** request/response shapes, `ok`/`result`/`error` envelope, query-param encoding (`path`/`ref`/`commit`/`paths` joined by `,`), status→code mapping, the 8 wire codes, and limits (32/256 KiB/1 MiB/512/32, segment grammar, blob base64+`sha256`+`size` round-trip) all match `vendor/.../src/{app,state,paths,errors}.ts`. `expected_head: null` bootstrap is serialized correctly.
- **#3 Secret safety:** no path found by which the token is printed, logged (`println`/`tracing`/`dbg!`), serialized (`SecretToken` has no `Display`/`Serialize` derive, config.rs:67), stored in an error, or rendered in `Debug`. Errors/details are redacted before construction (`redact_value`); `min len 8` redaction is appropriate for 32+ char broker tokens. Token travels only in `Authorization` (client.rs:657) and host-only labels strip paths.
- **#4 `hydrate_into`:** safe — `validate_logical_path` + `projection_relative_path` reject `..`, absolute, backslash, NUL, and Windows-reserved names, so `dir.join(relative)` cannot escape the caller's directory.
- **#6 Integration point:** `sync::SyncManager::fetch_and_adopt_v3_refs` exists at `cache.rs:432` and `hub_v3::fetch_v3_refs_for_join` at `hub_v3.rs:2135`; backend-selection description matches `config.rs`. Doc is accurate.

## Not-tested / uncertainty
- Static review only: did not run the suite. The design doc's "40 passed" (`--lib state_broker`) is plausible — the filter matches module-qualified test names — but not executed.
- No live broker available: the `upstream_error` + `details.commit`/`failed_paths` read-back path is only exercised against the mock's injected failure, not the real `502` shape.
- `is_windows_reserved_name` (utils.rs:232) is invoked by projection path mapping but was outside the listed scope, so its correctness was not verified.
- 2 MiB broker request-body limit vs client 1 MiB total: reasoned consistent (base64 overhead stays under 2 MiB), not executed.

# Author disposition (self-review of delegated output)

Every finding was checked against the code before acting. Dispositions:

| # | Severity | Finding | Disposition |
|---|---|---|---|
| 1 | major | `commit_cas` `already_applied` set `verified` from path presence, not content | **Confirmed and fixed.** The reconciled branch now compares each verified entry's `sha256`/`size` against the intended payload; a reused op id or later overwrite reports `verified: false`. Regression test `commit_cas_does_not_vouch_for_content_it_did_not_write`. Commit `182e68188`. |
| 2 | minor | Stub `verify` only accepted the head commit; commit body was lenient | **Confirmed and fixed.** The stub now keeps a commit→snapshot history, serves any known commit for `/verify` and `/blob`, and re-validates the commit body with broker-equivalent rules reimplemented in the test (independent of the client's validators). New test `verify_and_blob_read_support_historical_commits`. Commit `182e68188`. |
| 3 | minor | Stub did not enforce path grammar / max paths / duplicates | **Confirmed and fixed** by the same independent validator additions (paths, ≤32 verify paths, duplicates, limits, strict base64, trailer-key message rule). Commit `182e68188`. |
| 4 | nit | `SecretToken::expose()` was `pub`, contradicting the documented boundary | **Confirmed and fixed.** Now `pub(crate)`, with the module docs aligned ("reachable only from crate-internal code"). Commit `182e68188`. |
| 5 | nit | `validate_message` rejected C1 controls the broker accepts | **Confirmed and fixed.** Now matches the broker rule exactly (`\u0000-\u001F`, `\u007F`), with a regression test asserting a C1 character is accepted. Commit `182e68188`. |

Reviewer's "not-tested" disclosures were accurate: it ran no tests and made no
live broker call. The reviewer's static verdicts on claims #1/#3/#4/#6 were
re-checked against the code and accepted; its claim #5 critique was the source
of findings 2–3.

# Additional finding from the author's own suite run (not the reviewer)

The full `cargo test --bin crosslink` suite surfaced a **new caller of the
audit-guarded destructive v2 hydration path** in the adapter's unit test
(`hydrate_to_sqlite_exempt` call-site inventory guard). Rather than weaken the
guard, the test was rewritten to use the v3 checkpoint path
(`write_checkpoint` → broker blob → projection → `read_checkpoint` →
`hydrate_from_state`), which is the entry point the v3 write path actually
uses. The guard test passes; see `bb8f3959d`.

# Pre-existing flake discovered (not caused by this change)

`commands::agents_hygiene::tests` is order/parallelism flaky: `run_sync`
installs the shared policy at `crosslink_dir.parent()/AGENTS.md`, and the tests
pass a bare tempdir, so the target is the shared `/tmp/AGENTS.md`. Reproduction
in this worktree: 2 failures in 15 consecutive module runs; the module passes in
isolation. The module's source is untouched by this branch. Filed as a
follow-up issue (see issue #802 comment) rather than fixed here, to keep this
change scoped.
