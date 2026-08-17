//! Storage-range orchestration for account micro-batches.

use super::StateDownloader;
use crate::{error::SnapSyncError, MAX_REQUEST_ATTEMPTS, SNAP_RESPONSE_BYTES_LIMIT};
use alloy_primitives::{map::B256Map, B256};
use reth_db_api::transaction::DbTxMut;
use reth_downloaders::snap::{
    StorageRangeContinuation, StorageRangeDownloader, StorageRangeOutcome, VerifiedStorageRanges,
};
use reth_eth_wire_types::snap::{GetStorageRangesMessage, RangeBound};
use reth_network_p2p::{error::RequestError, snap::client::SnapClient};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::{HashedStorage, TrieAccount};

impl<C, F> StateDownloader<'_, C, F>
where
    C: SnapClient + Clone + Unpin + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider<Tx: DbTxMut> + StateWriter,
{
    /// Collects complete storage tries before their accounts are committed.
    pub(super) async fn collect_storage(
        &mut self,
        accounts: &[(B256, TrieAccount)],
    ) -> Result<Option<B256Map<HashedStorage>>, SnapSyncError> {
        let mut collected = accounts
            .iter()
            .map(|(account_hash, _)| (*account_hash, HashedStorage::new(true)))
            .collect::<B256Map<_>>();
        let mut account_index = 0;
        let mut starting_hash = B256::ZERO;

        while account_index < accounts.len() {
            let remaining = &accounts[account_index..];
            let Some(verified) = self.fetch_storage_ranges(remaining, starting_hash).await? else {
                return Ok(None)
            };

            for range in verified.ranges {
                collected
                    .get_mut(&range.account_hash)
                    .expect("the shared downloader only returns requested accounts")
                    .storage
                    .extend(range.slots);
            }

            match verified.continuation {
                None => return Ok(Some(collected)),
                Some(StorageRangeContinuation::Partial {
                    account_index: offset,
                    account_hash,
                    starting_hash: next,
                }) => {
                    account_index += offset;
                    debug_assert_eq!(accounts[account_index].0, account_hash);
                    starting_hash = next;
                }
                Some(StorageRangeContinuation::NextAccount {
                    account_index: offset,
                    account_hash,
                }) => {
                    account_index += offset;
                    debug_assert_eq!(accounts[account_index].0, account_hash);
                    starting_hash = B256::ZERO;
                }
            }
        }

        Ok(Some(collected))
    }

    /// Retries peer-attributed unavailability before declaring the target root stale.
    async fn fetch_storage_ranges(
        &mut self,
        accounts: &[(B256, TrieAccount)],
        starting_hash: B256,
    ) -> Result<Option<VerifiedStorageRanges>, SnapSyncError> {
        for _ in 0..MAX_REQUEST_ATTEMPTS {
            let request = GetStorageRangesMessage {
                request_id: self.next_request_id(),
                root_hash: self.root_hash,
                account_hashes: accounts.iter().map(|(hash, _)| *hash).collect(),
                starting_hash: starting_hash.into(),
                // Every account's storage trie is wanted whole, which snap/2 states as an
                // unbounded limit rather than a 32-byte maximum.
                limit_hash: RangeBound::default(),
                response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
            };
            let downloader = StorageRangeDownloader::new(
                self.client.clone(),
                request,
                accounts,
                self.runtime.clone(),
            )
            .map_err(|error| SnapSyncError::Network(error.to_string()))?;

            match downloader.await.map_err(storage_range_error)? {
                StorageRangeOutcome::Verified(range) => return Ok(Some(range)),
                StorageRangeOutcome::Unavailable { peer_id } => {
                    tracing::debug!(target: "snap", ?peer_id, "Peer lacks requested storage range");
                }
            }
        }

        Ok(None)
    }
}

fn storage_range_error(error: RequestError) -> SnapSyncError {
    if error == RequestError::UnsupportedCapability {
        SnapSyncError::NoSnapPeers
    } else {
        SnapSyncError::Network(format!("snap storage range request failed: {error}"))
    }
}
