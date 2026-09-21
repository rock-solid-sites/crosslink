# ADR-802 C-now derived publish — protocol and test specification (CDP-1)

Status: **Proposed design — no implementation; blocks the first reviewed live write**
Issue: #802 · Date: 2026-09-21 · Branch: `design/pp3g-802-derived-publish`
Base: `feature/pp3g-state-broker-adapter` @ `48b503ec6`
Inputs: `.design/state-broker-authority-adr.md` (ADR-802),
`.design/state-broker-transport.md`, `handoffs/802-live-smoke.md`,
`handoffs/review-802/99-synthesis.md`, `crosslink/src/state_broker/*`,
`crosslink/src/checkpoint.rs`, `crosslink/src/hub_v3.rs` (`compact_v3`,
`commit_upserts_to_ref`, `CHECKPOINT_REF`), `crosslink/src/sync/cache.rs`,
`fix/pp3g-802-decision-independent-hardening` @ `6a92d6942` (read-only).

This document specifies the **smallest deterministic protocol for publishing one
derived Crosslink checkpoint into broker v1**. It is a design + test-spec only:
it changes no code, performs no broker write, and wires no `SyncManager`.

---

## 0. Decision summary

1. **The published document is the v3 checkpoint blob itself.** The exact bytes
   chunked are the bytes of `state.json` in the **pushed** git checkpoint commit
   (`refs/heads/crosslink/checkpoint`). The publisher copies the blob; it never
   re-serializes and never re-reduces. This makes the derived publish a pure
   function of one git commit and structurally removes ADR-802 §11.7 (reducing
   from refs alone after a prune).
2. **One logical publish = one broker commit**: `checkpoint/manifest.json` plus
   the active fixed-slot chunks `checkpoint/chunks/0000…`, all in a single
   whole-tree CAS. Broker v1 has no delete; slots are overwritten, never removed.
3. **Fixed-slot chunking** with `SLOT_BYTES = 262144` (256 KiB, the broker
   per-file cap) and a derived `MAX_SLOTS = 4` under the 1 MiB commit cap.
   Shrinking a publish simply lists fewer active chunks; unlisted slots are
   inert and must never be read.
4. **Compression is mandatory** for the current hub (1,846,959 B raw > 1 MiB
   commit cap; gzip-9 ≈ 457 KiB > 256 KiB per-file cap). `compression: "gzip"`,
   deterministic header, recorded implementation id.
5. **Watermark-monotonic CAS**: on `stale_state` the publisher re-reads the head
   and compares `OrderingKey` watermarks; equal watermark + identical semantic
   identity ⇒ already current (no write); equal watermark + different
   `state_sha256` ⇒ hard divergence; lower candidate ⇒ refuse; higher candidate
   ⇒ supersede (publisher-owned paths only).
6. **Ownership is a convention with evidence**: `checkpoint/**` is owned solely
   by the derived publisher (ADR-802 §5). The manifest records `publisher_id`
   and `op_id`; the publisher refuses to supersede a valid head manifest of the
   same watermark with different content, ever.
7. **Read-back is part of the protocol**: `verified: true` from the broker is
   necessary but not sufficient. The publisher re-reads the manifest and every
   chunk at the landed commit and reconstructs `state_sha256` before declaring
   `Landed`.
8. **Readers pin to one commit** (the head observed at the start), read only the
   manifest's active chunks at that commit, verify every digest/size, and only
   then decompress and parse. A missing path is never deletion; an unlisted
   slot is never current data.
9. **Projections are fail-closed and self-identifying**: a `v2` projection
   marker derives `op_id`, `source_checkpoint_commit`, `state_sha256`,
   `watermark`, `manifest_sha256`, and the projected `checkpoint/state.json`
   digest from the manifest; freshness requires marker head == current broker
   head **and** re-read manifest identity.
10. **This protocol is not a durability or prune gate.** It is a derived read
    tier. Nothing in it may be consulted for prune safety, lock winner, or
    display-id freeze (ADR-802 §3, §9).

---

## 1. Source-of-truth precondition

### 1.1 Exactly which pushed Git ref(s) must exist

The only authoritative source is the **v3 checkpoint ref**, and only at a commit
proven present on the git remote:

| # | Requirement | Proof |
|---|---|---|
| P1 | `refs/heads/crosslink/checkpoint` exists on the remote | a successful `git fetch <remote> +refs/heads/crosslink/checkpoint:refs/crosslink-remote/checkpoint` in this invocation, or a same-process `compact_v3` result with `checkpoint_pushed == true` and `checkpoint_commit == S` |
| P2 | `S := rev-parse refs/crosslink-remote/checkpoint` (or the same-process pushed commit) | non-null 40-hex sha |
| P3 | the tree of `S` contains `state.json` | `git cat-file -e S:state.json`; `state_blob_sha := rev-parse S:state.json` |
| P4 | the blob parses as `crate::checkpoint::CheckpointState` | `CheckpointState::from_slice(bytes)` |
| P5 | the state carries a watermark | `state.watermark == Some(W)`; `W` is an `OrderingKey` |

**A local-only checkpoint is not publishable.** If `refs/heads/crosslink/checkpoint`
is ahead of the remote tracking ref (a local `refresh_local_checkpoint` or an
unpushed `compact_v3`), the publisher either (a) publishes the remote tip `S`,
or (b) refuses. It must never read the state blob from the local ref unless that
exact commit is proven pushed. Publishing from unpushed local state would make
the broker the sole durable record of a journal mutation — the exact ADR-802 §3
prohibition.

No agent ref (`refs/heads/crosslink/agents/*`) and no `meta` ref is required:
the checkpoint blob is self-contained. Ref-tip snapshots are deliberately not
part of the manifest (§4.4) so the manifest stays a pure function of `S`.

### 1.2 Watermark and coverage

- The manifest's coverage claim is exactly `W = state.watermark`: "every event
  with `OrderingKey <= W` is represented by this state or was pruned after being
  covered by it". Nothing beyond `W` is claimed.
- `W` must be `Some`. A checkpoint with `watermark: None` is a full-reset
  genesis (`compaction::reduce` resets state when the watermark is `None`) and
  **must be refused**, even though `bootstrap_v3_hub` and
  `migrate_hub-v3` always write `Some(genesis_sentinel_watermark())`.
