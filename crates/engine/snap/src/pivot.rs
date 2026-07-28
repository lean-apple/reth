//! Pivot tracking and advancement.
//!
//! Serving nodes only keep a short window of historical state roots, so a download that takes
//! longer than that window has to move to a fresher root rather than fail. The tracker follows
//! the chain head reported by the engine and picks a pivot [`PIVOT_OFFSET`](crate::PIVOT_OFFSET)
//! blocks behind it, far enough back that the pivot's state is persisted rather than still in the
//! engine's in-memory overlay.

use crate::{SnapSyncError, SNAP_RESPONSE_BYTES_LIMIT};
use alloy_consensus::BlockHeader;
use alloy_eip7928::bal::RawBal;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{Bytes, B256};
use reth_db_api::transaction::DbTx;
use reth_eth_wire_types::snap::GetBlockAccessListsMessage;
use reth_network_p2p::{
    headers::client::HeadersClient,
    snap::client::{SnapClient, SnapResponse},
};
use reth_primitives_traits::SealedHeader;
use reth_provider::{DatabaseProviderFactory, HeaderProvider};
use reth_storage_api::DBProvider;
use std::collections::BTreeMap;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::info;

/// How far behind the pivot buffered blocks are kept, so a pivot advance does not discard blocks
/// that a later catch-up pass still needs.
const BUFFER_RETENTION: u64 = 2 * crate::PIVOT_OFFSET;

/// Tracks the block whose state is being downloaded and buffers what the engine reports about
/// newer blocks.
#[derive(Debug)]
pub struct PivotTracker {
    /// Current pivot block number.
    pivot_block: u64,
    /// State root at the current pivot.
    pivot_root: B256,
    /// Highest block number reported by the engine.
    known_head: u64,
    /// Hash of the highest block reported by the engine.
    known_head_hash: B256,
    /// Blocks seen since the pivot was set, keyed by block number.
    buffered_blocks: BTreeMap<u64, BufferedBlock>,
    /// Engine event stream.
    events: UnboundedReceiver<SnapSyncEvent>,
}

impl PivotTracker {
    /// Creates a tracker starting at the given pivot.
    pub const fn new(
        pivot_block: u64,
        pivot_root: B256,
        events: UnboundedReceiver<SnapSyncEvent>,
    ) -> Self {
        Self {
            pivot_block,
            pivot_root,
            known_head: 0,
            known_head_hash: B256::ZERO,
            buffered_blocks: BTreeMap::new(),
            events,
        }
    }

    /// Returns the current pivot block number.
    pub const fn pivot_block(&self) -> u64 {
        self.pivot_block
    }

    /// Returns the state root at the current pivot.
    pub const fn pivot_root(&self) -> B256 {
        self.pivot_root
    }

    /// Returns the highest block number reported by the engine.
    pub const fn known_head(&self) -> u64 {
        self.known_head
    }

    /// Returns the hash of the highest block reported by the engine.
    pub const fn known_head_hash(&self) -> B256 {
        self.known_head_hash
    }

