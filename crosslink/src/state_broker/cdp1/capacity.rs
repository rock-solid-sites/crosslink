//! CDP-1 capacity model and fail-closed checks (spec §8).
//!
//! Every check runs before any broker call. The primary accounting model is
//! [`super::COMMIT_BUDGET_ACCOUNTING`]; the `Wire` fallback rescales both the
//! per-file and per-commit budgets, so switching models is a single constant.

use super::{
    AccountingModel, COMMIT_BUDGET_BYTES, MAX_FILES_PER_COMMIT, MAX_MANIFEST_BYTES, MAX_SLOTS,
    MAX_STATE_BYTES,
};

/// Why a publish would exceed the broker-v1 budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityError {
    /// The state document is empty.
    EmptyState,
    /// The compressed payload is empty.
    EmptyPayload,
    /// The manifest exceeds [`MAX_MANIFEST_BYTES`].
    ManifestTooLarge {
        /// Actual size.
        bytes: usize,
    },
    /// Manifest plus payload exceed the commit budget.
    CommitTooLarge {
        /// Manifest size.
        manifest: usize,
        /// Payload size.
        payload: u64,
        /// Budget.
        budget: u64,
    },
    /// Payload exceeds the model's payload cap.
    PayloadTooLarge {
        /// Payload size.
        payload: u64,
        /// Cap.
        cap: u64,
    },
    /// More slots than [`MAX_SLOTS`].
    TooManySlots {
        /// Slots needed.
        slots: usize,
    },
    /// Manifest plus chunks exceed [`MAX_FILES_PER_COMMIT`].
    TooManyFiles {
        /// Files needed.
        files: usize,
    },
    /// A chunk is empty or over the slot cap.
    ChunkOutOfRange {
        /// Slot index.
        slot: usize,
        /// Chunk size.
        size: u64,
        /// Slot cap.
        cap: u64,
    },
    /// A non-last chunk is shorter than the slot cap.
    NonLastChunkShort {
        /// Slot index.
        slot: usize,
    },
    /// The state document exceeds the reader-side safety cap.
    StateTooLarge {
        /// State size.
        bytes: u64,
    },
}

impl CapacityError {
    /// Stable machine-readable label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::EmptyState => "empty_state",
            Self::EmptyPayload => "empty_payload",
            Self::ManifestTooLarge { .. } => "manifest_too_large",
            Self::CommitTooLarge { .. } => "commit_too_large",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::TooManySlots { .. } => "too_many_slots",
            Self::TooManyFiles { .. } => "too_many_files",
            Self::ChunkOutOfRange { .. } => "chunk_out_of_range",
            Self::NonLastChunkShort { .. } => "non_last_chunk_short",
            Self::StateTooLarge { .. } => "state_too_large",
        }
    }
}

impl std::fmt::Display for CapacityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {self:?}", self.label())
    }
}

impl std::error::Error for CapacityError {}

/// A checked capacity plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityPlan {
    /// Accounting model the plan was checked against.
    pub accounting: AccountingModel,
    /// Uncompressed state length.
    pub state_bytes: u64,
    /// Compressed payload length.
    pub payload_bytes: u64,
    /// Manifest length.
    pub manifest_bytes: usize,
    /// Active slot count.
    pub chunk_count: usize,
    /// Total files in the commit.
    pub files: usize,
}

impl CapacityPlan {
    /// Total decoded bytes the commit carries.
    #[must_use]
    pub const fn commit_bytes(&self) -> u64 {
        self.payload_bytes + self.manifest_bytes as u64
    }
}

