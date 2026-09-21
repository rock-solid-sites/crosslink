//! Durable-state transport seam for the deployed broker contract v1.
//!
//! # What this trait is (and is not)
//!
//! This trait is a **broker-v1-shaped CAS transport**: whole-tree
//! `expected_head`, 40-hex commit shas, `Broker-Op:` trailer reconciliation,
//! exact-commit read-back verification. It is the candidate seam for
//! Crosslink's durable-state operations, but the mapping between Crosslink's
//! v3 per-agent refs and the broker's single state tree is an **open design
//! decision** (`.design/state-broker-transport.md` §4); nothing here pre-answers
//! it. A substitute backend must reproduce the broker's CAS vocabulary or the
//! provided methods below do not apply to it.
//!
//! # Why the trait exists
//!
//! Recon of the existing persistence boundaries found no single abstraction
//! spanning durable-state *read* and *mutation*:
//!
//! - [`crate::hub_source::HubSource`] is read-only (compaction input);
//! - [`crate::hub_v3`] owns CAS ref writes but is git-plumbing specific and
//!   per-agent-ref shaped;
//! - [`crate::sync::SyncManager`] owns remote fetch/push but is the git
//!   transport itself.
//!
//! The existing local/direct git behavior is untouched; adopting the broker is
//! opt-in (see [`crate::state_broker::config::StateBackend`]).
//!
//! # Implementing backends
//!
//! - [`crate::state_broker::client::StateBrokerClient`] — the deployed broker.
//! - [`crate::state_broker::mock::MockStateTransport`] — deterministic
//!   in-memory broker with matching CAS/history semantics, for tests and dry
//!   runs.
//!
//! # Op-id semantics (the reconciliation contract)
//!
//! Every write that may need reconciliation carries a caller-chosen `op_id`;
//! the broker records it in a `Broker-Op:` trailer. On a conflict or an
//! ambiguous failure, Crosslink distinguishes four cases:
//!
//! | Case | Detection | Verdict |
//! |---|---|---|
//! | **stale conflict** | `stale_state` and the head does not record our op id | rebase only after proving non-overlap, else [`CasResolution::ReconcileRequired`] |
//! | **replay** | the head records our op id and every path matches the intended payload | [`CasResolution::AlreadyApplied`] (nothing written) |
//! | **same op id, different content** | the head records our op id but a digest differs | [`CasResolution::ReconcileRequired`] with [`ReconcileReason::OpIdReusedWithDifferentContent`] |
//! | **ambiguous write** | timeout/upstream/`verified: false` | [`CasResolution::ReconcileRequired`] with [`ReconcileReason::AmbiguousWrite`] |
//!
//! `op_id` uniqueness is a caller obligation: one op id must identify exactly
//! one intended payload for one writer. Reuse with different content is
//! *detected*, never accepted.
//!
//! # Automatic rebase is refused unless non-overlap is proven
//!
//! `commit_cas` re-issues whole-file upserts. Re-issuing them against a moved
//! head is only safe when the intervening commits provably did not touch any
//! requested path (or already wrote exactly the intended payload). The proof
//! compares per-path digests at the base and observed heads; when it cannot be
//! made, the call returns [`CasResolution::ReconcileRequired`] instead of
//! silently discarding a competing writer's bytes.
//!
//! # Local projections are disposable
//!
//! [`ProjectStateTransport::hydrate_into`] materializes durable state blobs
//! into a caller-chosen directory. That directory is a **cache**: it is never
//! the source of truth, it is not committed, and deleting it loses nothing.
//! The durable head is always re-read from the backend. Every projection
//! carries an identity/freshness marker; consumers verify it with
//! [`ProjectStateTransport::verify_projection`] before treating a projection as
//! current (see [`crate::state_broker::projection`]).

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use super::client::{
    CommitOutcome, CommitRequest, ProjectState, StateBlob, StateBrokerClient, VerifiedEntry,
    VerifiedFile,
};
use super::error::StateBrokerError;
use super::projection::{self, ProjectionMarker};

/// Outcome of materializing durable state into a disposable local directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionReport {
    /// Commit the projection was materialized from (`None` when the project has
    /// no durable state yet, in which case nothing was written).
    pub commit: Option<String>,
    /// Root directory the files were written under.
    pub root: std::path::PathBuf,
    /// Logical paths written, in hydration order.
    pub files: Vec<String>,
    /// Total bytes written.
    pub bytes: u64,
    /// Identity/freshness marker written beside the projection (`None` when
    /// there was no durable state and nothing was written).
    pub marker: Option<ProjectionMarker>,
}

