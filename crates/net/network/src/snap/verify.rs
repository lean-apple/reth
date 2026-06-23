//! Verification of `snap/2` block access list responses.
//!
//! BALs are verified against the `block_access_list_hash` committed in the block header and applied
//! in strict block order; a required-but-missing BAL is a hard failure (EIP-8189).

use alloy_primitives::{keccak256, Bytes, B256};

/// Error verifying a `snap/2` block access list.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BalVerifyError {
    /// A BAL required to make progress was not provided by the peer.
    #[error("missing block access list for block {0}")]
    Missing(B256),
    /// The BAL payload does not hash to the header's `block_access_list_hash`.
    #[error("block access list hash mismatch for block {block}: got {got}, expected {expected}")]
    HashMismatch {
        /// Block the BAL belongs to.
        block: B256,
        /// Hash computed from the received payload.
        got: B256,
        /// Hash committed in the block header.
        expected: B256,
    },
}

/// Verifies a single BAL payload against the `block_access_list_hash` from its block header.
pub fn verify_block_access_list(
    block: B256,
    bal: &Bytes,
    expected_hash: B256,
) -> Result<(), BalVerifyError> {
    let got = keccak256(bal);
    if got == expected_hash {
        Ok(())
    } else {
        Err(BalVerifyError::HashMismatch { block, got, expected: expected_hash })
    }
}

/// Verifies BALs for a contiguous span of blocks in strict order.
///
/// Each item is `(block_hash, expected_bal_hash, received_bal)`. Verification stops at the first
/// failure so BALs are only ever applied in order and after their hash is checked.
pub fn verify_in_order<I>(items: I) -> Result<Vec<(B256, Bytes)>, BalVerifyError>
where
    I: IntoIterator<Item = (B256, B256, Option<Bytes>)>,
{
    let mut verified = Vec::new();
    for (block, expected_hash, received) in items {
        let bal = received.ok_or(BalVerifyError::Missing(block))?;
        verify_block_access_list(block, &bal, expected_hash)?;
        verified.push((block, bal));
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_matching_hash() {
        let bal = Bytes::from_static(b"block-access-list");
        let hash = keccak256(&bal);
        assert!(verify_block_access_list(B256::with_last_byte(1), &bal, hash).is_ok());
    }

    #[test]
    fn rejects_mismatched_hash() {
        let bal = Bytes::from_static(b"block-access-list");
        let err = verify_block_access_list(B256::with_last_byte(1), &bal, B256::ZERO).unwrap_err();
        assert!(matches!(err, BalVerifyError::HashMismatch { .. }));
    }

    #[test]
    fn verifies_span_in_order_and_fails_on_missing() {
        let bal0 = Bytes::from_static(b"bal-0");
        let bal1 = Bytes::from_static(b"bal-1");
        let (b0, b1) = (B256::with_last_byte(1), B256::with_last_byte(2));

        let ok = verify_in_order([
            (b0, keccak256(&bal0), Some(bal0.clone())),
            (b1, keccak256(&bal1), Some(bal1.clone())),
        ])
        .unwrap();
        assert_eq!(ok, vec![(b0, bal0), (b1, bal1.clone())]);

        // a required BAL the peer omitted is a hard failure
        let err = verify_in_order([(b0, keccak256(&bal1), None)]).unwrap_err();
        assert_eq!(err, BalVerifyError::Missing(b0));
    }
}
