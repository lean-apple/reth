//! The snap sync session: one serialized state machine that owns a single state generation.
//!
//! Pivot advancement and reorgs are transitions of this machine rather than separate subsystems.
//! Keeping them here is what makes the ordering rules enforceable: a session starts from a clean
//! generation, advances its covered prefix only once the state behind it is durable, and moves its
//! target only through an explicit transition that reconciles what was already downloaded.

use crate::{
    chain::{BlockRef, CanonicalChainSource},
    download::{DownloadStateOutcome, StateDownloader},
    error::SnapSyncError,
    heal::{decode_block_access_list, BlockStateDiff},
    metrics::SnapSyncMetrics,
    store::{SnapGeneration, SnapStateWriter},
    MAX_REQUEST_ATTEMPTS, PIVOT_OFFSET, SNAP_RESPONSE_BYTES_LIMIT,
};
use alloy_eip7928::bal::RawBal;
use alloy_primitives::{Bytes, B256};
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_eth_wire_types::snap::GetBlockAccessListsMessage;
use reth_network_p2p::{
    error::RequestError,
    snap::client::{SnapClient, SnapResponse},
};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{BalStoreHandle, DBProvider, StateWriter, StorageSettingsCache, TrieWriter};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info};

/// Drives one snap sync from a clean state generation to a verified state root.
#[derive(Debug)]
pub struct SnapSyncSession<C, F, H> {
    /// Peer client for every snap request.
    client: C,
    /// Provider factory the state is assembled into.
    factory: F,
    /// Where canonicality comes from.
    chain: H,
    /// Access lists the node already holds, shared with the rest of the node.
    ///
    /// Only an optimization: a session falls back to requesting a list from peers, and verifies
    /// it against the header commitment either way.
    bal_store: BalStoreHandle,
    /// Where the session currently is.
    state: SyncState,
    /// Progress counters for this session.
    metrics: SnapSyncMetrics,
    /// Monotonic counter correlating this session's own requests with responses.
    ///
    /// Atomic only because requests are issued through `&self`; the session itself is serial.
    request_id: AtomicU64,
}