/// Why automatic CAS reconciliation stopped.
///
/// Every variant means "do not treat this as an ordinary success or an
/// ordinary retry"; see [`CasResolution::ReconcileRequired`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileReason {
    /// The write may or may not have landed (timeout, lost response,
    /// unparseable response, upstream read-back mismatch, `verified: false`).
    AmbiguousWrite {
        /// What did not resolve.
        detail: String,
    },
    /// Reconciliation determined that the write is not at the current head.
    /// Callers may retry the same payload after re-reading state.
    WriteNotLanded,
    /// The head records our `op_id` but carries different content: the op id
    /// was reused for another payload, or our payload was overwritten later.
    OpIdReusedWithDifferentContent,
    /// A competing commit changed one or more of the paths this request would
    /// replace; rebasing would discard that writer's bytes.
    OverlappingPaths {
        /// The paths whose observed content changed and does not match the
        /// intended payload.
        paths: Vec<String>,
    },
    /// Non-overlap could not be proven (historical digests unreadable, or the
    /// state ref disappeared).
    OverlapUnprovable {
        /// What could not be proven, or why it could not be read.
        detail: String,
    },
    /// The durable head moved while the verdict was being computed, so no
    /// verdict can be vouched for at the head the caller will act on.
    HeadMovedDuringReconcile,
}

impl ReconcileReason {
    /// Stable machine-readable label (used in error `details.reason`).
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::AmbiguousWrite { .. } => "ambiguous_write",
            Self::WriteNotLanded => "write_not_landed",
            Self::OpIdReusedWithDifferentContent => "op_id_reused_with_different_content",
            Self::OverlappingPaths { .. } => "overlapping_paths",
            Self::OverlapUnprovable { .. } => "overlap_unprovable",
            Self::HeadMovedDuringReconcile => "head_moved_during_reconcile",
        }
    }
}

/// Outcome of a reconciled compare-and-swap commit.
///
/// Only [`Self::Applied`] and [`Self::AlreadyApplied`] are success shapes, and
/// both imply content-verified state: a transport result carrying
/// `verified: false`, a response that does not verify every submitted path, or
/// a rebase that cannot be proven safe is never representable as success — it
/// becomes [`Self::ReconcileRequired`].
///
/// **An `Ok(CasResolution)` is not by itself success.** Write-path callers must
/// either match this enum exhaustively or call
/// [`CasResolution::require_success`], which converts every non-success verdict
/// into a typed [`StateBrokerError`] (including the hard
/// `op_id_reused_with_different_content` divergence).
#[must_use = "match the CasResolution variant or call require_success(); \
              only Applied/AlreadyApplied are success"]
#[derive(Debug, Clone)]
pub enum CasResolution {
    /// Our CAS write landed and the read-back verified the payload.
    Applied {
        /// The broker's commit outcome (`verified` is always `true` here).
        outcome: CommitOutcome,
        /// Broker `commit` calls made (1 = no conflict).
        attempts: u8,
    },
    /// The durable head already recorded our `op_id` with exactly the intended
    /// payload; nothing new was written.
    AlreadyApplied {
        /// The head commit that already carries our write.
        commit: String,
        /// That commit's message (including broker trailers).
        message: String,
        /// Per-path read-back digests compared against the intended payload.
        files: Vec<VerifiedFile>,
        /// Broker `commit` calls made.
        attempts: u8,
    },
    /// The write must not be treated as an ordinary success or retried
    /// blindly: reconcile explicitly by `op_id` and re-read state.
    ReconcileRequired {
        /// Why automatic reconciliation stopped.
        reason: ReconcileReason,
        /// The head observed when reconciliation stopped, when known.
        observed_head: Option<String>,
        /// Per-path digests gathered during reconciliation, when available.
        files: Vec<VerifiedFile>,
        /// Broker `commit` calls made.
        attempts: u8,
    },
}

impl CasResolution {
    /// Broker `commit` calls made (1 = the first attempt succeeded).
    #[must_use]
    pub const fn attempts(&self) -> u8 {
        match self {
            Self::Applied { attempts, .. }
            | Self::AlreadyApplied { attempts, .. }
            | Self::ReconcileRequired { attempts, .. } => *attempts,
        }
    }

