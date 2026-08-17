//! Streaming download of accounts, storage and bytecodes at a fixed state root.
//!
//! [`StateDownloader`] walks the account trie in hashed order. Each account batch is verified
//! against the pivot root, written, and immediately followed by that batch's storage and
//! bytecodes before the next range is requested, so peak memory stays at one batch regardless of
//! how large the state is.

mod accounts;
mod bytecodes;
mod storage;

use crate::{error::SnapSyncError, store::SnapStateWriter};
use alloy_primitives::{
    map::{B256Map, B256Set},
    B256, KECCAK256_EMPTY, U256,
};
use reth_db_api::transaction::DbTxMut;
use reth_network_p2p::snap::client::SnapClient;
use reth_network_peers::PeerId;
use reth_primitives_traits::Account;
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_tasks::Runtime;
use reth_trie::{HashedPostState, TrieAccount};
use tracing::debug;

/// Maximum number of account hashes per storage range request.
const STORAGE_BATCH_SIZE: usize = 20;

/// Maximum number of code hashes per bytecode request.
const BYTECODE_BATCH_SIZE: usize = 50;

/// Downloads the hashed state at one state root from snap peers.
#[derive(Debug)]
pub struct StateDownloader<'a, C, F> {
    /// Peer client used for every snap request.
    client: C,
    /// Blocking executor used for peer-controlled proof verification.
    runtime: Runtime,
    /// Sink for verified state.
    writer: SnapStateWriter<'a, F>,
    /// The state root every response is verified against.
    root_hash: B256,
    /// Monotonic counter correlating requests with responses.
    request_id: u64,
}

impl<'a, C, F> StateDownloader<'a, C, F>
where
    C: SnapClient + Clone + Unpin + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider<Tx: DbTxMut> + StateWriter,
{
    /// Creates a downloader for the state at `root_hash`.
    pub const fn new(client: C, factory: &'a F, root_hash: B256, runtime: Runtime) -> Self {
        Self { client, runtime, writer: SnapStateWriter::new(factory), root_hash, request_id: 0 }
    }

    /// Downloads accounts, storage and bytecodes starting from `starting_hash`.
    ///
    /// A served account range is committed in micro-batches: nothing becomes durable until that
    /// batch's accounts, their complete storage and every bytecode they reference are in hand and
    /// written together. Writing accounts ahead of their storage would let a stale root strand
    /// accounts above the resume point, where the rolling target transition no longer reaches
    /// them and a later range at a fresher root would not mention the ones that had been deleted.
    pub async fn run(
        &mut self,
        starting_hash: B256,
    ) -> Result<DownloadStateOutcome, SnapSyncError> {
        let mut cursor = starting_hash;

        loop {
            let (decoded, exhausted) = match self.fetch_account_range(cursor).await {
                Ok(reth_downloaders::snap::AccountRangeOutcome::Unavailable { peer_id }) => {
                    debug!(target: "snap", ?peer_id, root_hash = %self.root_hash, "Snap peers no longer serve the target state");
                    return Ok(DownloadStateOutcome::Stale { resume_from: cursor })
                }
                Ok(reth_downloaders::snap::AccountRangeOutcome::Verified(range)) => {
                    if range.accounts.is_empty() {
                        return Ok(DownloadStateOutcome::Done)
                    }
                    (range.accounts, !range.has_more)
                }
                Err(SnapSyncError::NoSnapPeers) => {
                    return Ok(DownloadStateOutcome::WaitingForPeers { resume_from: cursor })
                }
                Err(err) => return Err(err),
            };

            debug!(
                target: "snap",
                accounts = decoded.len(),
                root_hash = %self.root_hash,
                "Verified account range"
            );

            for micro_batch in decoded.chunks(STORAGE_BATCH_SIZE) {
                // Resuming here re-downloads only this micro-batch, and everything below it is
                // already durable and complete.
                let resume_from = micro_batch[0].0;

                match self.commit_micro_batch(micro_batch).await {
                    Ok(true) => {}
                    Ok(false) => return Ok(DownloadStateOutcome::Stale { resume_from }),
                    Err(SnapSyncError::NoSnapPeers) => {
                        return Ok(DownloadStateOutcome::WaitingForPeers { resume_from })
                    }
                    Err(err) => return Err(err),
                }
            }

            // An exhausted range was already checked against the root, so there is nothing after
            // it.
            if exhausted {
                return Ok(DownloadStateOutcome::Done)
            }
            let last_hash = decoded.last().map(|(hash, _)| *hash).expect("range was not empty");
            let Some(next) = next_hash(last_hash) else { return Ok(DownloadStateOutcome::Done) };
            cursor = next;
        }
    }

    /// Assembles one micro-batch and commits it as a unit.
    ///
    /// Returns `false` when the root went stale part-way, in which case nothing was written.
    async fn commit_micro_batch(
        &mut self,
        batch: &[(B256, TrieAccount)],
    ) -> Result<bool, SnapSyncError> {
        let Some(storages) = self.collect_storage(batch).await? else { return Ok(false) };

        let code_hashes: B256Set = batch
            .iter()
            .map(|(_, account)| account.code_hash)
            .filter(|hash| *hash != KECCAK256_EMPTY)
            .collect();
        let bytecodes = self.collect_bytecodes(&code_hashes).await?;

        let accounts = batch
            .iter()
            .map(|(hash, account)| (*hash, Some(Account::from(*account))))
            .collect::<B256Map<_>>();

        self.writer.commit_batch(HashedPostState { accounts, storages }, &bytecodes)?;
        Ok(true)
    }

    const fn next_request_id(&mut self) -> u64 {
        self.request_id += 1;
        self.request_id
    }

    /// Reports a peer whose response could not be used, and returns the error to retry against.
    ///
    /// Every check that leads here is one a correct server passes, so the peer is downgraded and
    /// the request goes out again — the network layer then routes it elsewhere.
    fn penalize(&self, peer: PeerId, err: SnapSyncError) -> SnapSyncError {
        debug!(target: "snap", ?peer, %err, "Rejected snap response");
        self.client.report_bad_message(peer);
        err
    }
}

