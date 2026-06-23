//! The snap/2 state sync stage (EIP-8189).
//!
//! Downloads account/storage/code ranges at a frozen pivot, then applies verified block access list
//! diffs up to the head and checks the state root. Pivot selection, restart on reorg and progress
//! checkpoints are owned by the [`Pipeline`](crate::Pipeline); this stage only drives the
//! [`SnapClient`] and persists the downloaded state.
//!
//! Skeleton: state persistence is not yet wired, so the stage makes no progress and is not part of
//! the default pipeline.

use reth_network_p2p::snap::client::SnapClient;
use reth_stages_api::{
    ExecInput, ExecOutput, Stage, StageCheckpoint, StageError, StageId, UnwindInput, UnwindOutput,
};

/// Downloads and applies state via the `snap/2` protocol.
#[derive(Debug, Clone)]
pub struct SnapSyncStage<C> {
    /// Client used to request account/storage/code ranges and block access lists from peers.
    client: C,
    /// Number of blocks of BAL catch-up applied per chunk, bounding peak memory.
    catch_up_chunk: u64,
}

impl<C> SnapSyncStage<C> {
    /// Creates a new snap/2 sync stage backed by the given client.
    pub const fn new(client: C, catch_up_chunk: u64) -> Self {
        Self { client, catch_up_chunk }
    }

    /// The client used to request state from peers.
    pub const fn client(&self) -> &C {
        &self.client
    }

    /// The configured BAL catch-up chunk size.
    pub const fn catch_up_chunk(&self) -> u64 {
        self.catch_up_chunk
    }
}

impl<Provider, C> Stage<Provider> for SnapSyncStage<C>
where
    C: SnapClient + 'static,
{
    fn id(&self) -> StageId {
        StageId::Other("SnapSync")
    }

    fn execute(
        &mut self,
        _provider: &Provider,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        // Skeleton: state persistence is not yet implemented, so no progress is made.
        Ok(ExecOutput { checkpoint: input.checkpoint(), done: true })
    }

    fn unwind(
        &mut self,
        _provider: &Provider,
        input: UnwindInput,
    ) -> Result<UnwindOutput, StageError> {
        Ok(UnwindOutput { checkpoint: StageCheckpoint::new(input.unwind_to) })
    }
}