    /// Whether this resolution vouches for content-verified state.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::Applied { .. } | Self::AlreadyApplied { .. })
    }

    /// The commit of a verified resolution, when there is one.
    #[must_use]
    pub const fn commit(&self) -> Option<&str> {
        match self {
            Self::Applied { outcome, .. } => Some(outcome.commit.as_str()),
            Self::AlreadyApplied { commit, .. } => Some(commit.as_str()),
            Self::ReconcileRequired { .. } => None,
        }
    }

    /// The broker commit outcome of an [`Self::Applied`] resolution.
    #[must_use]
    pub const fn applied_outcome(&self) -> Option<&CommitOutcome> {
        match self {
            Self::Applied { outcome, .. } => Some(outcome),
            Self::AlreadyApplied { .. } | Self::ReconcileRequired { .. } => None,
        }
    }

    /// Convert this resolution into a verified success, or a typed
    /// reconcile-required error.
    ///
    /// This is the supported way for a write path to treat a CAS result as
    /// success: `Ok(CasResolution)` is not success. The error preserves the
    /// verdict distinction in `details.reason` — in particular an op-id
    /// divergence is `"op_id_reused_with_different_content"`, a hard
    /// non-success that must never be retried blindly.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::ReconcileRequired`] for every non-success verdict
    /// ([`ReconcileReason`] label in `details.reason`, observed head and any
    /// unverified paths in `details`). Never retryable.
    ///
    /// [`BrokerErrorCode::ReconcileRequired`]: super::error::BrokerErrorCode::ReconcileRequired
    pub fn require_success(self) -> Result<VerifiedCas, StateBrokerError> {
        match self {
            Self::Applied { outcome, attempts } => Ok(VerifiedCas {
                commit: outcome.commit,
                message: outcome.message,
                files: outcome.files,
                already_applied: false,
                attempts,
            }),
            Self::AlreadyApplied {
                commit,
                message,
                files,
                attempts,
            } => Ok(VerifiedCas {
                commit,
                message,
                files,
                already_applied: true,
                attempts,
            }),
            Self::ReconcileRequired {
                reason,
                observed_head,
                files,
                attempts,
            } => {
                let failed_paths: Vec<Value> = files
                    .iter()
                    .filter(|file| !file.verified)
                    .map(|file| Value::String(file.path.clone()))
                    .collect();
                Err(StateBrokerError::reconcile_required(
                    format!(
                        "compare-and-swap did not produce a verified success ({})",
                        reason.label()
                    ),
                    Some(serde_json::json!({
                        "reason": reason.label(),
                        "observed_head": observed_head,
                        "failed_paths": failed_paths,
                        "attempts": attempts,
                    })),
                ))
            }
        }
    }
}

/// A [`CasResolution`] that was converted into a verified success by
/// [`CasResolution::require_success`].
#[derive(Debug, Clone)]
pub struct VerifiedCas {
    /// The commit whose content was verified.
    pub commit: String,
    /// The commit message (including broker trailers).
    pub message: String,
    /// Per-path read-back digests compared against the intended payload.
    pub files: Vec<VerifiedFile>,
    /// `true` when the durable head already recorded our op id (nothing new
    /// was written).
    pub already_applied: bool,
    /// Broker `commit` calls made.
    pub attempts: u8,
}

/// What explicit op-id reconciliation concluded about a write.
///
/// This is the "landed / not landed / same-op-different-content" trichotomy
/// callers can use when a raw [`ProjectStateTransport::commit`] fails
/// ambiguously.
#[derive(Debug, Clone)]
pub enum OpReconciliation {
    /// The durable head records our `op_id` and every requested path carries
    /// exactly the intended content.
    Landed {
        /// Head commit that records the op.
        commit: String,
        /// That commit's message.
        message: String,
        /// Per-path digests compared against the intended payload.
        files: Vec<VerifiedFile>,
    },
    /// The durable head records our `op_id` but the content differs (reused op
    /// id, or a later overwrite).
    LandedWithDifferentContent {
        /// Head commit that records the op.
        commit: String,
        /// That commit's message.
        message: String,
        /// Per-path digests compared against the intended payload.
        files: Vec<VerifiedFile>,
    },
    /// The durable head does not record our `op_id`.
    NotLanded {
        /// The head observed (`None` when the ref does not exist).
        observed_head: Option<String>,
    },
}