- Lag is allowed and documented (ADR-802 lifecycle step 1: "manifest watermark
  unchanged until publish; readers must accept documented lag"). A broker
  watermark older than the git checkpoint tip is not an error for derived reads;
  it is an error only when a consumer requires `min_watermark` (e.g. a hydration
  that must not regress SQLite).
- Monotonicity: a candidate publish is refused when `candidate.W < head.W`
  (ADR-802 §5 rule 4). Equal `W` is allowed only with identical semantic
  identity (§5.4).
- **Prune interaction is one-way**: prune safety is defined solely by the pushed
  git checkpoint (ADR-802 §9). This protocol must never be required for, nor
  consulted by, the prune gate, and it must not run inside `compact_v3`.

### 1.3 Proof that the publish derives only from durable journal state

The derivation proof is byte-level and reproducible by any third party:

1. `state_bytes := git cat-file blob S:state.json` — a git object, not a local
   file, not SQLite, not a fresh reduction.
2. `sha256(state_bytes)` is recorded as `source.state_sha256`; `rev-parse
   S:state.json` is recorded as `source.state_blob_sha`.
3. `payload := gzip(state_bytes)`; `payload_sha256`, per-chunk sizes/digests,
   and `state_bytes` length are recorded in the manifest.
4. A journal-anchored reader (one with git access) reproduces:
   `git cat-file blob <S>:state.json | sha256sum == manifest.source.state_sha256`
   and `git hash-object` of the same bytes `== manifest.source.state_blob_sha`.

Because the manifest is a deterministic function of `S` plus pinned compressor
settings, two independent publishers that publish the same `S` produce the same
semantic identity (`source.commit`, `state_sha256`, `W`) even if their compressed
bytes differ (§2.4). That semantic identity — not manifest bytes — is what
CAS reconciliation compares.

### 1.4 Forbidden sources

The publisher must fail closed if any of the following would be needed:

- a local worktree file (`<cache>/checkpoint/state.json`), the `.hub-cache`
  worktree, or a hydrated projection;
- SQLite or any materialized/hydrated state;
- a fresh `compaction::reduce` from refs (ADR-802 §11.7);
- the browse tree (`issues/*.json`, `meta/milestones.json`, `README.md`) — it is
  explicitly not mirrored under C (ADR-802 §9; it also cannot fit 32 files);
- any unpushed local checkpoint commit.

---

## 2. Canonical serialization

### 2.1 Exact bytes that are chunked

```
canonical_state_bytes := git cat-file blob <S>:state.json     // byte-for-byte
payload_bytes         := gzip(canonical_state_bytes)          // deterministic
```

`canonical_state_bytes` is **not** re-serialized, re-pretty-printed, normalized,
or re-ordered. It is the blob exactly as `compact_v3` committed it
(`serde_json::to_vec_pretty(&state)`, one trailing newline absent, LF-only).
`state_bytes = len(canonical_state_bytes)` and `payload_bytes = len(payload)`.

This matters because the existing writer is already deterministic for a given
event set (`BTreeMap`/`BTreeSet`/struct field order, no `HashMap`, no floats,
no wall-clock fields inside the serialized state) — but the protocol does not
rely on that: it relies on git's content addressing of the blob it copies.

### 2.2 Deterministic ordering

- Chunk order is the byte order of `payload`: slot `i` is
  `payload[i*SLOT_BYTES .. min((i+1)*SLOT_BYTES, len(payload))]`.
- The manifest's `chunks` array is ordered by ascending `slot`; `chunk_count ==
  chunks.len()`; slots are exactly `0..chunk_count` (no gaps, no duplicates).
- The manifest itself is serialized as compact JSON (`serde_json::to_vec`, not
  `_pretty`) over a fixed field-order struct; any map is a `BTreeMap`; no
  floats; no timestamps added by the publisher; no trailing newline. The
  manifest therefore has exactly one canonical byte representation per field
  value set.

### 2.3 Compression

| Field | Value |
|---|---|
| `encoding.compression` | `"gzip"` |
| `encoding.compression_level` | `9` |
| `encoding.compression_impl` | crate/version string, e.g. `"flate2-1.1.2/miniz_oxide-0.8.9"` |
| `encoding.gzip_header` | `"mtime=0,xfl=2,os=255"` |

Requirements:

- The gzip header must not carry wall-clock time or host identity: `MTIME = 0`,
  `OS = 255` (unknown), `XFL` determined by level. `flate2`'s `GzBuilder`
  defaults already set `mtime = 0` and `operating_system = 255`; the
  implementation must set them explicitly rather than rely on defaults.
- `compression_impl` is provenance, not a reader requirement: readers only
  decompress. Publishers that cannot reproduce a byte-identical payload (a
  different compressor build) must allocate a **new** `op_id`; they must not
  assume a byte-identical replay.
- gzip is mandatory for v1 publishers because the measured hub exceeds both the
  per-file and the per-commit caps uncompressed (§8). A hypothetical future
  `"none"` value is reserved but must not be emitted by v1 publishers; readers
  reject unknown compression ids.
- Adding a compression dependency (e.g. `flate2` with the pure-Rust
  `miniz_oxide` backend) is a prerequisite (§12).

### 2.4 Version fields and reproducibility

| Field | Purpose |
|---|---|
| `schema` | manifest schema id, `"crosslink-checkpoint-manifest/v1"` |
| `encoding.state_format` | state document family, `"crosslink-checkpoint-state/json"` |
| `chunk_slot_bytes` | the fixed slot payload cap used by this publish |

Reproducibility contract:

- **Same `S`, same compressor implementation/settings ⇒ byte-identical
  manifest and chunks.** This is what makes a replay a no-op rather than a new
  commit.
- **Same `S`, different compressor implementation ⇒ different bytes, same
  semantic identity** (`source.commit`, `source.state_sha256`, `W`,
  `state_bytes`). Reconciliation compares semantic identity; a differing
  payload with equal `W` and equal `state_sha256` is `AlreadyCurrent`, not
  divergence.
- **Different `S` ⇒ different `source.commit` and (normally) different `W`.**
  Watermark monotonicity governs.

---

## 3. Fixed-slot chunk scheme

### 3.1 Constants

```
SLOT_BYTES            = 262_144        // 256 KiB = broker maxFileBytes
SLOT_PATH_FMT         = "checkpoint/chunks/{slot:04}"
MANIFEST_PATH         = "checkpoint/manifest.json"
MAX_SLOTS             = 4              // derived, see §8
MAX_MANIFEST_BYTES    = 4_096          // fail-closed cap
COMMIT_BUDGET_BYTES   = 1_048_576      // broker maxTotalBytes (decoded bytes)
MAX_STATE_BYTES       = 16_777_216     // reader-side decompression cap (§6)
```

### 3.2 Exact path convention

| Purpose | Broker logical path | Notes |
|---|---|---|
| manifest | `checkpoint/manifest.json` | always exactly one |
| chunk slot 0 | `checkpoint/chunks/0000` | |
| chunk slot 1 | `checkpoint/chunks/0001` | |
| … | `checkpoint/chunks/{slot:04}` | `slot ∈ 0..MAX_SLOTS` |

All paths satisfy the broker grammar (`validate_logical_path`): ≤16 segments,
≤64 chars/segment, `[A-Za-z0-9._-]` with a non-`.`/`..` segment, ≤256 chars
total. `checkpoint/chunks/{slot:04}` is the concrete encoding of the ADR-802 §5
convention `checkpoint/chunks/<i>`; nothing else is written under `checkpoint/`
by CDP-1. In particular no slot directory listing is ever enumerated by a
reader, and no delete is ever issued (broker v1 has none).

### 3.3 Split rule

```
n = max(1, ceil(len(payload) / SLOT_BYTES))          // fail closed if n > MAX_SLOTS
for i in 0..n:
    chunks[i] = payload[i*SLOT_BYTES .. min((i+1)*SLOT_BYTES, len(payload))]
```

- Every chunk is non-empty (`1..=SLOT_BYTES`). An empty payload is a protocol
  error (the broker rejects empty files anyway).
- Only the last chunk may be shorter than `SLOT_BYTES`.
- A payload of exactly `SLOT_BYTES` is one chunk; `SLOT_BYTES + 1` is two.

### 3.4 Shrinking (fewer slots than the previous publish)

- A publish writes **only** the slots it needs, plus the manifest, in one
  commit. A later publish that needs `n' < n` writes slots `0..n'` and a
  manifest with `chunk_count = n'`. Slots `n'..n` from the previous commit
  remain in the tree, unreferenced.
- The active set is defined solely by the manifest's ordered `chunks` array.
  Readers must not list `checkpoint/chunks/`, must not infer a chunk from a
  slot number, and must not treat an unlisted-but-present slot as data.
- The publisher never writes slot indices beyond `n-1` in a given publish, so
  there is no window in which an old slot could be part of the active set of a
  newer commit: the commit is atomic and the reader pins to one commit.

### 3.5 No reliance on broker delete

Every operation is an upsert. Consequences accepted by design:

- Storage grows only up to `MAX_SLOTS + 1` files per project (5 files), and
  slots are reused by index, so growth is bounded by the slot namespace, not by
  publish count.
- "Deleted" derived data is represented by *not being listed* (manifest
  `chunk_count`), never by path absence. Missing path ≠ deletion
  (ADR-802 §9).

---

## 4. Manifest

### 4.1 Schema (exact proposed fields)

```rust
/// Serialized compact (`serde_json::to_vec`) with this exact field order.
struct CheckpointManifestV1 {
    schema: String,                 // "crosslink-checkpoint-manifest/v1"
    project_uuid: String,           // canonical lowercase RFC 4122 uuid
    state_ref: String,              // "refs/heads/projects/<uuid>/state"
    publisher_id: String,           // writer identity, agent-id charset, 3..=64
    op_id: String,                  // broker op id, [A-Za-z0-9._:-]{1,128}
    source: SourceCheckpoint,       // provenance of the exact git blob
    encoding: Encoding,             // how the payload is encoded
    payload_bytes: u64,             // sum of chunk sizes (compressed length)
    payload_sha256: String,         // 64-hex sha256 of concatenated chunks
    chunk_slot_bytes: u64,          // 262144
    chunk_count: u32,               // 1..=4
    chunks: Vec<ChunkEntry>,        // ordered by ascending slot, 0..chunk_count
}

struct SourceCheckpoint {
    ref_: String,                   // serde rename "ref" = "refs/heads/crosslink/checkpoint"
    commit: String,                 // 40-hex pushed git checkpoint commit S
    state_path: String,             // git tree path inside S: "state.json"
    state_blob_sha: String,         // 40-hex git blob sha of S:state.json
    state_sha256: String,           // 64-hex sha256 of the uncompressed bytes
    state_bytes: u64,               // uncompressed length
    watermark: OrderingKey,         // {timestamp, agent_id, agent_seq}, must match state.watermark
}

struct Encoding {
    state_format: String,           // "crosslink-checkpoint-state/json"
    compression: String,            // "gzip"
    compression_level: u32,         // 9
    compression_impl: String,       // provenance, not a reader requirement
    gzip_header: String,            // "mtime=0,xfl=2,os=255"
}

struct ChunkEntry {
    slot: u32,                      // 0-based, ascending, contiguous
    path: String,                   // must equal format!("checkpoint/chunks/{slot:04}")
    size: u64,                      // 1..=chunk_slot_bytes
    sha256: String,                 // 64-hex sha256 of this chunk's bytes
}
```

### 4.2 JSON example (current hub, illustrative digests elided)

```json
{
  "schema": "crosslink-checkpoint-manifest/v1",
  "project_uuid": "1d440dcf-bcbf-4d1a-987c-d5334568a716",
  "state_ref": "refs/heads/projects/1d440dcf-bcbf-4d1a-987c-d5334568a716/state",
  "publisher_id": "codex-cloud-codex-build",
  "op_id": "ckpt-68e72560f562-3f9a1c7d2b4e",
  "source": {
    "ref": "refs/heads/crosslink/checkpoint",
    "commit": "68e72560f5628f5953919f3ae7f60a8c9482e61a",
    "state_path": "state.json",
    "state_blob_sha": "76f477adf79f5ce0a612d63a47539c1fcc81181f",
    "state_sha256": "<64-hex>",
    "state_bytes": 1846959,
    "watermark": {
      "timestamp": "2026-09-21T04:40:12.611359099Z",
      "agent_id": "driver",
      "agent_seq": 76
    }
  },
  "encoding": {
    "state_format": "crosslink-checkpoint-state/json",
    "compression": "gzip",
    "compression_level": 9,
    "compression_impl": "flate2-1.1.2/miniz_oxide-0.8.9",
    "gzip_header": "mtime=0,xfl=2,os=255"
  },
  "payload_bytes": 457246,
  "payload_sha256": "<64-hex>",
  "chunk_slot_bytes": 262144,
  "chunk_count": 2,
  "chunks": [
    { "slot": 0, "path": "checkpoint/chunks/0000", "size": 262144, "sha256": "<64-hex>" },
    { "slot": 1, "path": "checkpoint/chunks/0001", "size": 195102, "sha256": "<64-hex>" }
  ]
}
```

### 4.3 Validation rules (reader and publisher)

| # | Rule | Failure class |
|---|---|---|
| M1 | `schema == "crosslink-checkpoint-manifest/v1"` | wrong format |
| M2 | `project_uuid` canonical lowercase UUID and equals the transport's configured UUID | wrong project |
| M3 | `state_ref` equals `read_state().state.state_ref` | wrong project |
| M4 | `source.ref == "refs/heads/crosslink/checkpoint"` | wrong checkpoint |
| M5 | `source.commit` is 40-hex lowercase; equals the expected/pinned commit when the caller supplied one | wrong checkpoint |
| M6 | `source.state_path == "state.json"` | wrong checkpoint |
| M7 | `source.state_blob_sha` 40-hex, `source.state_sha256`/`payload_sha256` 64-hex lowercase | corruption |
| M8 | `source.state_bytes == decompressed length`, `payload_bytes == sum(chunk.size)` | truncation |
| M9 | `chunk_count == chunks.len()`, `1 <= chunk_count <= MAX_SLOTS`, slots exactly `0..chunk_count` ascending | wrong ordering |
| M10 | every `chunk.path` equals the derived `checkpoint/chunks/{slot:04}` | wrong ordering/injection |
| M11 | `1 <= chunk.size <= chunk_slot_bytes`; only the last chunk may be `< chunk_slot_bytes`; `chunk_slot_bytes == 262144` | corruption |
| M12 | manifest bytes ≤ `MAX_MANIFEST_BYTES` | oversized |
| M13 | `source.watermark` parses and, by **value**, equals the decoded state's `watermark` | wrong checkpoint |
| M14 | `source.state_bytes <= MAX_STATE_BYTES` (reader safety cap) | oversized |
| M15 | `encoding.compression == "gzip"` (v1) and `state_format == "crosslink-checkpoint-state/json"` | unsupported encoding |
| M16 | `op_id` matches the broker op-id grammar and (journal-anchored) equals the landed commit trailer `Broker-Op:` | wrong operation |

### 4.4 Required reader distinctions and the fields that carry them

| The reader must distinguish | Fields used |
|---|---|
| a complete current publish | manifest present and valid + full digest ladder + `head_commit` pin (§6.2) |
| old unused slots | slots absent from the ordered `chunks` list (`chunk_count` bounds the active set) |
| corruption | `chunks[].sha256`, `payload_sha256`, `source.state_sha256` |
| truncation | `payload_bytes` vs sum of chunk sizes; `source.state_bytes` vs decompressed length; missing active chunk |
| wrong project / wrong checkpoint | `project_uuid`, `state_ref`, `source.ref`, `source.commit`, `source.state_path` |
| stale publish | `source.watermark` compared (by value) against `expect.min_watermark` and the head manifest's watermark |
| operation identity / replay | `op_id` (+ broker `Broker-Op:` trailer), `source.commit`, `source.state_sha256` |
| provenance (ADR-802) | `source.commit`, `source.state_blob_sha`, `source.state_sha256`, `source.watermark`, `encoding.*` |

### 4.5 What the manifest deliberately does **not** contain

- Wall-clock publish time (would break byte-determinism). Time belongs in the
  commit message and the local attempt record, both non-authoritative.
- Agent-ref tip snapshots (would make the manifest a function of publish-time
  fetch state rather than of `S`; ADR-802 does not require them).
- A self-digest (the manifest cannot contain its own digest). Its digest is
  anchored by the local attempt record, the projection marker, and the broker's
  `verify`/read-back.
- The browse tree, locks, display-id counters as separate files — the state
  document already carries locks and display IDs; the browse tree is out of
  scope for C.

---

## 5. CAS / publish algorithm

### 5.1 Preflight (no writes; fail closed before any commit)

1. Config: backend is `broker`; `project_uuid`, URL, token present; `whoami()`
   succeeds and reports the same UUID and a scope including `state:write`.
2. Source: §1.1 P1–P5. Fetch the checkpoint ref; `S` = remote tip; read the blob
   from the git object store; parse; require `watermark: Some(W)`.
3. Canonicalize: `state_bytes` → `payload` (§2), compute all digests, split into
   slots (§3.3), build the canonical manifest bytes.
4. Capacity: §8 checks; any violation → `FailedClosed`, **zero** broker calls.
5. Write-ahead attempt record (Appendix C) with a fresh `op_id` **before** the
   first write attempt. `op_id` format: `ckpt-<S[0..12]>-<16 random/unique
   hex>`; it is unique per logical publish and reused only for retries of that
   publish.
6. Initial read: `read_state()` → `head`; classify (§5.3). No head or a
   supersedable head ⇒ proceed; already-current ⇒ `AlreadyCurrent`; otherwise
   refuse.

### 5.2 Ownership proof

- Every path in the request is in the owned set `{checkpoint/manifest.json} ∪
  {checkpoint/chunks/0000..0003}` (ADR-802 §5). The publisher asserts this
  before building the request; the broker cannot enforce it (project-scoped
  token), so this protocol is the enforcement.
- The head manifest's `publisher_id` identifies the prior publisher. A
  different `publisher_id` may be superseded only under §5.3's watermark rule
  and is logged as a takeover. Same `publisher_id` is the normal path.
- No other Crosslink component may write `checkpoint/**` under C-now; in
  particular the inbox (`inbox/<writer>/**`) and any future `heads/**`/`meta/**`
  namespaces are disjoint.

### 5.3 Head classification (watermark comparison)

Given candidate `(W, S, state_sha256)` and the observed head `H`:

| Head state | Verdict |
|---|---|
| `H = None` | `Bootstrap` — allowed (`expected_head = null`) |
| `checkpoint/manifest.json` absent and no `checkpoint/**` path in the inventory | `Supersede(prior = None)` — first CDP-1 publish into a tree with unrelated files |
| `checkpoint/manifest.json` absent but `checkpoint/**` paths exist | **refuse** (`OverlapUnprovable`) — foreign/legacy chunk namespace |
| manifest present but unparsable/invalid (§4.3) | **refuse** (`HeadManifestUnreadable`) |
| manifest `project_uuid`/`state_ref` mismatch | **refuse** (`WrongProject`) |
| `head.W < W` | `Supersede(prior = manifest)` |
| `head.W == W` and `source.commit == S` and `source.state_sha256 == state_sha256` | `AlreadyCurrent(H)` — no write |
| `head.W == W` and semantic identity differs | **diverged** (`EqualWatermarkDifferentContent`) — no write, hard block |
| `head.W > W` | **refuse** (`CandidateStale`) — re-plan from a newer checkpoint |

`OrderingKey` comparison is the derived `Ord` on `(timestamp, agent_id,
agent_seq)` — the same total order `compaction::reduce` uses. Watermark equality
is by parsed value, not by string formatting (chrono normalizes subsecond
digits).

### 5.4 Single-commit requirement

The request contains exactly `1 + chunk_count` files (`manifest` + active
chunks), all upserts, in **one** `commit()` call. Never split a publish across
commits; never write a manifest before its chunks (there is no ordering inside
one commit, and a manifest referencing not-yet-written chunks would be a
truncated state if a later commit failed).

### 5.5 Read-back verification (after `verified: true`)

1. `verify(commit, [manifest] + chunk_paths)` — every entry `present` with
   `sha256`/`size` equal to the intended payload. (≤5 paths, well under the
   32-path verify cap.)
2. `read_blob(manifest_path, Some(commit))` — parse; require `op_id`,
   `source.commit`, `source.state_sha256`, `payload_sha256`, and the chunk list
   to equal the intended manifest.
3. `read_blob(chunk_i, Some(commit))` for every chunk; concatenate; require
   `payload_sha256`; decompress; require `state_sha256` and `state_bytes`.
   (Full reconstruction is mandatory for the canary publish and whenever
   `--verify-full` is set; `verify`+manifest read is the minimum otherwise.)
4. Re-read head. If it equals `commit` ⇒ `Landed`. If it moved (a concurrent
   publish superseded us) ⇒ `LandedSuperseded { commit, head }`; the landed
   commit is verified at `commit`, and the caller may re-run to reach the newer
   watermark.

### 5.6 Outcome taxonomy

| Outcome | Meaning | Next action |
|---|---|---|
| `Landed { commit, op_id }` | verified at the landed commit; head still ours | done |
| `LandedSuperseded { commit, head }` | our commit landed and verified, but a later publish is head | re-run (idempotent) |
| `AlreadyCurrent { commit }` | head already carries the same semantic identity | done, no write |
| `NotLanded { observed_head }` | provably nothing of ours at head | retry with fresh `expected_head` (bounded) |
| `ReconcileRequired { reason, observed_head }` | unknown or unprovable; writes blocked | resolve by re-read; operator if persistent |
| `Diverged { reason, observed_head }` | same op id / same watermark with different content | hard stop; manual investigation |
| `FailedClosed { reason }` | preflight/limit violation | fix input; no broker call made |

### 5.7 Lost-response and ambiguity reconciliation

`stale_state`, timeouts, connection resets, non-envelope responses, `upstream_error`
(502, including read-back disagreement), and an `Ok` outcome with
`verified: false` are all **ambiguous**. The rule is: never retry a commit whose
outcome is unknown; reconcile first by `op_id` and content.

Reconciliation procedure (`reconcile_ambiguous`):

```
read_state()  -> Err                      => UNKNOWN (block further publishes)
head == None                              => if expected_head == None: NOT_LANDED
                                             else: UNKNOWN (ref vanished; never bootstrap over it)
message_records_op(head.message, op_id):
    manifest at head parses &&
    manifest.source.commit == S &&
    manifest.source.state_sha256 == state_sha256 &&
    manifest.payload_sha256 == payload_sha256   => LANDED(head)
    otherwise                                   => DIVERGED (op id reused / overwritten)
else (trailer absent):
    if the error carried a commit sha X and X != head:
        verify(X, owned_paths) all match       => LANDED_SUPERSEDED(X, head)
        otherwise                               => NOT_LANDED(head)
    else if X == head:
        # broker named our commit as head but recorded no trailer: contract
        # violation, neither landed nor failed
                                                => UNKNOWN
    else                                        => NOT_LANDED(head)
```

Why `NOT_LANDED` is safe when the head moved: broker commits are atomic and
every successful commit carries the caller's `Broker-Op:` trailer. If the head
does not record our op id, our write is either absent or has already been
superseded by another publish. In both cases re-planning against the current
head is correct: classification (§5.3) will then either refuse (candidate
stale), return `AlreadyCurrent`, or supersede a lower watermark under the
ownership proof. The ADR's "descends from our expected head" condition is
subsumed because the broker exposes no ancestry API and does not need one: the
trailer plus atomicity is the attribution mechanism (ADR-802 §7).

Blocking rules:

- `UNKNOWN` sets the local attempt record to `blocked`; every subsequent publish
  must first call reconciliation. `UNKNOWN` is cleared only by a successful
  read that yields a definitive verdict.
- `DIVERGED` is terminal without operator intervention; the attempt record
  records the observed head and the differing digests.
- Attempt budget: at most one automatic retry after a definitive `NOT_LANDED`;
  a second `NOT_LANDED` returns `NotLanded` (safe to retry later, but no
  unbounded loop).
- Recovery after process death: on startup, if the attempt record's `phase` is
  not `resolved`, reconcile by `op_id` before any new publish (the record
  contains everything needed: `op_id`, `S`, `state_sha256`, `payload_sha256`,
  `expected_head`).

### 5.8 Reconciliation state machine

States and transitions (publisher):

```
        +--------+   preflight ok    +----------+
        |  IDLE  | ----------------> | PREPARED |   op_id persisted (write-ahead)
        +--------+                   +----------+
            ^                             |
            | preflight fail              | commit()
            | (no broker call)            v
        +-------------+            +-----------+
        | FAILED_CLOSED|           |  WRITING  |
        +-------------+            +-----------+
                                       |
             +-------------------------+--------------------------+
             | Ok(verified=true)       | Ok(verified=false)       | Err(stale/ambiguous)
             v                         v                          v
        +-------------+          +----------------------------------+
        | READ_BACK   |          |           RECONCILE              |
        +-------------+          +----------------------------------+
             |                          |         |          |
      digests match             LANDED  |   NOT_LANDED |  DIVERGED/UNKNOWN
             v                          |         |          |
        +-----------+                   v         v          v
        |  LANDED   |<------------------+   retry WRITE    BLOCKED
        +-----------+                       (once, fresh   (record; no write
             |                               expected_head)  until resolved)
             | head moved concurrently
             v
     +------------------+
     | LANDED_SUPERSEDED|
     +------------------+
```

Invariant: every attempt resolves to exactly one of `landed-verified`,
`not-landed`, or `unknown/reconcile-required` (ADR-802 §7). `AlreadyCurrent` is
a distinct success shape (no write). `Diverged` is never a success and never
retried.

---

## 6. Reader algorithm

### 6.1 Reader profiles

| Profile | Capabilities | May conclude |
|---|---|---|
| **broker-only** | broker read only | the manifest and reconstructed state are internally consistent, belong to this project, and are fresh relative to a required watermark |
| **journal-anchored** | broker + git object access | additionally that the bytes equal the blob at a pushed checkpoint commit (`state_blob_sha`, `state_sha256`) |

Only the journal-anchored profile may treat a derived read as "derived from
durable journal state". Broker-only reads are advisory (ADR-802 §3, §8) and
must never be used for exclusivity, prune, or display-id decisions.

### 6.2 Algorithm (pin to one commit)

```
read_derived_checkpoint(transport, expect):
  st   = transport.read_state()
  head = st.state.head or Err(NoDurableState)
  C    = head.commit                                    # pin once
  inv  = index(st.state.entries by path)

  m_raw = transport.read_blob(MANIFEST_PATH, Some(C))   # exact commit
  if m_raw.commit != C: Err(ProtocolMismatch)           # broker answered another commit
  if inv has MANIFEST_PATH and (inv.blob_sha != m_raw.blob_sha or inv.size != m_raw.size):
      Err(InventoryMismatch)
  m     = parse_and_validate(m_raw.bytes(), §4.3, st, expect)

  if expect.source_commit and m.source.commit != expect.source_commit: Err(WrongCheckpoint)
  if expect.min_watermark and m.source.watermark < expect.min_watermark: Err(Stale)

  payload = new Vec
  for c in m.chunks:                                     # ordered, active only
      b = transport.read_blob(c.path, Some(C))           # same pinned commit
      if b.commit != C: Err(ProtocolMismatch)
      if inv has c.path and (inv.blob_sha != b.blob_sha or inv.size != b.size):
          Err(InventoryMismatch)
      raw = b.bytes()                                    # envelope digest/size
      if len(raw) != c.size or sha256(raw) != c.sha256: Err(CorruptChunk{c.slot})
      payload += raw
  if len(payload) != m.payload_bytes or sha256(payload) != m.payload_sha256: Err(Truncated)

  state_bytes = gunzip_bounded(payload, m.source.state_bytes, MAX_STATE_BYTES)
  if len(state_bytes) != m.source.state_bytes: Err(Truncated)
  if sha256(state_bytes) != m.source.state_sha256: Err(CorruptState)

  state = CheckpointState::from_slice(state_bytes) or Err(MalformedState)
  if state.watermark != m.source.watermark (by value): Err(WrongCheckpoint)

  if journal_anchored:                                   # optional strong check
      sha = git_rev_parse("<m.source.commit>:<m.source.state_path>")
      if sha != m.source.state_blob_sha: Err(ProvenanceMismatch)
      blob = git_cat_file_blob("<m.source.commit>:<m.source.state_path>")
      if blob != state_bytes or sha256(blob) != m.source.state_sha256: Err(ProvenanceMismatch)

  return VerifiedCheckpoint{ commit: C, manifest: m, state, state_bytes }
```

Key properties:

- **One commit, pinned.** `C` is read once; every blob read uses `Some(C)`.
  A concurrent publish cannot tear the read (the hardening branch's TOCTOU
  finding is addressed by pinning; the missing pinning *test* is in §10).
- **Active chunks only.** The reader never lists the directory; unlisted slots
  are invisible by construction.
- **Digest ladder before parse.** Envelope digest → per-chunk digest →
  payload digest → state digest → parse → watermark value check. Nothing is
  parsed before the bytes are proven.
- **Pinned-commit and inventory cross-checks.** Every blob response must report
  `commit == C`, and when the head inventory lists the path its `blob_sha`/`size`
  must agree with the blob response (ADR-802 §17 item 7).
- **Bounded decompression.** Output is capped at `m.source.state_bytes` and at
  `MAX_STATE_BYTES`; a hostile zip bomb fails closed.

### 6.3 Rejection matrix (what each failure proves)

| Condition | Distinguishes |
|---|---|
| `read_state` fails / no head | no durable publish (not corruption) |
| manifest absent at `C` | not a CDP-1 publish |
| blob answered at a different commit than `C` | protocol mismatch (pin broken) |
| blob disagrees with the head inventory (`blob_sha`/`size`) | inventory↔blob mismatch |
| schema/field/grammar invalid | malformed manifest |
| `project_uuid`/`state_ref` mismatch | wrong project |
| `source.ref`/`state_path` wrong, or pinned commit mismatch | wrong checkpoint |
| chunk path/slot/count/order invalid | wrong ordering / injected manifest |
| chunk missing at `C` | truncation (missing active chunk) |
| chunk size/digest mismatch | corruption |
| `payload_bytes`/`payload_sha256` mismatch | truncation or corruption |
| decompressed size/digest mismatch | corruption / wrong encoding |
| JSON parse failure | malformed state |
| state watermark ≠ manifest watermark | manifest/state mismatch |
| `expect.min_watermark` > manifest watermark | stale publish |
| unused slots present at `C` | **not** a failure; ignored, optionally reported |
| `expect.source_commit` mismatch | stale or wrong checkpoint |

---

## 7. Old-slot semantics — why leftovers cannot be current data

Broker v1 cannot delete. After a publish with `n` chunks, a later publish with
`n' < n` leaves slots `n'..n` in the tree. The proof that they cannot be
misinterpreted:

1. **The active set is manifest-defined.** `chunk_count` and the ordered
   `chunks` array name every byte of the payload. `payload_bytes` equals the sum
   of the listed sizes; `payload_sha256` is the digest of exactly that
   concatenation.
2. **A reader never enumerates slots.** It reads `checkpoint/chunks/{slot:04}`
   only for slots in the manifest. An unlisted slot is never fetched, so it can
   never enter the payload.
3. **Even if a buggy reader fetched an unlisted slot**, including it would
   change the concatenation length and/or digest, failing `payload_bytes` /
   `payload_sha256`. A leftover slot cannot be silently absorbed.
4. **A leftover slot cannot be a valid successor payload.** Slot `k >= n'`
   starts at offset `k*SLOT_BYTES >= payload_bytes`; appending it after the
   active payload cannot match `payload_bytes` unless it is empty (it is not).
5. **There is no window.** Manifest and chunks land in one atomic commit; a
   reader pinned to that commit sees a coherent set. A reader pinned to the
   prior commit sees the prior manifest and prior chunks.
6. **A shrunk publish is not a deletion.** `n' < n` is a *supersession of the
   active set*, not a tombstone; nothing reads absence as deletion (ADR-802 §9).
7. **Bounded namespace.** Slots are fixed at `0000..0031` (practically `0000..
   0003`); a publish never creates a new slot name it did not previously
   consider, so garbage cannot grow without bound.

The one behavior that must be explicitly forbidden: a reader (or a "recovery"
tool) that reconstructs state by globbing `checkpoint/chunks/*` and sorting.
That is not CDP-1; the manifest is the only index.

---

## 8. Capacity boundary

### 8.1 Budgets

| Budget | Value | Source |
|---|---|---|
| files/commit | 32 | broker v1 (`MAX_FILES_PER_COMMIT`) |
| bytes/file | 262,144 (256 KiB) | broker v1 (`MAX_FILE_BYTES`) |
| bytes/commit | 1,048,576 (1 MiB) | broker v1 (`MAX_TOTAL_BYTES`, decoded content) |
| manifest cap | 4,096 | this protocol, fail-closed |
| slots | 4 | derived below |

### 8.2 Derivation

```
MAX_PAYLOAD_BYTES = COMMIT_BUDGET_BYTES - MAX_MANIFEST_BYTES
                  = 1_048_576 - 4_096
                  = 1_044_480 bytes
MAX_SLOTS         = ceil(MAX_PAYLOAD_BYTES / SLOT_BYTES)
                  = ceil(1_044_480 / 262_144)
                  = 4
files per publish = 1 + MAX_SLOTS = 5 <= 32        (file cap never binding)
```

Any payload `P <= 1_044_480` is representable in 4 slots: slots 0–2 hold
262,144 each and slot 3 holds `P - 786,432 <= 258,048`. A manifest with 4 chunk
entries is well under the 4,096-byte cap (compact form ≈ 1.2–1.8 KiB), so the
manifest cap and the payload cap are mutually consistent.

**Maximum publishable compressed payload: 1,044,480 bytes.**

### 8.3 Maximum uncompressed state

Compression ratio is data-dependent; the protocol limits the *compressed* size,
so the uncompressed maximum is:

```
max_state_bytes ≈ MAX_PAYLOAD_BYTES × (raw / gzip(raw))    # measured per publish
```

Measured for this repository's checkpoint (`68e72560…`, 800 issues, 1,460
comments):

| Quantity | Value |
|---|---|
| raw `state.json` | 1,846,959 B |
| gzip-9, deterministic header | 457,246 B (zlib) / 459,858 B (CLI `gzip -9`) |
| ratio | ≈ 4.04 |
| payload used | 457,246 B = **43.8 %** of capacity |
| slots used | 2 of 4 |
| files | 3 of 32 |
| headroom | ≈ 2.28× compressed; ≈ 4.22 MB / 4.02 MiB raw at this ratio |
| raw-equivalent maximum | ≈ 4,218,980 B ≈ 1,827 issues at the measured 2,308.7 B/issue density |

These are **measurements, not guarantees**: a less compressible hub hits the
compressed cap sooner. The fail-closed rule is always on the measured payload.

### 8.4 Fail-closed checks before any write

```
require state_bytes > 0
require payload_bytes > 0
require manifest_bytes <= MAX_MANIFEST_BYTES
require payload_bytes + manifest_bytes <= COMMIT_BUDGET_BYTES
require payload_bytes <= MAX_PAYLOAD_BYTES
require chunk_count <= MAX_SLOTS
require 1 + chunk_count <= MAX_FILES_PER_COMMIT
require every chunk size in 1..=SLOT_BYTES
require only the last chunk < SLOT_BYTES
require state_bytes <= MAX_STATE_BYTES
```

Any violation ⇒ `FailedClosed` with zero broker calls, no attempt record beyond
a `failed_closed` note. The protocol never truncates, never splits across
commits, and never writes a partial set.

### 8.5 Accounting assumption (must be verified, not assumed forever)

The client mirror (`validate.rs`) checks **decoded** content lengths, derived
from the broker's `state.ts`. If the deployed broker actually counts the
base64-encoded wire body, the commit budget becomes 786,432 decoded bytes and
the payload capacity 782,336 bytes (3 slots). The protocol therefore carries a
single constant `COMMIT_BUDGET_ACCOUNTING ∈ {Decoded, Wire}`; the primary value
is `Decoded`, and the loopback boundary test (§10 H6) plus the eventual live
boundary probe are the evidence that resolves it. A hub near the boundary must
not be published until that evidence exists.

---

## 9. Interaction with projections

### 9.1 What is projected

A derived-checkpoint projection contains exactly one authoritative file:

```
<projection_dir>/checkpoint/state.json      # decompressed canonical bytes
<projection_dir>/.crosslink-state-projection+v2.json
```

The directory is disposable: it is not committed, is disjoint from
`.crosslink/.hub-cache` and all authoritative directories, and may be deleted at
any time. `default_projection_dir()` (`.crosslink/state-projection/`) is the
default root. The v2 marker file name
(`PROJECTION_MARKER_FILE_V2 = ".crosslink-state-projection+v2.json"`) keeps the
`+` character that the broker path grammar rejects, so it can never collide
with a broker path; it is a distinct file from the hardening branch's v1 marker
(`.crosslink-state-projection+v1.json`), and a derived-checkpoint consumer must
require v2.

### 9.2 Marker schema (extension of the hardening branch's marker)

```rust
struct DerivedProjectionMarker {
    schema: String,                 // "crosslink-state-projection/v2"
    backend_host: Option<String>,   // from transport.backend_host()
    project_uuid: String,
    state_ref: String,
    head_commit: String,            // broker commit C the projection was pinned to
    complete: bool,                 // false until every file is written
    // ---- derived from the manifest ----
    manifest_path: String,          // "checkpoint/manifest.json"
    manifest_sha256: String,        // sha256 of the manifest bytes at C
    op_id: String,
    source_checkpoint_ref: String,
    source_checkpoint_commit: String,   // S
    source_state_blob_sha: String,
    state_sha256: String,               // == manifest.source.state_sha256
    watermark: OrderingKey,             // == manifest.source.watermark
    payload_sha256: String,
    chunk_count: u32,
    // ---- projected files ----
    files: Vec<ProjectionFile>,     // [{path:"checkpoint/state.json", sha256: state_sha256, size: state_bytes}]
    bytes: u64,
}
```

Every field above `files` is copied from the verified manifest (plus the pinned
commit); nothing is invented by the projection writer. The marker is written
incomplete before the first file and complete only after the file digest is
verified (temp + rename), matching the hardening branch's interrupted-hydration
rule.

### 9.3 Freshness check

```
verify_derived_projection(transport, dir, expect):
  1. marker exists, parses, schema == v2, complete == true
  2. identity: backend_host/project_uuid/state_ref match the transport
  3. re-read state; marker.head_commit == current head          # else stale
  4. re-read manifest at head; sha256(manifest_bytes) == marker.manifest_sha256
     and manifest.op_id == marker.op_id
     and manifest.source.state_sha256 == marker.state_sha256
     and manifest.source.watermark == marker.watermark          # else stale/tampered
  5. re-read+verify checkpoint/state.json in dir:
     size == marker.bytes, sha256 == marker.state_sha256
  6. if expect.min_watermark: marker.watermark >= expect.min_watermark
  7. (journal-anchored) git cross-check of source_checkpoint_commit/state_sha256
```

A projection is fresh for a consumer iff its pinned commit equals the head read
at consumption (step 3) and the manifest identity still matches (step 4). A
consumer that explicitly accepts a recorded older watermark may use it for
advisory reads only — never for exclusivity, prune, or display-id decisions
(ADR-802 §8).

`checkpoint/state.json` in the projection is fed to the existing
`crate::checkpoint::read_checkpoint`/`hydrate_from_state` path (the same entry
point the v3 write path uses); the projection never touches SQLite directly and
never writes a `record_hydrated_ref` git marker (namespaces stay distinct).

---

## 10. Test matrix

Levels: **U** = pure unit (no transport), **M** = mock-broker contract
(`MockStateTransport`), **H** = loopback HTTP (real `StateBrokerClient` +
`tests/state_broker_contract.rs`-style stub), **L** = one reviewed live write
(not CI).

| ID | Scenario | Level | Assertion |
|---|---|---|---|
| T01 | one chunk (payload ≤ SLOT_BYTES, incl. exactly 1 and exactly 262144) | U | 1 slot; sizes/offsets exact |
| T02 | multiple chunks (262145; 1,044,480) | U | slot count/sizes exact; only last short |
| T03 | shrink from N=4 to n=2 | U, M | manifest lists 2; slots 2–3 untouched; reassembly uses 2 only |
| T04 | shrink over the broker: publish 4, then 2, read back | M | second commit has 3 files; reader reconstructs from 2 |
| T05 | corrupt chunk (byte flip) | U, M | digest mismatch → reject; no parse attempted |
| T06 | corrupt manifest (bad JSON / bad schema / bad field) | U, M | typed rejection; no chunk reads |
| T07 | wrong ordering (permuted chunks array; slot gaps/dupes) | U | validation rejects; reassembly digest fails if forced |
| T08 | wrong digest (chunk / payload / state) | U, M | each layer rejects with a distinct class |
| T09 | missing active chunk at pinned commit | M, H | NotFound → reject (truncation) |
| T10 | stale broker head (409 between read and commit) | M, H | rebase once under §5.3; single new commit; or refuse if candidate stale |
| T11 | newer watermark already present | M | `CandidateStale` refusal, no write |
| T12 | same watermark + identical semantic identity replay | M | `AlreadyCurrent`, commit count unchanged |
| T13 | same watermark + different `state_sha256` | M | `Diverged`, no write |
| T14 | same op-id + different content at head | M, H | `Diverged` (never `Ok`/`verified:false`) |
| T15 | lost response after successful commit (commit lands, client sees transport error) | M, H | reconcile by op id → `Landed`, exactly one commit |
| T16 | lost response where commit did not land | M, H | reconcile → `NotLanded` → one bounded retry → `Landed` |
| T17 | 502 read-back ambiguity with commit sha in details, head has our content | H | `Landed` |
| T18 | 502 where head moved past our commit and carries no op id | H | `NotLanded`, no divergence |
| T19 | reconcile read failure (state read fails) | M | `ReconcileRequired`; subsequent publish blocked until resolved |
| T20 | oversized checkpoint (payload > capacity) | U, M | `FailedClosed`, zero commit calls |
| T21 | exact size boundary (payload == 1,044,480 OK; +1 fail; manifest 4096 OK/4097 fail; 4 slots OK/5 fail) | U | boundary exactness |
| T22 | project identity mismatch (manifest vs config; head manifest wrong project) | U, M | reader rejects; publisher refuses to supersede |
| T23 | source checkpoint mismatch (reader pinned to another S; publisher S not pushed) | U, M | reader `WrongCheckpoint`; publisher fails preflight with no broker call |
| T24 | stale projection (marker head ≠ current head; marker watermark < min) | U, M | fail-closed hydration |
| T25 | unused old slots present at head | U, M | reader ignores them; publish succeeds; inventory may report them informationally |
| T26 | interrupted hydration leaves incomplete marker | U | v2 marker `complete:false` refuses |
| T27 | reader pins one commit across a concurrent publish | M | all blobs read at C; no tearing (pinning regression test) |
| T28 | gzip determinism + header (mtime=0, os=255, xfl=2) | U | two compressions byte-identical; decompress round-trip |
| T29 | canonical manifest bytes (compact, fixed order, no trailing newline) | U | two builds byte-identical |
| T30 | watermark comparison by value (equal timestamps formatted differently) | U | parsed-value equality, not string equality |
| T31 | writer never deletes / never lists (mock records calls) | M | zero delete operations; reader reads only listed paths |
| T32 | attempt-record write-ahead + crash recovery | M | op id persisted before commit; restart reconciles landed |
| T33 | full reconstruction of the real checkpoint blob (fixture) | U | state_sha256 == `git cat-file blob` digest of the fixture |
| T34 | head manifest unreadable/foreign `checkpoint/**` without manifest | M | refuse (`HeadManifestUnreadable`/`OverlapUnprovable`), no write |
| T35 | HTTP limits (per-file/per-commit/path grammar/base64 body) | H | request accepted by a strict stub; oversize rejected by stub as `invalid_input` |
| T36 | broker-only vs journal-anchored reader profiles | U, H | broker-only cannot claim provenance; anchored profile verifies blob sha + digest |
| T37 | projection hydration from manifest+chunks → `checkpoint/state.json` → existing `read_checkpoint` | M, H | byte-identical to source; marker v2 fields exact |
| T38 | `--dry-run` (plan + capacity + no commit) | M | zero broker write calls; full plan reported |
| T39 | blob answered at another commit / inventory↔blob mismatch | U, M, H | `ProtocolMismatch` / `InventoryMismatch` rejection |

**L1 (the one reviewed live write).** After all gates in §12 are closed and with
explicit operator approval: publish the current pushed checkpoint of this
repository into the real project UUID; assert `Landed`, `Broker-Op` trailer
equals the manifest `op_id`, read back manifest+chunks, reconstruct
`state_sha256`, compare to `git cat-file blob S:state.json | sha256sum`;
re-run the same command → `AlreadyCurrent` with no new commit; hydrate a
projection and verify it against the source blob. One write, one project, one
commit; no `SyncManager`, no inbox, no prune. Any live boundary/size probe on a
separate synthetic project is a distinct operator-approved action (L2), not part
of L1.

### 10.1 Level rationale

- **U** covers all pure functions: splitting, manifest encode/decode/validate,
  digest ladders, capacity math, watermark ordering, projection marker
  derivation, determinism. These are fast, hermetic, and must be exhaustive.
- **M** covers CAS/reconcile semantics against the deterministic in-memory
  broker, including injected failures (stale, ambiguous, competing commit,
  ref deletion, lost response) and call-count assertions. This is where the
  state machine is proven.
- **H** covers the exact HTTP contract: envelopes, base64 bodies, path grammar,
  409 `details.observed_head`, 502 mapping, connection-loss injection. It uses
  the existing loopback stub, extended to model commit history and strict
  request validation (the hardening branch already made the stub
  history-aware).
- **L** is one reviewed, operator-approved write. It is the only test that can
  prove the deployed broker's actual limit accounting and trailer behavior. It
  is never part of CI and never run without the §12 gate.

---

## 11. Unresolved questions

1. **Limit accounting (decoded vs wire bytes).** §8.5. Must be resolved by the
   loopback boundary test plus a live probe before any near-boundary publish.
2. **Compression determinism.** Is a pinned `flate2`/`miniz_oxide` version
   guaranteed byte-stable across platforms? If not, the protocol still works
   (semantic-identity comparison), but replay is a new commit rather than a
   no-op. Confirm the chosen dependency and record `compression_impl`; consider
   a determinism test across the CI matrix.
3. **Project UUID ↔ repository binding.** ADR-802 §16/§17 item 8 residual: the
   manifest records the broker project UUID, but nothing yet binds it to this
   git repository/remote. Publishing into the wrong project remains possible
   until that binding exists. Is `state_ref` + configured UUID + operator
   invocation sufficient for the first write, or must repo↔UUID binding land
   first?
4. **Manifest provenance scope.** Agent-ref tip snapshots were deliberately
   excluded (§4.4) to keep the manifest a pure function of `S`. Confirm that
   ADR-802 does not require them; if a coverage audit does, add them as a
   *separate* provenance object excluded from semantic identity.
5. **Superseding a different `publisher_id`.** Is automatic takeover (strictly
   lower watermark, valid manifest) acceptable, or should a different publisher
   always require an explicit operator flag? (Recommended: allow with a logged
   takeover; require the flag only if the head publisher differs *and* the head
   watermark is equal-but-different, which is already `Diverged`.)
6. **`AlreadyCurrent` when the manifest bytes differ but the semantic identity
   matches** (different compressor): treat as no-op (recommended) or as a new
   publish to normalize the manifest? (Recommended: no-op.)
7. **Slot count ceiling.** `MAX_SLOTS = 4` is forced by the 1 MiB commit cap.
   A hub whose gzip exceeds 1,044,480 B simply cannot be published under C-now.
   Is fail-closed acceptable as the terminal state for such hubs (with broker
   v2 / per-head layout as the escape hatch), or is a multi-commit
   continuation rule needed? (Recommended: fail closed; multi-commit violates
   ADR-802 §10.)
8. **Reader trust for broker-only consumers.** A broker-only reader cannot
   distinguish a self-consistent forged manifest from a genuine one. ADR-802
   already limits derived reads to advisory use; confirm that no consumer with
   hydration privileges will run broker-only without the journal-anchored check
   available.
9. **Projection directory disjointness.** ADR-802 §17 item 4 residual: the
   projection root must be provably disjoint from `.crosslink/.hub-cache` and
   authoritative dirs. Is a path check plus the v2 marker sufficient, or does
   this need a configured allow-list?
10. **Attempt-record location and retention.** Proposed
    `.crosslink/state-broker/publish-attempt.json`; confirm it is acceptable for
    the record to persist (including `Diverged`/`blocked` states) and whether it
    should be gitignored.
11. **Whether to publish at all while a prune has occurred.** The blob is
    self-contained, so prune is irrelevant; confirm no operator expectation
    that a broker publish implies "recently pruned".

---

## 12. What must be true before implementation begins

**Policy / decision gates**

1. ADR-802 is accepted by the operator (it is currently "Proposed — binding for
   any SyncManager wiring until superseded"); CDP-1 is the §14 "Publish" path.
2. An active Crosslink issue exists for the implementation (this design is
   under #802; the implementation should be a scoped follow-up with its own
   worktree/issue binding).
3. This document passes an independent review by a model family different from
   the author, with material claims (limits, path grammar, watermark rules,
   outcome taxonomy) verified against the code and broker contract.

**Prerequisite code gates (ADR-802 §17)**

4. `validate_message` non-ASCII panic fixed (item 1).
5. Reconcile-required class present and complete, including timeout/502
   ambiguity (item 2).
6. Op-id-seen/content-differs is a distinct hard outcome, never
   `Ok{verified:false}` (item 3).
7. Projection marker mandatory and fail-closed, extended per §9 (item 4;
   residuals closed).
8. Blind same-path rebase replaced by the §5 proof (item 6). **Note:** the
   generic `commit_cas` refuses every whole-file rebase over changed paths, so
   CDP-1 must **not** call generic `commit_cas`; it needs a publisher-specific
   reconcile implementing §5.3 + §5.7. This is a design requirement on the
   implementation, not an optional refinement.
9. Backend identity binding per-read at minimum (item 8 partial); repo↔UUID
   binding is question 3 and must be resolved or explicitly waived for the
   first write.
10. Journal high-water-mark check at append (item 9) — required before any
    broker write is enabled per ADR-802 §17, independent of CDP-1's correctness.

**Implementation prerequisites**

11. Compression dependency selected and pinned (`flate2` + `miniz_oxide` or
    equivalent), with a determinism test (T28) and the header requirements of
    §2.3.
12. `COMMIT_BUDGET_ACCOUNTING` resolved by the loopback boundary test (T21,
    T35, H6); no near-boundary publish before it is resolved.
13. A `--dry-run`/plan mode exists that performs zero broker writes (T38) so
    the protocol can be exercised in CI and in review.
14. The local attempt record (Appendix C) is implemented with atomic writes and
    is consulted before any publish (crash recovery, T32).
15. `whoami` scope and project-UUID checks run before every publish; failures
    are `FailedClosed` with no broker call.

**Operational prerequisites for L1 (the one live write)**

16. Operator approval for the specific project UUID, the specific `S`, and the
    single write, immediately before it (no standing approval).
17. The token is provided out-of-band; never in chat or config; redaction tests
    remain green.
18. `SyncManager` is not wired (ADR-802 §16); no inbox work is included; no
    prune is triggered by the publish.
19. A read-back comparison against `git cat-file blob S:state.json` is performed
    as part of L1, and the result is recorded in the issue/handoff.

---

## Appendix A — publish pseudocode (normative)

```
fn publish_checkpoint(cfg, git, transport, opts) -> PublishOutcome:
  # ---------- preflight (zero writes) ----------
  who = transport.whoami()                       # typed error -> FailedClosed
  require who.project_uuid == cfg.project_uuid and who.scopes contains "state:write"
  st0 = transport.read_state()
  require st0.project.uuid == cfg.project_uuid and st0.state.state_ref == cfg.state_ref

  S = resolve_pushed_checkpoint(git, cfg.remote) # §1.1: fetch or same-process push proof
  require S != null
  state_bytes = git.cat_file_blob(S, "state.json")
  state_blob_sha = git.rev_parse(S + ":state.json")
  state = CheckpointState::from_slice(state_bytes) or FailedClosed
  W = state.watermark or FailedClosed                 # None is not publishable

  payload = gzip9_deterministic(state_bytes)
  manifest = build_manifest(cfg, S, state_blob_sha, state_bytes, payload, W, opts.publisher_id)
  manifest_bytes = canonical_json(manifest)
  check_capacity(state_bytes, payload, manifest_bytes) or FailedClosed   # §8.4
  chunks = split(payload, SLOT_BYTES)

  # ---------- write-ahead ----------
  op_id = "ckpt-" + S[0..12] + "-" + unique_hex(16)
  record = attempt_record(op_id, S, state_sha256(state_bytes), payload_sha256(payload), st0.head)
  atomic_write(record)                               # phase = prepared

  # ---------- classify head ----------
  decision = classify_head(transport, st0, cfg, W, S, state_sha256(state_bytes))  # §5.3
  match decision:
    AlreadyCurrent(c) -> resolve(record, already_current, c); return AlreadyCurrent(c)
    Refuse(reason)    -> resolve(record, refused, reason);    return ReconcileRequired(reason)
    Bootstrap | Supersede -> proceed

  # ---------- one CAS commit ----------
  req = CommitRequest{ expected_head: st0.head?.commit, message: commit_message(S, W, chunks),
                       op_id: op_id, files: [manifest_bytes] + chunks }
  record.phase = in_flight; atomic_write(record)
  match transport.commit(req):
    Ok(o) if o.verified -> return read_back_and_resolve(record, o, ...)
    Ok(_)               -> return reconcile(record, req, carried_commit = o.commit)
    Err(e) if e.stale_state or e.reconcile_required or e.transport or e.upstream or e.protocol:
                            return reconcile(record, req, carried_commit = e.details.commit?)
    Err(e)              -> resolve(record, failed, e); return FailedClosed(e)   # auth/invalid/method: nothing written

fn read_back_and_resolve(record, o, ...) -> PublishOutcome:
  entries = transport.verify(o.commit, req.paths())
  require every entry.present && entry.sha256/size == intended        # else reconcile
  m2 = parse(transport.read_blob(MANIFEST_PATH, Some(o.commit)).bytes())
  require m2 == intended_manifest
  if opts.verify_full or opts.canary:
      payload2 = concat(transport.read_blob(c.path, Some(o.commit)).bytes() for c in chunks)
      require sha256(payload2) == payload_sha256
      require sha256(gunzip(payload2)) == state_sha256
  head = transport.read_state().state.head?.commit
  if head == o.commit: resolve(record, landed, o.commit); return Landed(o.commit)
  else:                resolve(record, landed_superseded, o.commit); return LandedSuperseded(o.commit, head)

fn reconcile(record, req, carried_commit) -> PublishOutcome:
  st = transport.read_state() or return block(record, unknown)
  head = st.state.head
  if head == null:
      if req.expected_head == null: return NotLanded(null)
      else: return block(record, unknown)               # never bootstrap over a vanished ref
  if message_records_op(head.message, req.op_id):
      m = read_manifest_at(head.commit)
      if m valid and m.source.commit == S and m.source.state_sha256 == state_sha256
         and m.payload_sha256 == payload_sha256:
            resolve(record, landed, head.commit); return Landed(head.commit)
      else:
            resolve(record, diverged, head.commit); return Diverged(head.commit)
  if carried_commit != null and carried_commit != head.commit:
      if verify(carried_commit, owned_paths) all match:
          resolve(record, landed_superseded, carried_commit); return LandedSuperseded(carried_commit, head.commit)
  if carried_commit != null and carried_commit == head.commit:
      # The broker named our commit as the head but the trailer is absent:
      # a contract violation, not evidence of landing or failure.
      return block(record, unknown)
  return NotLanded(head.commit)                          # bounded retry, fresh expected_head
```

A `NotLanded` verdict re-enters the algorithm **at most once** with
`expected_head = observed_head` (a fresh classify + commit); a second
`NotLanded` is returned to the caller. `AlreadyCurrent`/`Diverged`/`blocked`
never re-enter. `LandedSuperseded` does not re-enter automatically; the caller
may re-run to reach the newer watermark.

## Appendix B — reader pseudocode (normative)

```
fn read_derived_checkpoint(transport, cfg, expect) -> VerifiedCheckpoint:
  st = transport.read_state()
  C  = st.state.head?.commit or Err(NoDurableState)
  m_raw = transport.read_blob(MANIFEST_PATH, Some(C)).bytes()      # pinned
  if m_raw.len() > MAX_MANIFEST_BYTES: Err(OversizedManifest)
  m = CheckpointManifestV1::from_slice(m_raw) or Err(MalformedManifest)
  validate_manifest(m, st, cfg, expect) or Err(<class from §4.3>)

  payload = Vec::with_capacity(m.payload_bytes)
  for c in m.chunks:
      raw = transport.read_blob(c.path, Some(C)).bytes()
      if raw.len() != c.size or sha256(raw) != c.sha256: Err(CorruptChunk(c.slot))
      payload.extend(raw)
  if payload.len() != m.payload_bytes or sha256(payload) != m.payload_sha256: Err(Truncated)

  state_bytes = gunzip_bounded(payload, limit = m.source.state_bytes) or Err(CorruptState)
  if state_bytes.len() != m.source.state_bytes or sha256(state_bytes) != m.source.state_sha256:
      Err(CorruptState)
  state = CheckpointState::from_slice(state_bytes) or Err(MalformedState)
  if state.watermark != m.source.watermark: Err(WrongCheckpoint)     # by value

  if expect.journal_anchored:
      if git.rev_parse(m.source.commit + ":" + m.source.state_path) != m.source.state_blob_sha:
          Err(ProvenanceMismatch)
      if git.cat_file_blob(m.source.commit + ":" + m.source.state_path) != state_bytes:
          Err(ProvenanceMismatch)

  return VerifiedCheckpoint{ commit: C, manifest: m, state, state_bytes }

fn hydrate_derived_projection(transport, dir, expect) -> ProjectionMarker:
  vc = read_derived_checkpoint(transport, cfg, expect)
  marker = DerivedProjectionMarker{ ..., complete: false, manifest_sha256: sha256(vc.manifest_bytes),
                                    op_id, source_checkpoint_commit, state_sha256, watermark, ... }
  atomic_write_marker(dir, marker)                       # incomplete first
  atomic_write(dir/"checkpoint/state.json", vc.state_bytes)
  verify file size+digest
  marker.complete = true; atomic_write_marker(dir, marker)
  return marker
```

## Appendix C — local attempt record (write-ahead)

Path: `.crosslink/state-broker/publish-attempt.json` (atomic write; gitignored).

```json
{
  "schema": "crosslink-publish-attempt/v1",
  "project_uuid": "1d440dcf-bcbf-4d1a-987c-d5334568a716",
  "publisher_id": "codex-cloud-codex-build",
  "op_id": "ckpt-68e72560f562-3f9a1c7d2b4e",
  "phase": "prepared | in_flight | resolved",
  "source": {
    "commit": "68e72560f5628f5953919f3ae7f60a8c9482e61a",
    "state_blob_sha": "76f477adf79f5ce0a612d63a47539c1fcc81181f",
    "state_sha256": "<64-hex>",
    "watermark": {"timestamp": "...", "agent_id": "driver", "agent_seq": 76}
  },
  "payload_bytes": 457246,
  "payload_sha256": "<64-hex>",
  "manifest_sha256": "<64-hex>",
  "chunk_count": 2,
  "expected_head": "93b1c6a6737231e4a3913eb7d87246b239c2a5e8",
  "resolution": {
    "outcome": "landed | landed_superseded | already_current | not_landed | diverged | unknown | failed_closed",
    "commit": "<40-hex or null>",
    "reason": "<stable label>",
    "at": "<RFC3339, local only>"
  }
}
```

The record is local and non-authoritative; it exists so that a crashed or
ambiguous publish can be reconciled by `op_id` before any new write, and so a
`blocked`/`diverged` state survives process restarts.

## Appendix D — constants summary

| Constant | Value |
|---|---|
| `MANIFEST_PATH` | `checkpoint/manifest.json` |
| `SLOT_PATH_FMT` | `checkpoint/chunks/{slot:04}` |
| `SLOT_BYTES` | `262_144` |
| `MAX_SLOTS` | `4` |
| `MAX_MANIFEST_BYTES` | `4_096` |
| `COMMIT_BUDGET_BYTES` | `1_048_576` |
| `MAX_PAYLOAD_BYTES` | `1_044_480` |
| `MAX_STATE_BYTES` | `16_777_216` |
| `MANIFEST_SCHEMA` | `crosslink-checkpoint-manifest/v1` |
| `PROJECTION_SCHEMA` | `crosslink-state-projection/v2` |
| `PROJECTION_MARKER_FILE_V2` | `.crosslink-state-projection+v2.json` |
| `SOURCE_REF` | `refs/heads/crosslink/checkpoint` |
| `SOURCE_STATE_PATH` | `state.json` |
| `COMPRESSION` | `gzip`, level 9, `mtime=0,xfl=2,os=255` |
