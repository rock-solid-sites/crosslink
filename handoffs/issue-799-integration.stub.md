# Recovery stub — Crosslink issue #799

Task: integrate the shared AGENTS hygiene bridge into the active Crosslink installation
Issue: local Crosslink #799
Status: complete
Model: opencode-go/muse-spark-1.3-contributor (Muse Spark 1.3, medium)
Catalog refresh: 2026-09-03T17:35Z; elevated launch required

Scope: rebuild from the current bridge branch, preserve the existing installed binary as rollback, install the new binary through the normal user-level path, and verify the installed command plus focused tests. Do not merge, rebase, push, alter unrelated stash/backup state, or begin repository synchronization.

Checkpoint: source HEAD `23801054` was verified as the bridge HEAD `4da7cc1c` plus only this assigned stub. Focused bridge tests passed (16); neighboring kickoff/session/init tests passed (219/57/108); Python session hook compilation passed. The previous installed binary was preserved at `/tmp/crosslink-rollback-20260903T175537Z-23801054` with matching pre-install SHA-256. `cargo install --path crosslink --force --locked` completed successfully. Installed `/home/claude-code/.cargo/bin/crosslink` reports `crosslink 0.9.0-beta.1+23801054-dirty` and exposes `agents-hygiene check|sync`.

Resume: Wave 3 pilot issue. Do not initialize or synchronize repositories from this stub.