/// CAS transport operations for a durable project-state backend.
///
/// All methods are synchronous: the broker client uses a blocking HTTP client,
/// matching Crosslink's CLI (and `sync`) execution model.
pub trait ProjectStateTransport {
    /// Current durable state: ref, head commit, file inventory, baseline.
    ///
    /// # Errors
    ///
    /// Transport/protocol failures, or a typed broker failure.
    fn read_state(&self) -> Result<ProjectState, StateBrokerError>;

    /// Hydrate one state file, at `at` (an exact commit sha) or the state head
    /// when `at` is `None`.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for a bad path/ref;
    /// [`BrokerErrorCode::NotFound`] when the file does not exist at that
    /// commit; plus transport/protocol failures.
    ///
    /// [`BrokerErrorCode::InvalidInput`]: super::error::BrokerErrorCode::InvalidInput
    /// [`BrokerErrorCode::NotFound`]: super::error::BrokerErrorCode::NotFound
    fn read_blob(&self, path: &str, at: Option<&str>) -> Result<StateBlob, StateBrokerError>;

    /// Read back digests for `paths` at an exact commit.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for a bad commit/paths, plus
    /// transport/protocol failures.
    ///
    /// [`BrokerErrorCode::InvalidInput`]: super::error::BrokerErrorCode::InvalidInput
    fn verify(
        &self,
        commit: &str,
        paths: &[String],
    ) -> Result<Vec<VerifiedEntry>, StateBrokerError>;

    /// Submit one compare-and-swap mutation. Never blind-retried.
    ///
    /// An `Ok` outcome whose `verified` flag is `false` must be treated as an
    /// ambiguous write ([`ReconcileReason::AmbiguousWrite`]); the provided
    /// [`Self::commit_cas`] does exactly that.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::StaleState`] when `expected_head` does not match the
    /// observed head (nothing was written); [`BrokerErrorCode::ReconcileRequired`]
    /// when the response was ambiguous; plus transport/protocol failures.
    ///
    /// [`BrokerErrorCode::StaleState`]: super::error::BrokerErrorCode::StaleState
    /// [`BrokerErrorCode::ReconcileRequired`]: super::error::BrokerErrorCode::ReconcileRequired
    fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, StateBrokerError>;

    /// The current durable head commit, or `None` when the project has no
    /// durable state yet.
    ///
    /// # Errors
    ///
    /// See [`Self::read_state`].
    fn current_head(&self) -> Result<Option<String>, StateBrokerError> {
        Ok(self.read_state()?.state.head.map(|head| head.commit))
    }

    /// Host-only label of the backend instance this transport is bound to,
    /// when the backend has one (the broker client returns its configured
    /// host; the mock returns a fixed label).
    ///
    /// Recorded in projection markers so a projection cannot silently move
    /// between backend instances that happen to share a project UUID.
    fn backend_host(&self) -> Option<String> {
        None
    }

    /// Materialize durable state files into `dir` (a disposable projection).
    ///
    /// `paths = None` hydrates the full inventory; `Some` hydrates exactly the
    /// named paths (each validated). A project with no durable state writes
    /// nothing and returns a report with `commit: None`.
    ///
    /// On success the directory carries an identity/freshness marker recording
    /// the project, the state ref, the head commit, and the digest of every
    /// projected file. The marker is written *incomplete* before the first
    /// file, so an interrupted hydration can never look complete. The write is
    /// an upsert: files listed by a previous marker but absent from this
    /// selection are removed.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] for unsafe paths,
    /// [`BrokerErrorCode::IdentityMismatch`] when `dir` already holds a
    /// projection of a different project, [`BrokerErrorCode::LocalIo`] when
    /// writing fails, [`BrokerErrorCode::Protocol`] when a blob's identity
    /// disagrees with the inventory (path/commit/blob sha/size), plus the read
    /// errors of [`Self::read_blob`].
    ///
    /// [`BrokerErrorCode::InvalidInput`]: super::error::BrokerErrorCode::InvalidInput
    /// [`BrokerErrorCode::IdentityMismatch`]: super::error::BrokerErrorCode::IdentityMismatch
    /// [`BrokerErrorCode::LocalIo`]: super::error::BrokerErrorCode::LocalIo
    /// [`BrokerErrorCode::Protocol`]: super::error::BrokerErrorCode::Protocol
    fn hydrate_into(
        &self,
        dir: &Path,
        paths: Option<&[String]>,
    ) -> Result<ProjectionReport, StateBrokerError> {
        projection::hydrate(self, dir, paths)
    }