    /// Consumes every event queued by the engine without blocking.
    pub fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.apply_event(event);
        }
    }

    /// Moves the pivot up to [`PIVOT_OFFSET`](crate::PIVOT_OFFSET) blocks behind the known head.
    ///
    /// Returns `false` when the head has not advanced far enough for a new pivot to exist, in
    /// which case the caller has to wait for more engine events.
    pub async fn advance_pivot<C, F>(
        &mut self,
        client: &C,
        factory: &F,
    ) -> Result<bool, SnapSyncError>
    where
        C: HeadersClient + 'static,
        F: DatabaseProviderFactory,
        F::Provider: DBProvider + HeaderProvider<Header = C::Header>,
        <F::Provider as DBProvider>::Tx: DbTx,
    {
        self.drain_events();

        let new_pivot = self.known_head.saturating_sub(crate::PIVOT_OFFSET);
        if new_pivot <= self.pivot_block {
            return Ok(false)
        }

        let old_pivot = self.pivot_block;
        let new_root = self.resolve_state_root(client, factory, new_pivot).await?;

        self.pivot_block = new_pivot;
        self.pivot_root = new_root;
        self.buffered_blocks =
            self.buffered_blocks.split_off(&new_pivot.saturating_sub(BUFFER_RETENTION));

        info!(target: "engine::snap", old_pivot, new_pivot, %new_root, "Advanced snap sync pivot");

        Ok(true)
    }

    /// Returns the block access list for `block_number`, verified against the header's
    /// commitment.
    ///
    /// Prefers the BAL the engine already delivered with the payload and falls back to a snap/2
    /// `GetBlockAccessLists` request.
    pub async fn verified_bal<C, F>(
        &self,
        client: &C,
        factory: &F,
        block_number: u64,
    ) -> Result<Bytes, SnapSyncError>
    where
        C: SnapClient + HeadersClient + 'static,
        F: DatabaseProviderFactory,
        F::Provider: DBProvider + HeaderProvider<Header = C::Header>,
        <F::Provider as DBProvider>::Tx: DbTx,
    {
        let (block_hash, expected) = self.resolve_commitment(client, factory, block_number).await?;

        let bal = match self.buffered_blocks.get(&block_number).and_then(|block| block.bal.clone())
        {
            Some(bal) => bal,
            None => self.fetch_bal(client, block_number, block_hash).await?,
        };

        if RawBal::new(bal.clone()).hash() != expected {
            return Err(SnapSyncError::BalVerification { block: block_number, expected })
        }

        Ok(bal)
    }

    fn apply_event(&mut self, event: SnapSyncEvent) {
        match event {
            SnapSyncEvent::NewBlock { number, hash, state_root, bal } => {
                self.buffered_blocks.insert(number, BufferedBlock { state_root, bal });
                if number > self.known_head {
                    self.known_head = number;
                    self.known_head_hash = hash;
                }
            }
            SnapSyncEvent::NewHead { number, hash } => {
                if number > self.known_head {
                    self.known_head = number;
                    self.known_head_hash = hash;
                }
            }
        }
    }

    async fn fetch_bal<C>(
        &self,
        client: &C,
        block_number: u64,
        block_hash: B256,
    ) -> Result<Bytes, SnapSyncError>
    where
        C: SnapClient + 'static,
    {
        let response = client
            .get_block_access_lists(GetBlockAccessListsMessage {
                request_id: 0,
                block_hashes: vec![block_hash],
                response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
            })
            .await
            .map_err(|err| {
                SnapSyncError::Network(format!("snap BAL request for block {block_number}: {err}"))
            })?;

        let SnapResponse::BlockAccessLists(msg) = response.into_data() else {
            return Err(SnapSyncError::Network(format!(
                "expected a block access lists response for block {block_number}"
            )))
        };

        // Peers signal "I don't have this one" with an empty entry rather than a short reply.
        msg.block_access_lists
            .0
            .into_iter()
            .next()
            .flatten()
            .ok_or(SnapSyncError::MissingBal(block_number))
    }

    /// Returns the block hash and access-list commitment for a block, from the local database if
    /// it has the header and from peers otherwise.
    async fn resolve_commitment<C, F>(
        &self,
        client: &C,
        factory: &F,
        block_number: u64,
    ) -> Result<(B256, B256), SnapSyncError>
    where
        C: HeadersClient + 'static,
        F: DatabaseProviderFactory,
        F::Provider: DBProvider + HeaderProvider<Header = C::Header>,
        <F::Provider as DBProvider>::Tx: DbTx,
    {
        let header = match self.local_header(factory, block_number) {
            Some(header) => header,
            None => self.fetch_header(client, block_number).await?,
        };

        let commitment =
            header.block_access_list_hash().ok_or(SnapSyncError::MissingBal(block_number))?;
        Ok((SealedHeader::seal_slow(header).hash(), commitment))
    }

    /// Returns the state root for a block, from the engine buffer, the local database, or peers.
    async fn resolve_state_root<C, F>(
        &self,
        client: &C,
        factory: &F,
        block_number: u64,
    ) -> Result<B256, SnapSyncError>
    where
        C: HeadersClient + 'static,
        F: DatabaseProviderFactory,
        F::Provider: DBProvider + HeaderProvider<Header = C::Header>,
        <F::Provider as DBProvider>::Tx: DbTx,
    {
        if let Some(block) = self.buffered_blocks.get(&block_number) {
            return Ok(block.state_root)
        }

        match self.local_header(factory, block_number) {
            Some(header) => Ok(header.state_root()),
            None => Ok(self.fetch_header(client, block_number).await?.state_root()),
        }
    }

    fn local_header<F>(
        &self,
        factory: &F,
        block_number: u64,
    ) -> Option<<F::Provider as HeaderProvider>::Header>
    where
        F: DatabaseProviderFactory,
        F::Provider: DBProvider + HeaderProvider,
        <F::Provider as DBProvider>::Tx: DbTx,
    {
        factory
            .database_provider_ro()
            .ok()
            .and_then(|provider| provider.header_by_number(block_number).ok().flatten())
    }

    async fn fetch_header<C>(
        &self,
        client: &C,
        block_number: u64,
    ) -> Result<C::Header, SnapSyncError>
    where
        C: HeadersClient + 'static,
    {
        client
            .get_header(BlockHashOrNumber::Number(block_number))
            .await
            .map_err(|err| {
                SnapSyncError::Network(format!("header request for block {block_number}: {err}"))
            })?
            .into_data()
            .ok_or(SnapSyncError::MissingHeader(block_number))
    }
}

