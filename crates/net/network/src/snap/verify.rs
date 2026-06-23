//! Verification of `snap/2` block access list responses.
//!
//! Received BALs are decoded and checked against the `block_access_list_hash` committed in the
//! block header using the canonical EIP-7928 hash of the decoded list, then applied in strict block
//! order. A required-but-missing BAL is a hard failure.

use alloy_eip7928::bal::DecodedBal;
use alloy_primitives::{Bytes, B256};

/// Error verifying a `snap/2` block access list.
#[derive(Debug, thiserror::Error)]
pub enum BalVerifyError {
    /// A BAL required to make progress was not provided by the peer.
    #[error("missing block access list for block {0}")]
    Missing(B256),
    /// The payload could not be decoded as a block access list.
    #[error("malformed block access list for block {block}: {source}")]
    Malformed {
        /// Block the payload belongs to.
        block: B256,
        /// Underlying RLP decode error.
        source: alloy_rlp::Error,
    },
    /// The decoded BAL does not hash to the header's `block_access_list_hash`.
    #[error("block access list hash mismatch for block {block}: got {got}, expected {expected}")]
    HashMismatch {
        /// Block the BAL belongs to.
        block: B256,
        /// Canonical hash computed from the decoded payload.
        got: B256,
        /// Hash committed in the block header.
        expected: B256,
    },
}

/// Decodes and verifies a single BAL payload against its header's `block_access_list_hash`.
///
/// The payload is decoded with [`DecodedBal::from_rlp_bytes`] and compared using the canonical
/// EIP-7928 hash of the decoded list (not `keccak256` of the raw bytes), matching execution and
/// payload validation. The decoded BAL is returned so callers persist the canonical form.
pub fn verify_block_access_list(
    block: B256,
    raw: Bytes,
    expected_hash: B256,
) -> Result<DecodedBal, BalVerifyError> {
    let decoded = DecodedBal::from_rlp_bytes(raw)
        .map_err(|source| BalVerifyError::Malformed { block, source })?;
    let got = decoded.as_bal().compute_hash();
    if got == expected_hash {
        Ok(decoded)
    } else {
        Err(BalVerifyError::HashMismatch { block, got, expected: expected_hash })
    }
}

/// Decodes and verifies BALs for a contiguous span of blocks in strict order.
///
/// Each item is `(block_hash, expected_bal_hash, received_bal)`. Verification stops at the first
/// failure so BALs are only ever applied in order and after their hash is checked.
pub fn verify_in_order<I>(items: I) -> Result<Vec<(B256, DecodedBal)>, BalVerifyError>
where
    I: IntoIterator<Item = (B256, B256, Option<Bytes>)>,
{
    let mut verified = Vec::new();
    for (block, expected_hash, received) in items {
        let raw = received.ok_or(BalVerifyError::Missing(block))?;
        verified.push((block, verify_block_access_list(block, raw, expected_hash)?));
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eip7928::bal::Bal;

    /// RLP of an empty block access list (an empty list).
    fn empty_bal_raw() -> Bytes {
        Bytes::from_static(&[alloy_rlp::EMPTY_LIST_CODE])
    }

    fn empty_bal_hash() -> B256 {
        Bal::default().compute_hash()
    }

    #[test]
    fn accepts_canonical_bal() {
        let block = B256::with_last_byte(1);
        let decoded = verify_block_access_list(block, empty_bal_raw(), empty_bal_hash()).unwrap();
        assert_eq!(decoded.as_bal().compute_hash(), empty_bal_hash());
    }

    #[test]
    fn rejects_hash_mismatch() {
        let err = verify_block_access_list(B256::with_last_byte(1), empty_bal_raw(), B256::ZERO)
            .unwrap_err();
        assert!(matches!(err, BalVerifyError::HashMismatch { .. }));
    }

    #[test]
    fn rejects_malformed_payload() {
        // A list header announcing one byte of payload that is not present.
        let err = verify_block_access_list(
            B256::with_last_byte(1),
            Bytes::from_static(&[0xc1]),
            empty_bal_hash(),
        )
        .unwrap_err();
        assert!(matches!(err, BalVerifyError::Malformed { .. }));
    }

    #[test]
    fn verifies_span_in_order_and_fails_on_missing() {
        let (b0, b1) = (B256::with_last_byte(1), B256::with_last_byte(2));
        let verified = verify_in_order([
            (b0, empty_bal_hash(), Some(empty_bal_raw())),
            (b1, empty_bal_hash(), Some(empty_bal_raw())),
        ])
        .unwrap();
        assert_eq!(verified.len(), 2);
        assert_eq!(verified[0].0, b0);
        assert_eq!(verified[1].0, b1);

        // a required BAL the peer omitted is a hard failure
        let err = verify_in_order([(b0, empty_bal_hash(), None)]).unwrap_err();
        assert!(matches!(err, BalVerifyError::Missing(block) if block == b0));
    }
}