    /// Verify that `dir` holds a complete projection of the transport's
    /// *current* durable head, returning its marker.
    ///
    /// This is the fail-closed gate for consumers: a missing, incomplete,
    /// stale, unreadable, or wrong-project projection is an error, never
    /// silently "current".
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::LocalIo`] for a missing/incomplete/stale/integrity
    /// failure, [`BrokerErrorCode::IdentityMismatch`] for another project, plus
    /// the read errors of [`Self::read_state`].
    ///
    /// [`BrokerErrorCode::LocalIo`]: super::error::BrokerErrorCode::LocalIo
    /// [`BrokerErrorCode::IdentityMismatch`]: super::error::BrokerErrorCode::IdentityMismatch
    fn verify_projection(&self, dir: &Path) -> Result<ProjectionMarker, StateBrokerError> {
        projection::verify_projection(self, dir)
    }

    /// Reconcile a possibly-ambiguous write by `op_id`: read the durable head,
    /// check whether it records our op id, and compare read-back digests
    /// against the intended payload.
    ///
    /// Returns the landed/not-landed/different-content verdict. Read failures
    /// are surfaced as errors — a caller that cannot read state cannot
    /// conclude anything.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] when `op_id` is absent, plus the
    /// errors of [`Self::read_state`] and [`Self::verify`].
    ///
    /// [`BrokerErrorCode::InvalidInput`]: super::error::BrokerErrorCode::InvalidInput
    fn reconcile(&self, request: &CommitRequest) -> Result<OpReconciliation, StateBrokerError> {
        let Some(op_id) = request.op_id.as_deref() else {
            return Err(StateBrokerError::invalid_input(
                "reconcile requires op_id: without it a write cannot be attributed",
            ));
        };
        let state = self.read_state()?;
        let Some(head) = state.state.head else {
            return Ok(OpReconciliation::NotLanded {
                observed_head: None,
            });
        };
        if !message_records_op(&head.message, op_id) {
            return Ok(OpReconciliation::NotLanded {
                observed_head: Some(head.commit),
            });
        }
        let files = verify_intended_content(self, &head.commit, request)?;
        if files.iter().all(|file| file.verified) {
            Ok(OpReconciliation::Landed {
                commit: head.commit,
                message: head.message,
                files,
            })
        } else {
            Ok(OpReconciliation::LandedWithDifferentContent {
                commit: head.commit,
                message: head.message,
                files,
            })
        }
    }

    /// Submit a mutation through the expected-head/CAS path, reconciling at
    /// most `max_retries` conflicts/ambiguous failures.
    ///
    /// Reconciliation is deliberately conservative:
    ///
    /// 1. On `stale_state` or an ambiguous write, reconcile by `op_id`: if the
    ///    head records our op id, compare digests against the intended payload
    ///    and return [`CasResolution::AlreadyApplied`] only when every file
    ///    matches; a mismatch is [`CasResolution::ReconcileRequired`].
    /// 2. Otherwise (the write is not at the head), re-issue **only after
    ///    proving non-overlap** between the base head and the observed head for
    ///    every requested path. Unprovable or overlapping rebases are refused
    ///    with [`CasResolution::ReconcileRequired`], never written.
    ///
    /// This is safe for whole-file upserts whose intervening commits provably
    /// did not touch the requested paths; append-style mutations must use
    /// [`Self::commit`] directly and reconcile explicitly. `op_id` is required.
    ///
    /// # Errors
    ///
    /// [`BrokerErrorCode::InvalidInput`] when `op_id` is absent, plus the
    /// errors of [`Self::read_state`] and [`Self::commit`] (typically the final
    /// `stale_state` when retries are exhausted and nothing landed).
    ///
    /// [`BrokerErrorCode::InvalidInput`]: super::error::BrokerErrorCode::InvalidInput
    fn commit_cas(
        &self,
        request: &CommitRequest,
        max_retries: u8,
    ) -> Result<CasResolution, StateBrokerError> {
        if request.op_id.is_none() {
            return Err(StateBrokerError::invalid_input(
                "commit_cas requires op_id so a stale_state conflict can be reconciled; \
                 use commit() for a single unretried attempt",
            ));
        }

        let mut expected_head = request.expected_head.clone();
        let mut attempt: u8 = 0;
        loop {
            let mut current = request.clone();
            current.expected_head.clone_from(&expected_head);
            match self.commit(&current) {
                Ok(outcome)
                    if outcome.verified
                        && super::client::outcome_verifies_every_requested_path(
                            &current, &outcome,
                        ) =>
                {
                    return Ok(CasResolution::Applied {
                        outcome,
                        attempts: attempt.saturating_add(1),
                    });
                }
                Ok(outcome) => {
                    // A transport that returns an unverified or incomplete
                    // outcome must not be treated as success; the write may
                    // have landed partially.
                    return Ok(CasResolution::ReconcileRequired {
                        reason: ReconcileReason::AmbiguousWrite {
                            detail: format!(
                                "transport returned an unverified or incomplete commit outcome for {}",
                                outcome.commit
                            ),
                        },
                        observed_head: outcome
                            .head_after
                            .clone()
                            .or_else(|| Some(outcome.commit.clone())),
                        files: outcome.files,
                        attempts: attempt.saturating_add(1),
                    });
                }
                Err(error) if error.is_stale_state() || error.is_reconcile_required() => {
                    let stale = error.is_stale_state();
                    let verdict = match self.reconcile(&current) {
                        Ok(verdict) => verdict,
                        Err(reconcile_error) => {
                            return Ok(CasResolution::ReconcileRequired {
                                reason: ReconcileReason::AmbiguousWrite {
                                    detail: format!(
                                        "reconciliation could not read the durable state: {}",
                                        reconcile_error.message()
                                    ),
                                },
                                observed_head: None,
                                files: Vec::new(),
                                attempts: attempt.saturating_add(1),
                            });
                        }
                    };
                    match verdict {
                        OpReconciliation::Landed {
                            commit,
                            message,
                            files,
                        } => {
                            let observed = match self.current_head() {
                                Ok(head) => head,
                                Err(recheck_error) => {
                                    return Ok(CasResolution::ReconcileRequired {
                                        reason: ReconcileReason::AmbiguousWrite {
                                            detail: format!(
                                                "the write landed at {commit} but the current head \
                                                 could not be re-read: {}",
                                                recheck_error.message()
                                            ),
                                        },
                                        observed_head: None,
                                        files,
                                        attempts: attempt.saturating_add(1),
                                    });
                                }
                            };
                            if observed.as_deref() != Some(commit.as_str()) {
                                return Ok(CasResolution::ReconcileRequired {
                                    reason: ReconcileReason::HeadMovedDuringReconcile,
                                    observed_head: observed,
                                    files,
                                    attempts: attempt.saturating_add(1),
                                });
                            }
                            return Ok(CasResolution::AlreadyApplied {
                                commit,
                                message,
                                files,
                                attempts: attempt.saturating_add(1),
                            });
                        }
                        OpReconciliation::LandedWithDifferentContent {
                            commit,
                            message: _,
                            files,
                        } => {
                            return Ok(CasResolution::ReconcileRequired {
                                reason: ReconcileReason::OpIdReusedWithDifferentContent,
                                observed_head: Some(commit),
                                files,
                                attempts: attempt.saturating_add(1),
                            });
                        }
                        OpReconciliation::NotLanded { observed_head } => {
                            if attempt >= max_retries {
                                return if stale {
                                    // stale_state guarantees the broker wrote
                                    // nothing, so the typed error is truthful.
                                    Err(error)
                                } else {
                                    Ok(CasResolution::ReconcileRequired {
                                        reason: ReconcileReason::WriteNotLanded,
                                        observed_head,
                                        files: Vec::new(),
                                        attempts: attempt.saturating_add(1),
                                    })
                                };
                            }
                            match prove_non_overlap(
                                self,
                                expected_head.as_deref(),
                                observed_head.as_deref(),
                                request,
                            ) {
                                RebaseProof::Proven => {
                                    expected_head = observed_head;
                                    attempt = attempt.saturating_add(1);
                                }
                                RebaseProof::Overlap(paths) => {
                                    return Ok(CasResolution::ReconcileRequired {
                                        reason: ReconcileReason::OverlappingPaths { paths },
                                        observed_head,
                                        files: Vec::new(),
                                        attempts: attempt.saturating_add(1),
                                    });
                                }
                                RebaseProof::Unprovable(detail) => {
                                    return Ok(CasResolution::ReconcileRequired {
                                        reason: ReconcileReason::OverlapUnprovable { detail },
                                        observed_head,
                                        files: Vec::new(),
                                        attempts: attempt.saturating_add(1),
                                    });
                                }
                            }
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// Whether `message`'s trailer block records `op_id` (`Broker-Op: <op_id>`).
///
/// The broker owns the trailer block, so a caller message can never spoof it.
#[must_use]
pub fn message_records_op(message: &str, op_id: &str) -> bool {
    let expected = format!("Broker-Op: {op_id}");
    message
        .lines()
        .any(|line| line.trim_end().trim_start() == expected)
}

/// Intended payload digests, keyed by logical path.
fn intended_digests(request: &CommitRequest) -> HashMap<&str, (String, u64)> {
    request
        .files
        .iter()
        .map(|file| {
            (
                file.path.as_str(),
                (
                    super::digest::sha256_hex(&file.content),
                    file.content.len() as u64,
                ),
            )
        })
        .collect()
}

/// Whether a read-back entry matches the intended payload for `path`.
fn matches_intended(
    entry: &VerifiedEntry,
    intended: &HashMap<&str, (String, u64)>,
    path: &str,
) -> bool {
    entry.present
        && entry.sha256.as_deref().is_some_and(|sha| {
            intended
                .get(path)
                .is_some_and(|(expected_sha, expected_size)| {
                    sha == expected_sha && entry.size == Some(*expected_size)
                })
        })
}

/// Read back digests at `commit` and mark each entry verified only when it
/// matches the intended payload.
fn verify_intended_content<T>(
    transport: &T,
    commit: &str,
    request: &CommitRequest,
) -> Result<Vec<VerifiedFile>, StateBrokerError>
where
    T: ProjectStateTransport + ?Sized,
{
    let intended = intended_digests(request);
    let entries = transport.verify(commit, &request.paths())?;
    Ok(entries
        .into_iter()
        .map(|entry| {
            let verified = matches_intended(&entry, &intended, entry.path.as_str());
            VerifiedFile {
                path: entry.path,
                blob_sha: entry.blob_sha,
                sha256: entry.sha256,
                size: entry.size,
                verified,
            }
        })
        .collect())
}

/// Result of proving that re-issuing whole-file upserts over a moved head is
/// lossless.
enum RebaseProof {
    /// No requested path was touched by the intervening commits (or the
    /// observed content already equals the intended payload).
    Proven,
    /// These requested paths changed and do not match the intended payload.
    Overlap(Vec<String>),
    /// The proof could not be made (historical digests unreadable, or the ref
    /// disappeared).
    Unprovable(String),
}

/// Prove that every path in `request` is either unchanged between `base_head`
/// and `observed_head`, or already carries exactly the intended payload.
///
/// `base_head = None` means "the base was an absent ref", so every path was
/// absent at the base.
fn prove_non_overlap<T>(
    transport: &T,
    base_head: Option<&str>,
    observed_head: Option<&str>,
    request: &CommitRequest,
) -> RebaseProof
where
    T: ProjectStateTransport + ?Sized,
{
    // A vanished ref cannot be proven non-overlapping: re-issuing would
    // bootstrap a fresh single-commit history and erase the durable past.
    let Some(observed_head) = observed_head else {
        return RebaseProof::Unprovable(
            "the state ref disappeared while reconciling; refusing to bootstrap over a deleted ref"
                .to_string(),
        );
    };
    let paths = request.paths();
    let observed = match transport.verify(observed_head, &paths) {
        Ok(entries) => index_entries(entries),
        Err(error) => {
            return RebaseProof::Unprovable(format!(
                "cannot read the observed head's digests: {}",
                error.message()
            ));
        }
    };
    let base = match base_head {
        None => None,
        Some(base_head) => match transport.verify(base_head, &paths) {
            Ok(entries) => Some(index_entries(entries)),
            Err(error) => {
                return RebaseProof::Unprovable(format!(
                    "cannot read the base head's digests: {}",
                    error.message()
                ));
            }
        },
    };

    let intended = intended_digests(request);
    let mut overlap = Vec::new();
    let mut unprovable = Vec::new();
    for path in &paths {
        let base_entry = base.as_ref().and_then(|entries| entries.get(path.as_str()));
        let observed_entry = observed.get(path.as_str());
        let base_present = base_entry.is_some_and(|entry| entry.present);
        let observed_present = observed_entry.is_some_and(|entry| entry.present);

        // Absent at both: re-creating the path cannot overwrite anything.
        if !base_present && !observed_present {
            continue;
        }
        // Present at both: "unchanged" must be proven by matching content
        // digests. Presence, `blob_sha`, or equal sizes are not content
        // evidence, so a digest-less read-back is *never* Proven: it is
        // equivalence-proven or Unprovable.
        if base_present && observed_present {
            let base_sha = base_entry.and_then(|entry| entry.sha256.as_deref());
            let observed_sha = observed_entry.and_then(|entry| entry.sha256.as_deref());
            match (base_sha, observed_sha) {
                (Some(base_sha), Some(observed_sha)) if base_sha == observed_sha => continue,
                (None, _) | (_, None) => {
                    // No digest evidence on at least one side. Equivalence with
                    // the intended payload (which requires an observed digest)
                    // is still a proof; otherwise the rebase cannot be proven.
                    if observed_entry.is_some_and(|entry| matches_intended(entry, &intended, path))
                    {
                        continue;
                    }
                    unprovable.push(path.clone());
                    continue;
                }
                // Both digests present and different: fall through to the
                // equivalence check (the observed bytes may already be ours).
                _ => {}
            }
        }
        // Equivalent: the observed content already equals our intended payload,
        // so re-issuing it cannot lose data.
        if observed_entry.is_some_and(|entry| matches_intended(entry, &intended, path)) {
            continue;
        }
        overlap.push(path.clone());
    }
    if !overlap.is_empty() {
        RebaseProof::Overlap(overlap)
    } else if !unprovable.is_empty() {
        RebaseProof::Unprovable(format!(
            "no content digest for requested path(s): {}",
            unprovable.join(", ")
        ))
    } else {
        RebaseProof::Proven
    }
}

/// Index read-back entries by logical path.
fn index_entries(entries: Vec<VerifiedEntry>) -> HashMap<String, VerifiedEntry> {
    entries
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect()
}

impl ProjectStateTransport for StateBrokerClient {
    fn read_state(&self) -> Result<ProjectState, StateBrokerError> {
        // Inherent method (HTTP call) — inherent methods take precedence here.
        self.state()
    }

    fn read_blob(&self, path: &str, at: Option<&str>) -> Result<StateBlob, StateBrokerError> {
        self.read_blob(path, at)
    }

    fn verify(
        &self,
        commit: &str,
        paths: &[String],
    ) -> Result<Vec<VerifiedEntry>, StateBrokerError> {
        Ok(self.verify(commit, paths)?.entries)
    }

    fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, StateBrokerError> {
        self.commit(request)
    }

    fn backend_host(&self) -> Option<String> {
        // Inherent method (config-derived label), as with `read_state`.
        Some(self.backend_host())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_trailer_detection_is_line_exact() {
        let message =
            "checkpoint: probe\n\nProject-UUID: 1d440dcf-bcbf-4d1a-987c-d5334568a716\nBroker: crosslink-state-broker/0.1.0\nBroker-Op: codex-1\n";
        assert!(message_records_op(message, "codex-1"));
        assert!(!message_records_op(message, "codex-2"));
        // A caller message mentioning the trailer text is not a trailer line.
        assert!(!message_records_op("note: Broker-Op: codex-1", "codex-1"));
        assert!(!message_records_op("Broker-Op: codex-10", "codex-1"));
    }

    #[test]
    fn reconcile_reason_labels_are_stable() {
        assert_eq!(
            ReconcileReason::AmbiguousWrite {
                detail: String::new()
            }
            .label(),
            "ambiguous_write"
        );
        assert_eq!(
            ReconcileReason::OverlappingPaths { paths: vec![] }.label(),
            "overlapping_paths"
        );
        assert_eq!(
            ReconcileReason::OverlapUnprovable {
                detail: String::new()
            }
            .label(),
            "overlap_unprovable"
        );
        assert_eq!(ReconcileReason::WriteNotLanded.label(), "write_not_landed");
        assert_eq!(
            ReconcileReason::OpIdReusedWithDifferentContent.label(),
            "op_id_reused_with_different_content"
        );
        assert_eq!(
            ReconcileReason::HeadMovedDuringReconcile.label(),
            "head_moved_during_reconcile"
        );
    }
}
