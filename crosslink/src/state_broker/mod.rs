//! Crosslink-side adapter for the deployed **Crosslink State Broker**
//! (`rock-solid-sites/crosslink-state-broker`, contract v1).
//!
//! The broker exposes six semantic operations against one fixed private
//! repository, scoped to a single project UUID, so a worker without GitHub
//! credentials can read and mutate durable project state. This module is the
//! Crosslink-side transport for those operations:
//!
//! - [`client::StateBrokerClient`] — typed blocking HTTP client;
//! - [`transport::ProjectStateTransport`] — the narrow persistence seam
//!   (read state, read blob, verify, CAS commit, disposable hydration);
//! - [`mock::MockStateTransport`] — deterministic in-memory broker with the
//!   same CAS/typed-error semantics, for tests and dry runs;
//! - [`config::StateBackend`] — backend selection (existing local behavior is
//!   the default; the broker is opt-in).
//!
//! # What this module does NOT do
//!
//! It does not change Crosslink's existing hub v3 model, its SQLite schema, or
//! its default git transport. The hub's per-agent refs, [`crate::sync`], and
//! [`crate::hydration`] keep working exactly as before; the broker transport is
//! an additional backend that call sites can adopt incrementally. See
//! `.design/state-broker-transport.md` for the integration points and the
//! remaining assumptions.
//!
//! # Local state is disposable
//!
//! [`config::default_projection_dir`] names the default directory for a
//! hydrated projection. It is a cache: SQLite and the projection can be
//! deleted and rebuilt from the broker at any time. The durable head always
//! comes from [`transport::ProjectStateTransport::read_state`].
//!
//! # Secret safety
//!
//! The bearer token lives in [`config::SecretToken`], which redacts itself in
//! `Debug` output and implements neither `Display` nor `Serialize`. Error
//! strings are redacted before construction. The token is sent only in the
//! `Authorization` header, only to the configured broker host.

pub mod client;
pub mod config;
pub mod digest;
pub mod error;
pub mod mock;
pub mod transport;
pub mod validate;

#[cfg(test)]
mod tests;

// The bin and lib each compile this module tree; the bin does not use most of
// these re-exports (mirrors the `#[allow(dead_code)]` pattern used for other
// shared modules in the bin tree).
#[allow(unused_imports)]
pub use client::{
    BaselineObservation, CommitFile, CommitOutcome, CommitRequest, Health, ProjectInfo,
    ProjectState, RegistryObservation, StateBlob, StateBrokerClient, StateEntry, StateHead,
    StateStatus, VerifiedEntry, VerifiedFile, VerifyResponse, WhoAmI,
};
#[allow(unused_imports)]
pub use config::{
    default_projection_dir, SecretToken, StateBackend, StateBrokerConfig, BACKEND_BROKER,
    BACKEND_LOCAL, DEFAULT_TIMEOUT_MS, ENV_BACKEND, ENV_BROKER_TOKEN, ENV_BROKER_TOKEN_FILE,
    ENV_BROKER_URL, ENV_PROJECT_UUID, ENV_TIMEOUT_MS, HOOK_CONFIG_KEY,
};
#[allow(unused_imports)]
pub use error::{redact_secret, BrokerErrorCode, BrokerResult, StateBrokerError};
#[allow(unused_imports)]
pub use mock::MockStateTransport;
#[allow(unused_imports)]
pub use transport::{
    message_records_op, transport_from_env, CasResolution, ProjectStateTransport,
    ProjectionReport,
};
