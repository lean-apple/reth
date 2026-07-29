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
use accounts::AccountRange;
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
use reth_trie::HashedPostState;
use storage::StorageRoots;
use tracing::debug;

/// Maximum number of account hashes per storage range request.
const STORAGE_BATCH_SIZE: usize = 20;

/// Maximum number of code hashes per bytecode request.
const BYTECODE_BATCH_SIZE: usize = 50;

/// Upper bound of the hashed key space.
const MAX_HASH: B256 = B256::new([0xff; 32]);

/// Downloads the hashed state at one state root from snap peers.
#[derive(Debug)]
pub struct StateDownloader<'a, C, F> {
    /// Peer client used for every snap request.
    client: &'a C,
    /// Sink for verified state.
    writer: SnapStateWriter<'a, F>,
    /// The state root every response is verified against.
    root_hash: B256,
    /// Monotonic counter correlating requests with responses.
    request_id: u64,
}

impl<'a, C, F> StateDownloader<'a, C, F>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    /// Creates a downloader for the state at `root_hash`.
    pub const fn new(client: &'a C, factory: &'a F, root_hash: B256) -> Self {
        Self { client, writer: SnapStateWriter::new(factory), root_hash, request_id: 0 }
    }

    /// Downloads accounts, storage and bytecodes starting from `starting_hash`.
    pub async fn run(
        &mut self,
        starting_hash: B256,
    ) -> Result<DownloadStateOutcome, SnapSyncError> {
        let mut cursor = starting_hash;

        loop {
            // Retrying a stale root restarts at the batch boundary, not mid-batch, so an account's
            // storage and code are never left half-written against a root we stopped trusting.
            let batch_start = cursor;

            let (decoded, exhausted) = match self.fetch_account_range(cursor).await? {
                AccountRange::Unavailable => {
                    return Ok(DownloadStateOutcome::Stale { resume_from: cursor })
                }
                AccountRange::PastTheEnd => return Ok(DownloadStateOutcome::Done),
                AccountRange::Verified { accounts, exhausted } => (accounts, exhausted),
            };

            let accounts = decoded
                .iter()
                .map(|(hash, account)| (*hash, Some(Account::from(*account))))
                .collect::<B256Map<_>>();
            let code_hashes = decoded
                .iter()
                .map(|(_, account)| account.code_hash)
                .filter(|hash| *hash != KECCAK256_EMPTY)
                .collect::<B256Set>();
            let storage_roots = StorageRoots(
                decoded.iter().map(|(hash, account)| (*hash, account.storage_root)).collect(),
            );

            debug!(
                target: "engine::snap",
                accounts = accounts.len(),
                root_hash = %self.root_hash,
                "Downloaded account range"
            );
            self.writer.write_state(HashedPostState { accounts, storages: B256Map::default() })?;

            let account_hashes: Vec<B256> = decoded.iter().map(|(hash, _)| *hash).collect();
            if self.download_storage(&account_hashes, &storage_roots).await? {
                return Ok(DownloadStateOutcome::Stale { resume_from: batch_start })
            }

            self.download_bytecodes(&code_hashes).await?;

            // An exhausted range was already checked against the root, so there is nothing after
            // it.
            let last_hash = account_hashes.last().copied().expect("checked non-empty above");
            if exhausted {
                return Ok(DownloadStateOutcome::Done)
            }
            let Some(next) = next_hash(last_hash) else { return Ok(DownloadStateOutcome::Done) };
            cursor = next;
        }
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
        debug!(target: "engine::snap", ?peer, %err, "Rejected snap response");
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
}

/// Returns the next hash after `hash`, or `None` at the end of the key space.
fn next_hash(hash: B256) -> Option<B256> {
    U256::from_be_bytes(hash.0).checked_add(U256::from(1)).map(B256::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b256(value: u64) -> B256 {
        B256::left_padding_from(&value.to_be_bytes())
    }

    #[test]
    fn next_hash_steps_and_stops_at_the_end() {
        assert_eq!(next_hash(B256::ZERO), Some(b256(1)));
        assert_eq!(next_hash(MAX_HASH), None);
    }
}