/// Result of a [`StateDownloader::run`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum DownloadStateOutcome {
    /// The whole account range was iterated and written; the state at the root is complete.
    Done,
    /// No peer could serve the requested root any more.
    ///
    /// Carries the account hash to resume from once the caller has a fresher root. State written
    /// before this point stays valid, because every batch was verified against the root it was
    /// served at.
    Stale {
        /// Account hash to resume the download from.
        resume_from: B256,
    },
    /// No connected peer advertises `snap/2`.
    ///
    /// Unlike [`Self::Stale`] the target is still fine; only the peer set is. Carries the same
    /// resume point so waiting costs nothing already downloaded.
    WaitingForPeers {
        /// Account hash to resume the download from.
        resume_from: B256,
    },
}

/// Returns the next hash after `hash`, or `None` at the end of the key space.
fn next_hash(hash: B256) -> Option<B256> {
    U256::from_be_bytes(hash.0).checked_add(U256::from(1)).map(B256::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_downloaders::snap::AccountRangeOutcome;
    use reth_eth_wire_types::snap::{
        AccountData, AccountRangeMessage, GetAccountRangeMessage, GetByteCodesMessage,
        GetStorageRangesMessage, MAX_HASH,
    };
    use reth_network_p2p::{
        download::DownloadClient, error::PeerRequestResult, priority::Priority,
        snap::client::SnapResponse,
    };
    use reth_network_peers::{PeerId, WithPeerId};
    use reth_provider::test_utils::create_test_provider_factory;
    use reth_trie_common::{HashBuilder, Nibbles, EMPTY_ROOT_HASH};
    use std::{
        collections::VecDeque,
        future::{ready, Ready},
        sync::{Arc, Mutex},
    };

    fn b256(value: u64) -> B256 {
        B256::left_padding_from(&value.to_be_bytes())
    }

    /// A client standing in for a network with no `snap/2` peer connected, which fails every snap
    /// request outright rather than queueing it.
    #[derive(Clone, Copy, Debug)]
    struct NoSnapPeers;

    impl DownloadClient for NoSnapPeers {
        fn report_bad_message(&self, _peer_id: reth_network_peers::PeerId) {
            panic!("a request that never reached a peer must not blame one")
        }

        fn num_connected_peers(&self) -> usize {
            0
        }
    }

    impl SnapClient for NoSnapPeers {
        type Output = Ready<PeerRequestResult<SnapResponse>>;

        fn get_account_range_with_priority(
            &self,
            _request: GetAccountRangeMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(reth_network_p2p::error::RequestError::UnsupportedCapability))
        }

        fn get_storage_ranges_with_priority(
            &self,
            _request: GetStorageRangesMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(reth_network_p2p::error::RequestError::UnsupportedCapability))
        }

        fn get_byte_codes_with_priority(
            &self,
            _request: GetByteCodesMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(reth_network_p2p::error::RequestError::UnsupportedCapability))
        }

        fn get_block_access_lists_with_priority(
            &self,
            _request: reth_eth_wire_types::snap::GetBlockAccessListsMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(reth_network_p2p::error::RequestError::UnsupportedCapability))
        }
    }

    #[derive(Clone, Debug)]
    struct AccountRangeClient {
        responses: Arc<Mutex<VecDeque<PeerRequestResult<SnapResponse>>>>,
        reported: Arc<Mutex<Vec<PeerId>>>,
    }

    impl DownloadClient for AccountRangeClient {
        fn report_bad_message(&self, peer_id: PeerId) {
            self.reported.lock().unwrap().push(peer_id);
        }

        fn num_connected_peers(&self) -> usize {
            3
        }
    }

    impl SnapClient for AccountRangeClient {
        type Output = Ready<PeerRequestResult<SnapResponse>>;

        fn get_account_range_with_priority(
            &self,
            _request: GetAccountRangeMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(self.responses.lock().unwrap().pop_front().expect("response available"))
        }

        fn get_storage_ranges_with_priority(
            &self,
            _request: GetStorageRangesMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(reth_network_p2p::error::RequestError::UnsupportedCapability))
        }

        fn get_byte_codes_with_priority(
            &self,
            _request: GetByteCodesMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(reth_network_p2p::error::RequestError::UnsupportedCapability))
        }

        fn get_block_access_lists_with_priority(
            &self,
            _request: reth_eth_wire_types::snap::GetBlockAccessListsMessage,
            _priority: Priority,
        ) -> Self::Output {
            ready(Err(reth_network_p2p::error::RequestError::UnsupportedCapability))
        }
    }

    #[test]
    fn next_hash_steps_and_stops_at_the_end() {
        assert_eq!(next_hash(B256::ZERO), Some(b256(1)));
        assert_eq!(next_hash(MAX_HASH), None);
    }

    #[tokio::test]
    async fn an_empty_peer_set_pauses_the_download_rather_than_ending_it() {
        let factory = create_test_provider_factory();
        let mut downloader =
            StateDownloader::new(NoSnapPeers, &factory, b256(0xabc), Runtime::test());

        // A session that starts before any snap peer connects would otherwise exhaust its retry
        // budget instantly and report a failed sync.
        let outcome = downloader.run(b256(7)).await.unwrap();

        assert_eq!(outcome, DownloadStateOutcome::WaitingForPeers { resume_from: b256(7) });
    }

    #[tokio::test]
    async fn unavailable_account_peers_do_not_stale_the_target_early() {
        let account_hash = b256(1);
        let account = TrieAccount {
            nonce: 1,
            balance: U256::from(2),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: KECCAK256_EMPTY,
        };
        let mut builder = HashBuilder::default();
        builder.add_leaf(Nibbles::unpack(account_hash), &alloy_rlp::encode(account));
        let root_hash = builder.root();
        let peers = [PeerId::random(), PeerId::random(), PeerId::random()];
        let responses = [
            AccountRangeMessage { request_id: 1, accounts: Vec::new(), proof: Vec::new() },
            AccountRangeMessage { request_id: 2, accounts: Vec::new(), proof: Vec::new() },
            AccountRangeMessage {
                request_id: 3,
                accounts: vec![AccountData::from_trie_account(account_hash, &account)],
                proof: Vec::new(),
            },
        ]
        .into_iter()
        .zip(peers)
        .map(|(response, peer_id)| {
            Ok(WithPeerId::new(peer_id, SnapResponse::AccountRange(response)))
        })
        .collect();
        let reported = Arc::new(Mutex::new(Vec::new()));
        let client = AccountRangeClient { responses: Arc::new(Mutex::new(responses)), reported };
        let factory = create_test_provider_factory();
        let mut downloader = StateDownloader::new(client, &factory, root_hash, Runtime::test());

        let outcome = downloader.fetch_account_range(B256::ZERO).await.unwrap();

        assert_eq!(
            outcome,
            AccountRangeOutcome::Verified(reth_downloaders::snap::VerifiedAccountRange {
                accounts: vec![(account_hash, account)],
                has_more: false,
            })
        );
        assert!(downloader.client.reported.lock().unwrap().is_empty());
    }
}
