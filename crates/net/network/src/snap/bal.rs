//! Client-side block access list fetch and verification for snap/2 catch-up.

use crate::snap::verify::{verify_in_order, BalVerifyError};
use alloy_primitives::{Bytes, B256};
use reth_eth_wire_types::snap::GetBlockAccessListsMessage;
use reth_network_p2p::{
    error::RequestError,
    snap::client::{SnapClient, SnapResponse},
};

/// A block whose BAL is requested, paired with the hash committed in its header.
#[derive(Debug, Clone, Copy)]
pub struct BalRequest {
    /// Block hash whose BAL is requested.
    pub block_hash: B256,
    /// `block_access_list_hash` committed in the block header.
    pub expected_hash: B256,
}

/// Error fetching or verifying BALs from a peer.
#[derive(Debug, thiserror::Error)]
pub enum BalFetchError {
    /// A returned BAL failed verification against its header.
    #[error(transparent)]
    Verify(#[from] BalVerifyError),
    /// The peer request failed.
    #[error("snap peer request failed: {0}")]
    Request(#[from] RequestError),
    /// The peer replied with a message other than `BlockAccessLists`.
    #[error("peer responded with an unexpected snap message")]
    UnexpectedResponse,
}

/// Fetches and verifies BALs for the given blocks, in order, from a snap/2 peer.
///
/// Returns the verified `(block_hash, bal)` pairs for the prefix the peer returned. Responses may
/// be tail-truncated, so fewer pairs than requested can come back and the caller should request the
/// remainder; a returned-but-empty entry is a genuine missing BAL and fails verification.
pub async fn fetch_and_verify_bals<C: SnapClient>(
    client: &C,
    request_id: u64,
    blocks: &[BalRequest],
    response_bytes: u64,
) -> Result<Vec<(B256, Bytes)>, BalFetchError> {
    let request = GetBlockAccessListsMessage {
        request_id,
        block_hashes: blocks.iter().map(|b| b.block_hash).collect(),
        response_bytes,
    };

    let response = client.get_block_access_lists(request).await?;
    let SnapResponse::BlockAccessLists(message) = response.into_data() else {
        return Err(BalFetchError::UnexpectedResponse);
    };

    // Zip the returned prefix back with the requested blocks and verify in strict order.
    let items = blocks
        .iter()
        .zip(message.block_access_lists.0)
        .map(|(req, bal)| (req.block_hash, req.expected_hash, bal));
    Ok(verify_in_order(items)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use core::future::{ready, Ready};
    use reth_eth_wire_types::{
        snap::{
            BlockAccessListsMessage, GetAccountRangeMessage, GetByteCodesMessage,
            GetStorageRangesMessage, GetTrieNodesMessage,
        },
        BlockAccessLists,
    };
    use reth_network_p2p::{download::DownloadClient, priority::Priority};
    use reth_network_peers::{PeerId, WithPeerId};

    /// Client that answers every `GetBlockAccessLists` with a fixed response.
    #[derive(Debug)]
    struct MockClient {
        peer: PeerId,
        response: BlockAccessListsMessage,
    }

    type Out = Ready<reth_network_p2p::error::PeerRequestResult<SnapResponse>>;

    impl DownloadClient for MockClient {
        fn report_bad_message(&self, _peer_id: PeerId) {}
        fn num_connected_peers(&self) -> usize {
            1
        }
    }

    impl SnapClient for MockClient {
        type Output = Out;

        fn get_account_range_with_priority(
            &self,
            _request: GetAccountRangeMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(RequestError::UnsupportedCapability))
        }

        fn get_storage_ranges(&self, _request: GetStorageRangesMessage) -> Self::Output {
            ready(Err(RequestError::UnsupportedCapability))
        }

        fn get_storage_ranges_with_priority(
            &self,
            _request: GetStorageRangesMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(RequestError::UnsupportedCapability))
        }

        fn get_byte_codes(&self, _request: GetByteCodesMessage) -> Self::Output {
            ready(Err(RequestError::UnsupportedCapability))
        }

        fn get_byte_codes_with_priority(
            &self,
            _request: GetByteCodesMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(RequestError::UnsupportedCapability))
        }

        fn get_trie_nodes(&self, _request: GetTrieNodesMessage) -> Self::Output {
            ready(Err(RequestError::UnsupportedCapability))
        }

        fn get_trie_nodes_with_priority(
            &self,
            _request: GetTrieNodesMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(RequestError::UnsupportedCapability))
        }

        fn get_block_access_lists_with_priority(
            &self,
            _request: GetBlockAccessListsMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Ok(WithPeerId::new(
                self.peer,
                SnapResponse::BlockAccessLists(self.response.clone()),
            )))
        }
    }

    fn client_returning(bals: Vec<Option<Bytes>>) -> MockClient {
        MockClient {
            peer: PeerId::random(),
            response: BlockAccessListsMessage {
                request_id: 1,
                block_access_lists: BlockAccessLists(bals),
            },
        }
    }

    #[tokio::test]
    async fn fetches_and_verifies_matching_bals() {
        let bal = Bytes::from_static(b"bal-0");
        let block = B256::with_last_byte(1);
        let client = client_returning(vec![Some(bal.clone())]);

        let verified = fetch_and_verify_bals(
            &client,
            1,
            &[BalRequest { block_hash: block, expected_hash: keccak256(&bal) }],
            u64::MAX,
        )
        .await
        .unwrap();

        assert_eq!(verified, vec![(block, bal)]);
    }

    #[tokio::test]
    async fn rejects_bal_with_wrong_hash() {
        let client = client_returning(vec![Some(Bytes::from_static(b"bal-0"))]);
        let err = fetch_and_verify_bals(
            &client,
            1,
            &[BalRequest { block_hash: B256::with_last_byte(1), expected_hash: B256::ZERO }],
            u64::MAX,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BalFetchError::Verify(BalVerifyError::HashMismatch { .. })));
    }

    #[tokio::test]
    async fn missing_bal_in_returned_prefix_is_rejected() {
        let client = client_returning(vec![None]);
        let err = fetch_and_verify_bals(
            &client,
            1,
            &[BalRequest { block_hash: B256::with_last_byte(1), expected_hash: B256::ZERO }],
            u64::MAX,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BalFetchError::Verify(BalVerifyError::Missing(_))));
    }
}