/// What the engine tells the snap sync loop about chain progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapSyncEvent {
    /// A block arrived via `newPayload`, carrying its access list when the payload had one.
    NewBlock {
        /// Block number.
        number: u64,
        /// Block hash.
        hash: B256,
        /// State root from the block header.
        state_root: B256,
        /// RLP-encoded block access list, when the payload carried one.
        bal: Option<Bytes>,
    },
    /// The canonical head changed via `forkchoiceUpdated`.
    NewHead {
        /// Head block number.
        number: u64,
        /// Head block hash.
        hash: B256,
    },
}

/// A block the engine reported that has not been applied yet.
#[derive(Debug, Clone)]
struct BufferedBlock {
    /// State root from the block header.
    state_root: B256,
    /// RLP-encoded block access list, when the payload carried one.
    bal: Option<Bytes>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn tracker(pivot: u64) -> (PivotTracker, tokio::sync::mpsc::UnboundedSender<SnapSyncEvent>) {
        let (tx, rx) = unbounded_channel();
        (PivotTracker::new(pivot, B256::ZERO, rx), tx)
    }

    fn new_head(number: u64) -> SnapSyncEvent {
        SnapSyncEvent::NewHead { number, hash: B256::left_padding_from(&number.to_be_bytes()) }
    }

    #[test]
    fn head_only_moves_forward() {
        let (mut tracker, tx) = tracker(0);
        tx.send(new_head(100)).unwrap();
        tx.send(new_head(50)).unwrap();

        tracker.drain_events();

        assert_eq!(tracker.known_head(), 100);
        assert_eq!(tracker.known_head_hash(), B256::left_padding_from(&100u64.to_be_bytes()));
    }

    #[test]
    fn new_block_events_advance_the_head_and_buffer_the_root() {
        let (mut tracker, tx) = tracker(0);
        let state_root = B256::repeat_byte(7);
        tx.send(SnapSyncEvent::NewBlock {
            number: 42,
            hash: B256::repeat_byte(1),
            state_root,
            bal: None,
        })
        .unwrap();

        tracker.drain_events();

        assert_eq!(tracker.known_head(), 42);
        assert_eq!(
            tracker.buffered_blocks.get(&42).map(|block| block.state_root),
            Some(state_root)
        );
    }

    #[test]
    fn dropped_event_sender_does_not_block_draining() {
        let (mut tracker, tx) = tracker(0);
        tx.send(new_head(10)).unwrap();
        drop(tx);

        tracker.drain_events();

        assert_eq!(tracker.known_head(), 10);
    }
}
