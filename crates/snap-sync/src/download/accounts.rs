//! Account-range orchestration for the state downloader.

use super::{StateDownloader, MAX_HASH};
use crate::{error::SnapSyncError, MAX_REQUEST_ATTEMPTS, SNAP_RESPONSE_BYTES_LIMIT};
use alloy_primitives::B256;
use reth_db_api::transaction::DbTxMut;
use reth_downloaders::snap::{AccountRangeDownloader, AccountRangeOutcome};
use reth_eth_wire_types::snap::GetAccountRangeMessage;
use reth_network_p2p::{error::RequestError, snap::client::SnapClient};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};

impl<C, F> StateDownloader<'_, C, F>
where
    C: SnapClient + Clone + Unpin + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider<Tx: DbTxMut> + StateWriter,
{
    /// Tries unavailable peers independently because absence at one peer does not stale a target.
    pub(super) async fn fetch_account_range(
        &mut self,
        cursor: B256,
    ) -> Result<AccountRangeOutcome, SnapSyncError> {
        let mut unavailable = None;

        for _ in 0..MAX_REQUEST_ATTEMPTS {
            let request = GetAccountRangeMessage {
                request_id: self.next_request_id(),
                root_hash: self.root_hash,
                starting_hash: cursor,
                limit_hash: MAX_HASH,
                response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
            };
            let downloader =
                AccountRangeDownloader::new(self.client.clone(), request, self.runtime.clone())
                    .map_err(|error| SnapSyncError::Network(error.to_string()))?;

            match downloader.await.map_err(account_range_error)? {
                outcome @ AccountRangeOutcome::Verified(_) => return Ok(outcome),
                AccountRangeOutcome::Unavailable { peer_id } => unavailable = Some(peer_id),
            }
        }

        Ok(AccountRangeOutcome::Unavailable {
            peer_id: unavailable.expect("at least one peer answered unavailable"),
        })
    }
}

fn account_range_error(error: RequestError) -> SnapSyncError {
    if error == RequestError::UnsupportedCapability {
        SnapSyncError::NoSnapPeers
    } else {
        SnapSyncError::Network(format!("snap account range request failed: {error}"))
    }
}
