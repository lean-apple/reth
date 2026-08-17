//! Snap/2 backfill orchestration.

use reth_engine_tree::backfill::{BackfillAction, BackfillEvent, BackfillSync, PipelineSync};
use reth_network_p2p::snap::client::SnapClient;
use reth_provider::{providers::ProviderNodeTypes, BalStoreHandle, ProviderFactory};
use reth_snap_sync::{ProviderChain, SessionRunOutcome, SnapSyncError, SnapSyncSession};
use reth_stages::{ControlFlow, Pipeline, PipelineError, PipelineTarget, StageError};
use reth_tasks::Runtime;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{oneshot, watch, Notify};

/// How long a session waits before retrying once no connected peer advertises `snap/2`.
///
/// Peers arrive from discovery rather than from anything the session does, so this only paces
/// the retry; nothing already downloaded is lost while it waits.
const SNAP_PEER_WAIT: Duration = Duration::from_secs(1);

/// Adds an optional snap bootstrap before regular pipeline backfill.
#[derive(Debug)]
pub(crate) struct SnapBootstrapSync<N: ProviderNodeTypes, C> {
    runtime: Runtime,
    /// Header-only pipeline used to establish and advance the snap target.
    headers: Option<PipelineSync<N>>,
    /// Standard pipeline backfill resumed after snap state is accepted.
    fallback: PipelineSync<N>,
    client: C,
    factory: ProviderFactory<N>,
    bal_store: BalStoreHandle,
    /// Target associated with the currently running header pipeline.
    header_target: Option<PipelineTarget>,
    /// Active snap state download and its head-update channel.
    snap: Option<SnapTask>,
    /// Whether actions should be delegated to the standard pipeline.
    bootstrapped: bool,
}

impl<N: ProviderNodeTypes, C> SnapBootstrapSync<N, C> {
    pub(crate) fn new(
        header_pipeline: Option<Pipeline<N>>,
        fallback: PipelineSync<N>,
        client: C,
        factory: ProviderFactory<N>,
        bal_store: BalStoreHandle,
        runtime: Runtime,
    ) -> Self {
        let bootstrapped = header_pipeline.is_none();
        Self {
            headers: header_pipeline.map(|pipeline| PipelineSync::new(pipeline, runtime.clone())),
            runtime,
            fallback,
            client,
            factory,
            bal_store,
            header_target: None,
            snap: None,
            bootstrapped,
        }
    }

    fn spawn_snap(&mut self, target: PipelineTarget) -> Result<(), PipelineError>
    where
        C: SnapClient + Clone + Unpin + 'static,
    {
        let hash = target.sync_target().ok_or_else(|| fatal("snap sync cannot unwind"))?;
        let chain = Arc::new(
            ProviderChain::new(self.factory.clone(), hash)
                .map_err(|error| PipelineError::Stage(StageError::Fatal(Box::new(error))))?,
        );
        let client = self.client.clone();
        let factory = self.factory.clone();
        let bal_store = self.bal_store.clone();
        let runtime = self.runtime.clone();
        let (tx, rx) = oneshot::channel();
        let (head_tx, head_rx) = watch::channel(None);

        self.runtime.spawn_critical_blocking_task("snap state sync", async move {
            let result =
                run_snap_session(client, factory, bal_store, chain, runtime, head_rx).await;
            let _ = tx.send(result);
        });
        self.snap = Some(SnapTask { result: rx, head: head_tx });
        Ok(())
    }

    fn on_header_event(&mut self, event: BackfillEvent) -> Option<BackfillEvent>
    where
        C: SnapClient + Clone + Unpin + 'static,
    {
        match event {
            BackfillEvent::Started(target) => {
                self.header_target = Some(target);
                self.snap.is_none().then_some(BackfillEvent::Started(target))
            }
            BackfillEvent::Finished(Ok(ControlFlow::Continue { .. })) => {
                let Some(target) = self.header_target.take() else {
                    return Some(BackfillEvent::TaskDropped(
                        "snap header pipeline completed without a target".into(),
                    ))
                };
                if let Some(snap) = &self.snap {
                    let Some(hash) = target.sync_target() else {
                        return Some(BackfillEvent::Finished(Err(fatal(
                            "snap head update cannot unwind",
                        ))))
                    };
                    let _ = snap.head.send(Some(hash));
                    None
                } else {
                    self.spawn_snap(target).err().map(|err| BackfillEvent::Finished(Err(err)))
                }
            }
            BackfillEvent::Finished(result) => {
                self.header_target = None;
                Some(BackfillEvent::Finished(result))
            }
            event @ BackfillEvent::TaskDropped(_) => Some(event),
        }
    }

    fn poll_snap(&mut self, cx: &mut Context<'_>) -> Poll<BackfillEvent> {
        let Some(snap) = &mut self.snap else { return Poll::Pending };
        let Poll::Ready(response) = Pin::new(&mut snap.result).poll(cx) else {
            return Poll::Pending
        };
        self.snap = None;

        Poll::Ready(match response {
            Ok(result) => {
                if matches!(result, Ok(ControlFlow::Continue { .. })) {
                    self.bootstrapped = true;
                }
                BackfillEvent::Finished(result)
            }
            Err(err) => BackfillEvent::TaskDropped(err.to_string()),
        })
    }
}

