//! CDP-1 payload codec and fixed-slot chunking (spec §2–§3).
//!
//! The publisher emits gzip with a deterministic header (`mtime=0`,
//! `operating_system=255`, level 9, `xfl=2`), so a given input plus a given
//! codec build yields identical bytes. The reader only decompresses; it never
//! reproduces the compression.

use std::io::{Read as _, Write as _};

use flate2::{Compression, GzBuilder};

use super::{AccountingModel, MAX_STATE_BYTES};
use crate::state_broker::StateBrokerError;

/// Gzip `state_bytes` with the frozen deterministic header.
///
/// # Errors
///
/// Returns a local-IO error when compression fails.
pub fn gzip_encode(state_bytes: &[u8]) -> Result<Vec<u8>, StateBrokerError> {
    let mut encoder = GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(Vec::new(), Compression::new(super::COMPRESSION_LEVEL));
    encoder
        .write_all(state_bytes)
        .map_err(|e| StateBrokerError::local_io(format!("gzip compression failed: {e}")))?;
    encoder
        .finish()
        .map_err(|e| StateBrokerError::local_io(format!("gzip finalization failed: {e}")))
}

/// Decompress a CDP-1 payload with a hard output cap.
///
/// The cap is `min(declared_limit, MAX_STATE_BYTES)`; a payload that expands
/// beyond it is refused (decompression-bomb guard).
///
/// # Errors
///
/// Returns a protocol error when the stream is invalid or exceeds the cap.
pub fn gzip_decode_bounded(
    payload: &[u8],
    declared_limit: u64,
) -> Result<Vec<u8>, StateBrokerError> {
    let limit = declared_limit.min(MAX_STATE_BYTES);
    let mut decoder = flate2::read::GzDecoder::new(payload);
    let mut out = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = decoder
            .read(&mut buffer)
            .map_err(|e| StateBrokerError::protocol(format!("gzip stream is invalid: {e}")))?;
        if read == 0 {
            break;
        }
        if out.len() as u64 + read as u64 > limit {
            return Err(StateBrokerError::protocol(format!(
                "decompressed state exceeds the declared {limit}-byte limit"
            )));
        }
        out.extend_from_slice(&buffer[..read]);
    }
    Ok(out)
}

/// Split a payload into fixed slots for `model` (spec §3.3).
///
/// Every chunk is non-empty and at most `model.slot_bytes()`; only the last may
/// be short. The returned vector length is the manifest `chunk_count`.
///
/// # Errors
///
/// Returns a protocol error for an empty payload or one that needs more than
/// [`super::MAX_SLOTS`] slots.
pub fn split_payload(
    payload: &[u8],
    model: AccountingModel,
) -> Result<Vec<Vec<u8>>, StateBrokerError> {
    if payload.is_empty() {
        return Err(StateBrokerError::protocol(
            "payload is empty; the broker rejects empty files",
        ));
    }
    let slot = model.slot_bytes() as usize;
    let count = payload.len().div_ceil(slot);
    if count > super::MAX_SLOTS as usize {
        return Err(StateBrokerError::protocol(format!(
            "payload needs {count} slots but the protocol allows {}",
            super::MAX_SLOTS
        )));
    }
    Ok(payload.chunks(slot).map(<[u8]>::to_vec).collect())
}

/// Concatenate active chunks in manifest order.
#[must_use]
pub fn join_payload(chunks: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_broker::digest::sha256_hex;

    #[test]
    fn gzip_is_deterministic_and_header_is_frozen() {
        let bytes = b"crosslink checkpoint payload".repeat(100);
        let a = gzip_encode(&bytes).unwrap();
        let b = gzip_encode(&bytes).unwrap();
        assert_eq!(a, b, "same input and codec must be byte-identical");
        // Header: magic, method, no flags, mtime=0, xfl=2, os=255.
        assert_eq!(&a[..4], &[0x1f, 0x8b, 0x08, 0x00]);
        assert_eq!(&a[4..8], &[0, 0, 0, 0], "mtime must be zero");
        assert_eq!(a[8], 2, "xfl must mark best compression");
        assert_eq!(a[9], 255, "os must be unknown");
        let back = gzip_decode_bounded(&a, bytes.len() as u64).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn gzip_decode_refuses_over_cap() {
        let payload = gzip_encode(&vec![7u8; 4096]).unwrap();
        assert!(gzip_decode_bounded(&payload, 1024).is_err());
        assert!(gzip_decode_bounded(&payload, 4096).is_ok());
    }

    #[test]
    fn gzip_decode_refuses_invalid_stream() {
        assert!(gzip_decode_bounded(b"not gzip", 1024).is_err());
    }

    #[test]
    fn split_boundaries_are_exact() {
        let model = AccountingModel::Decoded;
        let slot = model.slot_bytes() as usize;
        // Empty is refused.
        assert!(split_payload(&[], model).is_err());
        // One byte and exactly one slot are one chunk.
        assert_eq!(split_payload(&[1u8], model).unwrap().len(), 1);
        assert_eq!(split_payload(&vec![1u8; slot], model).unwrap().len(), 1);
        // slot+1 is two chunks.
        let two = split_payload(&vec![1u8; slot + 1], model).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].len(), slot);
        assert_eq!(two[1].len(), 1);
        // The model maximum fits exactly in MAX_SLOTS.
        let max = vec![2u8; model.max_payload_bytes() as usize];
        let chunks = split_payload(&max, model).unwrap();
        assert_eq!(chunks.len(), super::super::MAX_SLOTS as usize);
        assert_eq!(join_payload(&chunks), max);
        // One byte over four full slots needs five slots and is refused. (The
        // commit-budget cap is enforced by `check_capacity`, not by the
        // splitter: `max_payload_bytes` is smaller than four full slots.)
        let over_slots =
            vec![2u8; model.slot_bytes() as usize * super::super::MAX_SLOTS as usize + 1];
        assert!(split_payload(&over_slots, model).is_err());
    }

    #[test]
    fn split_only_last_chunk_is_short() {
        let model = AccountingModel::Decoded;
        let slot = model.slot_bytes() as usize;
        let chunks = split_payload(&vec![3u8; slot * 2 + 5], model).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), slot);
        assert_eq!(chunks[1].len(), slot);
        assert_eq!(chunks[2].len(), 5);
        assert_eq!(
            sha256_hex(&join_payload(&chunks)),
            sha256_hex(&vec![3u8; slot * 2 + 5])
        );
    }
}