/// Check a planned publish against the broker-v1 budget (spec §8.4).
///
/// # Errors
///
/// Returns the first [`CapacityError`].
pub fn check_capacity(
    state_bytes: &[u8],
    payload: &[u8],
    chunks: &[Vec<u8>],
    manifest_bytes: &[u8],
    model: AccountingModel,
) -> Result<CapacityPlan, CapacityError> {
    if state_bytes.is_empty() {
        return Err(CapacityError::EmptyState);
    }
    if state_bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(CapacityError::StateTooLarge {
            bytes: state_bytes.len() as u64,
        });
    }
    if payload.is_empty() {
        return Err(CapacityError::EmptyPayload);
    }
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(CapacityError::ManifestTooLarge {
            bytes: manifest_bytes.len(),
        });
    }
    let payload_len = payload.len() as u64;
    if payload_len > model.max_payload_bytes() {
        return Err(CapacityError::PayloadTooLarge {
            payload: payload_len,
            cap: model.max_payload_bytes(),
        });
    }
    if payload_len + manifest_bytes.len() as u64 > COMMIT_BUDGET_BYTES {
        return Err(CapacityError::CommitTooLarge {
            manifest: manifest_bytes.len(),
            payload: payload_len,
            budget: COMMIT_BUDGET_BYTES,
        });
    }
    if chunks.len() > MAX_SLOTS as usize {
        return Err(CapacityError::TooManySlots { slots: chunks.len() });
    }
    let files = 1 + chunks.len();
    if files > MAX_FILES_PER_COMMIT {
        return Err(CapacityError::TooManyFiles { files });
    }
    for (slot, chunk) in chunks.iter().enumerate() {
        let size = chunk.len() as u64;
        if size == 0 || size > model.slot_bytes() {
            return Err(CapacityError::ChunkOutOfRange {
                slot,
                size,
                cap: model.slot_bytes(),
            });
        }
        if slot + 1 != chunks.len() && size != model.slot_bytes() {
            return Err(CapacityError::NonLastChunkShort { slot });
        }
    }
    Ok(CapacityPlan {
        accounting: model,
        state_bytes: state_bytes.len() as u64,
        payload_bytes: payload_len,
        manifest_bytes: manifest_bytes.len(),
        chunk_count: chunks.len(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_broker::cdp1::payload::{gzip_encode, split_payload};

    fn plan_for(model: AccountingModel, state_len: usize) -> Result<CapacityPlan, CapacityError> {
        let state = vec![9u8; state_len];
        let payload = gzip_encode(&state).unwrap();
        let chunks = split_payload(&payload, model).unwrap();
        // A placeholder manifest of a realistic size.
        let manifest = vec![b'{'; 1_400];
        check_capacity(&state, &payload, &chunks, &manifest, model)
    }

    #[test]
    fn current_hub_shaped_state_fits_both_models() {
        // ~1.85 MB of highly compressible state.
        let plan = plan_for(AccountingModel::Decoded, 1_846_959).unwrap();
        assert_eq!(plan.chunk_count, 1);
        assert!(plan.commit_bytes() < COMMIT_BUDGET_BYTES);
        let plan = plan_for(AccountingModel::Wire, 1_846_959).unwrap();
        assert!(plan.chunk_count <= MAX_SLOTS as usize);
    }

    #[test]
    fn payload_cap_boundary_is_exact() {
        for model in [AccountingModel::Decoded, AccountingModel::Wire] {
            // Build a payload exactly at the cap, then one byte over.
            let cap = model.max_payload_bytes() as usize;
            let at_cap = vec![0u8; cap];
            let chunks = split_payload(&at_cap, model).unwrap();
            assert!(check_capacity(&[1u8], &at_cap, &chunks, &[b' '; 16], model).is_ok());
            let over = vec![0u8; cap + 1];
            // split_payload itself refuses > MAX_SLOTS; construct directly.
            let over_chunks: Vec<Vec<u8>> = over
                .chunks(model.slot_bytes() as usize)
                .map(<[u8]>::to_vec)
                .collect();
            let err = check_capacity(&[1u8], &over, &over_chunks, &[b' '; 16], model).unwrap_err();
            assert!(matches!(
                err,
                CapacityError::PayloadTooLarge { .. } | CapacityError::TooManySlots { .. }
            ));
        }
    }

    #[test]
    fn manifest_cap_boundary_is_exact() {
        let model = AccountingModel::Decoded;
        let state = vec![1u8; 100];
        let payload = gzip_encode(&state).unwrap();
        let chunks = split_payload(&payload, model).unwrap();
        let at_cap = vec![b' '; MAX_MANIFEST_BYTES];
        assert!(check_capacity(&state, &payload, &chunks, &at_cap, model).is_ok());
        let over = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert!(matches!(
            check_capacity(&state, &payload, &chunks, &over, model).unwrap_err(),
            CapacityError::ManifestTooLarge { .. }
        ));
    }

    #[test]
    fn empty_inputs_fail_closed() {
        assert_eq!(
            check_capacity(&[], &[1], &[vec![1]], &[b' '; 1], AccountingModel::Decoded),
            Err(CapacityError::EmptyState)
        );
        assert_eq!(
            check_capacity(&[1], &[], &[], &[b' '; 1], AccountingModel::Decoded),
            Err(CapacityError::EmptyPayload)
        );
    }
}