impl<N, C> BackfillSync for SnapBootstrapSync<N, C>
where
    N: ProviderNodeTypes,
    C: SnapClient + Clone + Unpin + 'static,
{
    fn on_action(&mut self, action: BackfillAction) {
        if self.bootstrapped {
            self.fallback.on_action(action);
            return
        }
        self.headers.as_mut().expect("snap headers exist before bootstrap").on_action(action);
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<BackfillEvent> {
        if self.bootstrapped {
            return self.fallback.poll(cx)
        }
        let headers = self.headers.as_mut().expect("snap headers exist before bootstrap");
        if let Poll::Ready(event) = headers.poll(cx) &&
            let Some(event) = self.on_header_event(event)
        {
            return Poll::Ready(event)
        }
        self.poll_snap(cx)
    }
}

/// What decides whether this database should use snap for its next backfill.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SnapBootstrapConditions {
    /// Whether `--snap` was passed.
    pub(crate) enabled: bool,
    /// Snap assembles Ethereum state only.
    pub(crate) is_optimism: bool,
    /// Snap responses are hashed with no preimages, so only the v2 layout can hold them.
    pub(crate) uses_hashed_state: bool,
    /// Height the pipeline has finished.
    pub(crate) finish: u64,
    /// Height the chain starts at.
    pub(crate) genesis: u64,
    /// Whether a previous generation was left unverified on disk.
    pub(crate) interrupted: bool,
}

impl SnapBootstrapConditions {
    /// Returns whether this database should use snap for its next backfill.
    ///
    /// Only a database still at genesis, or one already part-way through a generation, qualifies:
    /// a node that has executed blocks would have its state wiped for nothing.
    pub(crate) const fn met(self) -> bool {
        self.enabled &&
            !self.is_optimism &&
            self.uses_hashed_state &&
            (self.finish <= self.genesis || self.interrupted)
    }
}

async fn run_snap_session<N, C>(
    client: C,
    factory: ProviderFactory<N>,
    bal_store: BalStoreHandle,
    chain: Arc<ProviderChain<ProviderFactory<N>>>,
    runtime: Runtime,
    mut head: watch::Receiver<Option<alloy_primitives::B256>>,
) -> Result<ControlFlow, PipelineError>
where
    N: ProviderNodeTypes,
    C: SnapClient + Clone + Unpin + 'static,
{
    let head_updated = Arc::new(Notify::new());
    let session_head_updated = Arc::clone(&head_updated);
    let session_chain = Arc::clone(&chain);
    let session = async move {
        let mut session = SnapSyncSession::new(client, factory, session_chain, bal_store, runtime);

        loop {
            match session.run_until_blocked().await.map_err(snap_error)? {
                SessionRunOutcome::Verified(at) => {
                    session.accept().map_err(snap_error)?;
                    return Ok(ControlFlow::Continue { block_number: at.number })
                }
                SessionRunOutcome::WaitingForPeers => {
                    tokio::time::sleep(SNAP_PEER_WAIT).await;
                }
                SessionRunOutcome::WaitingForTarget => session_head_updated.notified().await,
            }
        }
    };
    tokio::pin!(session);

    loop {
        tokio::select! {
            result = &mut session => return result,
            changed = head.changed() => {
                changed.map_err(|_| fatal("snap controller stopped"))?;
                if let Some(hash) = *head.borrow_and_update() {
                    chain
                        .update_head(hash)
                        .map_err(|error| PipelineError::Stage(StageError::Fatal(Box::new(error))))?;
                    head_updated.notify_one();
                }
            }
        }
    }
}

fn snap_error(error: SnapSyncError) -> PipelineError {
    PipelineError::Stage(StageError::Fatal(Box::new(error)))
}

fn fatal(message: &'static str) -> PipelineError {
    PipelineError::Stage(StageError::Fatal(message.into()))
}

/// Handle for the active snap task and its rolling canonical head.
#[derive(Debug)]
struct SnapTask {
    result: oneshot::Receiver<Result<ControlFlow, PipelineError>>,
    head: watch::Sender<Option<alloy_primitives::B256>>,
}

#[cfg(test)]
mod tests {
    use super::SnapBootstrapConditions;

    /// A fresh Ethereum v2 database with snap enabled, which every case below varies from.
    const FRESH: SnapBootstrapConditions = SnapBootstrapConditions {
        enabled: true,
        is_optimism: false,
        uses_hashed_state: true,
        finish: 0,
        genesis: 0,
        interrupted: false,
    };

    #[test]
    fn snap_bootstrap_is_limited_to_fresh_or_interrupted_hashed_state_databases() {
        assert!(FRESH.met());
        assert!(SnapBootstrapConditions { finish: 100, interrupted: true, ..FRESH }.met());
        assert!(!SnapBootstrapConditions { finish: 100, ..FRESH }.met());
        assert!(!SnapBootstrapConditions { uses_hashed_state: false, ..FRESH }.met());
        assert!(!SnapBootstrapConditions { is_optimism: true, ..FRESH }.met());
        assert!(!SnapBootstrapConditions { enabled: false, ..FRESH }.met());
    }
}
