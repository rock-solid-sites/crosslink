//! SHA-256 hex helpers shared by the broker client, mock, and projection code.

use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// First 40 hex characters of the SHA-256 of `bytes` — a deterministic,
/// git-shaped identifier for mocks and projections that need a blob-sha
/// lookalike without a git object store.
#[must_use]
pub fn pseudo_git_sha(bytes: &[u8]) -> String {
    let mut digest = sha256_hex(bytes);
    digest.truncate(40);
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_deterministic_and_well_formed() {
        let digest = sha256_hex(b"hello");
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, sha256_hex(b"hello"));
        assert_ne!(digest, sha256_hex(b"world"));
        assert_eq!(pseudo_git_sha(b"hello").len(), 40);
    }
}
