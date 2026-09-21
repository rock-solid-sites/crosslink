# Crosslink State Broker transport adapter

Status: in progress (stub committed before implementation, per agent policy)
Task: Crosslink issue #802 (`feature/pp3g-state-broker-adapter`)
Parent context: ASES Crosslink issue #564
Baseline: `feature/pp3g-Zbk6-cleanup-rescope-349` @ `d323f020dd1a592113d71ebdb1facefb4b426a18`

## Scope

Add the smallest clean Crosslink-side adapter for the deployed
`crosslink-state-broker`: read durable project state, hydrate state blobs into
disposable local projections, submit expected-head/CAS mutations with typed
`stale_state` handling, and verify by read-back.

Out of scope / explicitly not done here:

- no redesign of Crosslink's hub v3 model (per-agent refs stay as they are);
- no historical state migration;
- no GitHub credentials in Crosslink;
- no live mutations against the production broker;
- no change to default (local/direct git) behavior.

## Recon summary (to be completed at handoff)

- Existing persistence boundaries inspected: `sync/`, `hub_v3.rs`,
  `hub_source.rs`, `hydration.rs`, `db/`, `checkpoint.rs`.
- Finding: no existing StateStore/backend trait spans read + mutation; the
  closest are `HubSource` (read-only compaction input) and the `hub_v3` ref
  plumbing (CAS writes).

(Implementation, integration point, assumptions, and test evidence follow in
the completed version of this document.)