impl<C, F, H> SnapSyncSession<C, F, H>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory,
    F::Provider: DBProvider,
    F::ProviderRW: DBProvider + StateWriter + TrieWriter + StorageSettingsCache,
    <F::Provider as DBProvider>::Tx: DbTx,
    <F::ProviderRW as DBProvider>::Tx: DbTx + DbTxMut,
    H: CanonicalChainSource,
{
    /// Creates an idle session.
    pub fn new(client: C, factory: F, chain: H, bal_store: BalStoreHandle) -> Self {
        Self {
            client,
            factory,
            chain,
            bal_store,
            state: SyncState::Idle,
            metrics: SnapSyncMetrics::default(),
            request_id: AtomicU64::new(0),
        }
    }

    /// Returns what the session is currently doing.
    pub const fn state(&self) -> &SyncState {
        &self.state
    }

    /// Discards any previous generation and picks a target behind the canonical head.
    ///
    /// The target is reached by following parent links rather than subtracting from the head's
    /// height, so it is a block on *this* chain. Starting clean is what keeps a failed attempt or
    /// a pre-existing genesis state from being mistaken for downloaded state.
    pub async fn start(&mut self) -> Result<BlockRef, SnapSyncError> {
        let head = self.chain.head();
        let target = self
            .chain
            .ancestor(head.hash, PIVOT_OFFSET)
            .await
            .map_err(|err| SnapSyncError::Network(format!("resolving a sync target: {err}")))?;

        self.writer().begin_generation(SnapGeneration {
            target_block: target.number,
            target_hash: target.hash,
            state_root: target.state_root,
        })?;
        self.state = SyncState::Downloading { target, covered_end: B256::ZERO };

        info!(target: "snap", number = target.number, hash = %target.hash, "Started snap sync");
        Ok(target)
    }

    /// Downloads state at the target until the whole account range is covered.
    ///
    /// A peer that no longer serves the target's root ends the step with the covered prefix
    /// recorded, so the caller can advance the target and resume rather than start over.
    pub async fn download(&mut self) -> Result<StepOutcome, SnapSyncError> {
        let SyncState::Downloading { target, covered_end } = self.state else {
            return Err(SnapSyncError::Network("session is not downloading".into()))
        };

        let mut downloader = StateDownloader::new(&self.client, &self.factory, target.state_root);
        match downloader.run(covered_end).await? {
            DownloadStateOutcome::Done => {
                self.state = SyncState::Healing { target, applied: target };
                Ok(StepOutcome::Advanced)
            }
            DownloadStateOutcome::Stale { resume_from } => {
                self.metrics.targets_stale.increment(1);
                self.state = SyncState::Downloading { target, covered_end: resume_from };
                Ok(StepOutcome::TargetStale)
            }
            DownloadStateOutcome::WaitingForPeers { resume_from } => {
                self.metrics.waits_for_peers.increment(1);
                self.state = SyncState::Downloading { target, covered_end: resume_from };
                Ok(StepOutcome::WaitingForPeers)
            }
        }
    }

    /// Moves the session to a fresher target, carrying the downloaded prefix across.
    ///
    /// This is EIP-8189's rolling transition and the normal answer to a target no peer will serve
    /// any more. The prefix below `covered_end` was assembled at the old target's root, so every
    /// access list between the two targets is applied to it; skipping that would leave a prefix
    /// from one state beside a suffix from another, which matches no block at all.
    ///
    /// Returns [`StepOutcome::TargetStale`] when the chain has not moved far enough to offer a
    /// newer target, and [`StepOutcome::Reorged`] when the old target is no longer an ancestor of
    /// the new one, which leaves the prefix unreconcilable and restarts the session.
    pub async fn advance_target(&mut self) -> Result<StepOutcome, SnapSyncError> {
        let SyncState::Downloading { target, covered_end } = self.state else {
            return Err(SnapSyncError::Network("session is not downloading".into()))
        };

        let head = self.chain.head();
        let new_target = self
            .chain
            .ancestor(head.hash, PIVOT_OFFSET)
            .await
            .map_err(|err| SnapSyncError::Network(format!("resolving a sync target: {err}")))?;

        if new_target.hash == target.hash {
            return Ok(StepOutcome::TargetStale)
        }

        let segment = match self.chain.segment(target.hash, new_target.hash).await {
            Ok(segment) => segment,
            Err(err) => {
                debug!(target: "snap", %err, "Target left the canonical chain");
                self.state = SyncState::Idle;
                return Ok(StepOutcome::Reorged)
            }
        };

        for block in segment {
            let bal = match self.verified_bal(&block).await {
                Ok(bal) => bal,
                // The target is left where it was, so a later attempt walks this segment again
                // and re-applies the blocks handled so far. An access list states post-block
                // values rather than deltas, so applying one twice lands on the same state.
                Err(SnapSyncError::NoSnapPeers) => {
                    self.metrics.waits_for_peers.increment(1);
                    return Ok(StepOutcome::WaitingForPeers)
                }
                Err(err) => return Err(err),
            };
            let changes = decode_block_access_list(&bal, block.number)?;
            BlockStateDiff::from_changes(&changes).apply(self.writer(), Some(covered_end))?;
            self.metrics.access_lists_applied.increment(1);
        }

        info!(
            target: "snap",
            from = target.number,
            to = new_target.number,
            "Advanced snap sync target"
        );
        self.state = SyncState::Downloading { target: new_target, covered_end };
        Ok(StepOutcome::Advanced)
    }

    /// Applies the access lists from the current target up to the canonical head.
    ///
    /// Each block is taken by hash from a segment walked over parent links, so a head that moved
    /// sideways or backwards yields a different segment rather than a mismatched height.
    pub async fn heal(&mut self) -> Result<StepOutcome, SnapSyncError> {
        let SyncState::Healing { target, applied } = self.state else {
            return Err(SnapSyncError::Network("session is not healing".into()))
        };

        let head = self.chain.head();
        let segment = match self.chain.segment(applied.hash, head.hash).await {
            Ok(segment) => segment,
            // The applied segment is no longer on the canonical chain. Applying an access list
            // records no pre-image, so there is nothing to roll back to; the session restarts.
            Err(err) => {
                debug!(target: "snap", %err, "Applied segment left the canonical chain");
                self.state = SyncState::Idle;
                return Ok(StepOutcome::Reorged)
            }
        };

        let mut applied = applied;
        for block in segment {
            let bal = match self.verified_bal(&block).await {
                Ok(bal) => bal,
                // Record the blocks that did land before waiting, so the next attempt resumes
                // from here rather than replaying the segment from the target.
                Err(SnapSyncError::NoSnapPeers) => {
                    self.state = SyncState::Healing { target, applied };
                    self.metrics.waits_for_peers.increment(1);
                    return Ok(StepOutcome::WaitingForPeers)
                }
                Err(err) => return Err(err),
            };
            let changes = decode_block_access_list(&bal, block.number)?;
            BlockStateDiff::from_changes(&changes).apply(self.writer(), None)?;

            applied = block;
            self.metrics.access_lists_applied.increment(1);
            debug!(target: "snap", number = block.number, "Applied block access list");
        }

        self.state = SyncState::Healing { target, applied };
        Ok(StepOutcome::Advanced)
    }

    /// Rebuilds the state trie, checks its root, and persists the trie tables.
    ///
    /// The block the state was assembled for is re-anchored against forkchoice first. The head
    /// can move while access lists are being applied, and a root that matches an orphaned block
    /// is still a root that matches nothing the node will build on.
    pub async fn finalize(&mut self) -> Result<BlockRef, SnapSyncError> {
        let SyncState::Healing { applied, .. } = self.state else {
            return Err(SnapSyncError::Network("session has nothing to finalize".into()))
        };

        let token = self.chain.canonical_token();
        self.ensure_canonical(applied).await?;

        self.writer().finalize_sync(applied.number, applied.state_root)?;

        // Rebuilding the trie walks the whole state, long enough for forkchoice to move
        // underneath it and leave the check above stale. The work is only trusted if no
        // forkchoice update landed while it was running; the trie tables it wrote are rebuilt
        // from hashed state on the next attempt either way.
        if self.chain.canonical_token() != token {
            self.ensure_canonical(applied).await?;
        }

        self.state = SyncState::Complete { at: applied };

        info!(target: "snap", number = applied.number, hash = %applied.hash, "Snap sync complete");
        Ok(applied)
    }

    /// Fails when `block` is no longer on the canonical chain, resetting the session.
    async fn ensure_canonical(&mut self, block: BlockRef) -> Result<(), SnapSyncError> {
        let head = self.chain.head();
        if block.hash == head.hash || self.chain.segment(block.hash, head.hash).await.is_ok() {
            return Ok(())
        }

        self.state = SyncState::Idle;
        Err(SnapSyncError::Reorged(block.hash))
    }

    /// Returns a block's access list, verified against the header's commitment.
    ///
    /// Prefers a list the engine already cached for this hash and falls back to a snap/2 request.
    async fn verified_bal(&self, block: &BlockRef) -> Result<Bytes, SnapSyncError> {
        let expected = block.bal_hash.ok_or(SnapSyncError::MissingBal(block.number))?;

        let cached = self
            .bal_store
            .get_by_hashes(core::slice::from_ref(&block.hash))
            .ok()
            .and_then(|mut found| found.pop().flatten());

        let (peer, bal) = match cached {
            // Already held by the node, so there is no peer to hold to account.
            Some(bal) => (None, bal),
            None => {
                let (peer, bal) = self.fetch_bal(block).await?;
                (Some(peer), bal)
            }
        };

        if RawBal::new(bal.clone()).hash() != expected {
            if let Some(peer) = peer {
                self.client.report_bad_message(peer);
            }
            return Err(SnapSyncError::BalVerification { block: block.number, expected })
        }

        Ok(bal)
    }

    /// Requests a block's access list, retrying with another peer on an unusable response.
    async fn fetch_bal(
        &self,
        block: &BlockRef,
    ) -> Result<(reth_network_peers::PeerId, Bytes), SnapSyncError> {
        let mut last_error = None;

        for _ in 0..MAX_REQUEST_ATTEMPTS {
            match self.request_bal(block).await {
                Ok(found) => return Ok(found),
                Err(SnapSyncError::NoSnapPeers) => return Err(SnapSyncError::NoSnapPeers),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error.expect("at least one attempt was made"))
    }

    async fn request_bal(
        &self,
        block: &BlockRef,
    ) -> Result<(reth_network_peers::PeerId, Bytes), SnapSyncError> {
        let response = self
            .client
            .get_block_access_lists(GetBlockAccessListsMessage {
                request_id: self.request_id.fetch_add(1, Ordering::Relaxed),
                block_hashes: vec![block.hash],
                response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
            })
            .await
            .map_err(|err| match err {
                // Spending an attempt cannot help: the network layer rejects snap requests
                // outright while no connected peer advertises the capability.
                RequestError::UnsupportedCapability => SnapSyncError::NoSnapPeers,
                err => {
                    SnapSyncError::Network(format!("snap BAL request for {}: {err}", block.hash))
                }
            })?;

        let (peer, data) = response.split();
        let SnapResponse::BlockAccessLists(msg) = data else {
            self.client.report_bad_message(peer);
            return Err(SnapSyncError::Network(format!(
                "expected a block access lists response for {}",
                block.hash
            )))
        };

        // Peers signal "I don't have this one" with an empty entry rather than a short reply, so
        // an absent entry is a legitimate answer and not grounds for penalizing.
        let bal = msg
            .block_access_lists
            .0
            .into_iter()
            .next()
            .flatten()
            .ok_or(SnapSyncError::MissingBal(block.number))?;

        Ok((peer, bal))
    }

    const fn writer(&self) -> SnapStateWriter<'_, F> {
        SnapStateWriter::new(&self.factory)
    }
}

/// Where a session is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    /// No generation in progress.
    Idle,
    /// Streaming state at `target`; accounts below `covered_end` are durable.
    Downloading {
        /// The block whose state is being downloaded.
        target: BlockRef,
        /// Account hash the next range resumes from.
        covered_end: B256,
    },
    /// State at `target` is complete; access lists are being applied toward the head.
    Healing {
        /// The block the download completed at.
        target: BlockRef,
        /// The highest block whose access list has been applied.
        applied: BlockRef,
    },
    /// The assembled state was verified against a header.
    Complete {
        /// The block the state corresponds to.
        at: BlockRef,
    },
}

/// What one step of the session accomplished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step made progress and the session moved on.
    Advanced,
    /// No peer serves the target's root; the target has to move before the download can resume.
    TargetStale,
    /// No connected peer advertises `snap/2`; the step can be retried once one does.
    ///
    /// Progress made before the peer set ran out is recorded, so waiting costs nothing.
    WaitingForPeers,
    /// The chain moved out from under the session, which has been reset.
    Reorged,
}
