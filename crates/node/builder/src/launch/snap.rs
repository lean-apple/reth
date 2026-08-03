//! Snap/2 backfill orchestration.

use reth_engine_tree::backfill::{BackfillAction, BackfillEvent, BackfillSync, PipelineSync};
use reth_network_p2p::snap::client::SnapClient;
use reth_provider::{providers::ProviderNodeTypes, BalStoreHandle, ProviderFactory};
use reth_snap_sync::{ProviderChain, SessionRunOutcome, SnapSyncError, SnapSyncSession};
use reth_stages::{
    ControlFlow, Pipeline, PipelineError, PipelineTarget, PipelineWithResult, StageError,
};
use reth_tasks::Runtime;
use std::{
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};

/// Returns whether this database should use snap for its next backfill.
pub(crate) const fn should_snap_bootstrap(
    enabled: bool,
    is_optimism: bool,
    uses_hashed_state: bool,
    finish: u64,
    genesis: u64,
    interrupted: bool,
) -> bool {
    enabled && !is_optimism && uses_hashed_state && (finish <= genesis || interrupted)
}

/// Backfill controller that persists headers before downloading snap state.
#[derive(Debug)]
pub(crate) struct SnapPipelineSync<N: ProviderNodeTypes, C> {
    runtime: Runtime,
    header_pipeline: Option<Box<Pipeline<N>>>,
    fallback: PipelineSync<N>,
    client: C,
    factory: ProviderFactory<N>,
    bal_store: BalStoreHandle,
    pending_target: Option<PipelineTarget>,
    bootstrapped: bool,
    state: SnapBackfillState<N>,
}

impl<N: ProviderNodeTypes, C> SnapPipelineSync<N, C> {
    pub(crate) fn new(
        header_pipeline: Pipeline<N>,
        fallback: PipelineSync<N>,
        client: C,
        factory: ProviderFactory<N>,
        bal_store: BalStoreHandle,
        runtime: Runtime,
    ) -> Self {
        Self {
            runtime,
            header_pipeline: Some(Box::new(header_pipeline)),
            fallback,
            client,
            factory,
            bal_store,
            pending_target: None,
            bootstrapped: false,
            state: SnapBackfillState::Idle,
        }
    }

    fn set_target(&mut self, target: PipelineTarget) {
        if target.sync_target().is_some_and(|hash| hash.is_zero()) {
            return
        }
        self.pending_target = Some(target);
    }

    fn try_spawn_headers(&mut self) -> Option<BackfillEvent> {
        if !matches!(self.state, SnapBackfillState::Idle) {
            return None
        }
        let target = self.pending_target.take()?;
        let pipeline = self.header_pipeline.take().expect("header pipeline exists while idle");
        let (tx, rx) = oneshot::channel();

        self.runtime.spawn_critical_blocking_task("snap header pipeline", async move {
            let result = pipeline.run_as_fut(Some(target)).await;
            let _ = tx.send(result);
        });
        self.state = SnapBackfillState::Headers { target, result: rx };
        Some(BackfillEvent::Started(target))
    }

    fn poll_headers(&mut self, cx: &mut Context<'_>) -> Poll<BackfillEvent>
    where
        C: SnapClient + Clone + 'static,
    {
        let SnapBackfillState::Headers { target, result } = &mut self.state else {
            return Poll::Pending
        };
        let target = *target;
        let response = ready!(Pin::new(result).poll(cx));

        let (pipeline, result) = match response {
            Ok(response) => response,
            Err(err) => {
                self.state = SnapBackfillState::Idle;
                return Poll::Ready(BackfillEvent::TaskDropped(err.to_string()))
            }
        };
        self.header_pipeline = Some(Box::new(pipeline));

        match result {
            Ok(ControlFlow::Unwind { target, bad_block }) => {
                self.state = SnapBackfillState::Idle;
                Poll::Ready(BackfillEvent::Finished(Ok(ControlFlow::Unwind { target, bad_block })))
            }
            Err(err) => {
                self.state = SnapBackfillState::Idle;
                Poll::Ready(BackfillEvent::Finished(Err(err)))
            }
            Ok(_) => match self.spawn_snap(target) {
                Ok(()) => self.poll_snap(cx),
                Err(err) => {
                    self.state = SnapBackfillState::Idle;
                    Poll::Ready(BackfillEvent::Finished(Err(err)))
                }
            },
        }
    }

    fn spawn_snap(&mut self, target: PipelineTarget) -> Result<(), PipelineError>
    where
        C: SnapClient + Clone + 'static,
    {
        let hash = target.sync_target().ok_or_else(|| fatal("snap sync cannot unwind"))?;
        let chain = Arc::new(
            ProviderChain::new(self.factory.clone(), hash)
                .map_err(|error| PipelineError::Stage(StageError::Fatal(Box::new(error))))?,
        );
        let client = self.client.clone();
        let factory = self.factory.clone();
        let bal_store = self.bal_store.clone();
        let (tx, rx) = oneshot::channel();
        let (waiting_tx, waiting_rx) = mpsc::unbounded_channel();
        let (target_tx, target_rx) = watch::channel(None);
        let session_chain = Arc::clone(&chain);

        self.runtime.spawn_critical_blocking_task("snap state sync", async move {
            let result =
                run_snap_session(client, factory, bal_store, session_chain, waiting_tx, target_rx)
                    .await;
            let _ = tx.send(result);
        });
        self.state = SnapBackfillState::Snap {
            result: rx,
            chain,
            waiting: waiting_rx,
            target: target_tx,
            waiting_for_target: false,
            header_update: None,
        };
        Ok(())
    }

    fn poll_snap(&mut self, cx: &mut Context<'_>) -> Poll<BackfillEvent> {
        loop {
            let SnapBackfillState::Snap { result, .. } = &mut self.state else {
                return Poll::Pending
            };
            if let Poll::Ready(response) = Pin::new(result).poll(cx) {
                self.state = SnapBackfillState::Idle;
                return Poll::Ready(match response {
                    Ok(result) => {
                        if matches!(result, Ok(ControlFlow::Continue { .. })) {
                            self.bootstrapped = true;
                            if let Some(target) = self.pending_target.take() {
                                self.fallback.on_action(BackfillAction::Start(target));
                            }
                        }
                        BackfillEvent::Finished(result)
                    }
                    Err(err) => BackfillEvent::TaskDropped(err.to_string()),
                })
            }

            if let Some(response) = self.poll_header_update(cx) {
                match response {
                    Ok((pipeline, result, target)) => {
                        self.header_pipeline = Some(Box::new(pipeline));
                        match result {
                            Ok(ControlFlow::Unwind { target, bad_block }) => {
                                self.state = SnapBackfillState::Idle;
                                return Poll::Ready(BackfillEvent::Finished(Ok(
                                    ControlFlow::Unwind { target, bad_block },
                                )))
                            }
                            Err(err) => {
                                self.state = SnapBackfillState::Idle;
                                return Poll::Ready(BackfillEvent::Finished(Err(err)))
                            }
                            Ok(_) => {
                                let hash = target
                                    .sync_target()
                                    .expect("head update target cannot be unwind");
                                let SnapBackfillState::Snap {
                                    chain,
                                    target,
                                    waiting_for_target,
                                    ..
                                } = &mut self.state
                                else {
                                    unreachable!()
                                };
                                if let Err(err) = chain.update_head(hash) {
                                    self.state = SnapBackfillState::Idle;
                                    return Poll::Ready(BackfillEvent::Finished(Err(
                                        PipelineError::Stage(StageError::Fatal(Box::new(err))),
                                    )))
                                }
                                *waiting_for_target = false;
                                let _ = target.send(Some(hash));
                            }
                        }
                    }
                    Err(err) => {
                        self.state = SnapBackfillState::Idle;
                        return Poll::Ready(BackfillEvent::TaskDropped(err.to_string()))
                    }
                }
            }

            let SnapBackfillState::Snap { waiting, waiting_for_target, .. } = &mut self.state
            else {
                unreachable!()
            };
            if Pin::new(waiting).poll_recv(cx).is_ready() {
                *waiting_for_target = true;
            }

            if !self.try_spawn_head_update() {
                return Poll::Pending
            }
        }
    }

    fn try_spawn_head_update(&mut self) -> bool {
        if !matches!(
            self.state,
            SnapBackfillState::Snap { waiting_for_target: true, header_update: None, .. }
        ) {
            return false
        }
        let Some(target) = self.pending_target.take() else { return false };
        let pipeline = self.header_pipeline.take().expect("header pipeline is not already running");
        let (tx, rx) = oneshot::channel();

        self.runtime.spawn_critical_blocking_task("snap header update", async move {
            let (pipeline, result) = pipeline.run_as_fut(Some(target)).await;
            let _ = tx.send((pipeline, result, target));
        });
        let SnapBackfillState::Snap { header_update, .. } = &mut self.state else { unreachable!() };
        *header_update = Some(rx);
        true
    }

    fn poll_header_update(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Option<Result<HeaderUpdateResult<N>, oneshot::error::RecvError>> {
        let SnapBackfillState::Snap { header_update: Some(update), .. } = &mut self.state else {
            return None
        };
        let Poll::Ready(response) = Pin::new(update).poll(cx) else { return None };
        let SnapBackfillState::Snap { header_update, .. } = &mut self.state else { unreachable!() };
        *header_update = None;
        Some(response)
    }
}

impl<N, C> BackfillSync for SnapPipelineSync<N, C>
where
    N: ProviderNodeTypes,
    C: SnapClient + Clone + 'static,
{
    fn on_action(&mut self, action: BackfillAction) {
        if self.bootstrapped {
            self.fallback.on_action(action);
            return
        }
        match action {
            BackfillAction::Start(target) => self.set_target(target),
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<BackfillEvent> {
        if self.bootstrapped {
            return self.fallback.poll(cx)
        }
        if let Some(event) = self.try_spawn_headers() {
            return Poll::Ready(event)
        }
        if matches!(self.state, SnapBackfillState::Headers { .. }) {
            return self.poll_headers(cx)
        }
        if matches!(self.state, SnapBackfillState::Snap { .. }) {
            return self.poll_snap(cx)
        }
        Poll::Pending
    }
}

async fn run_snap_session<N, C>(
    client: C,
    factory: ProviderFactory<N>,
    bal_store: BalStoreHandle,
    chain: Arc<ProviderChain<ProviderFactory<N>>>,
    waiting: mpsc::UnboundedSender<()>,
    mut target: watch::Receiver<Option<alloy_primitives::B256>>,
) -> Result<ControlFlow, PipelineError>
where
    N: ProviderNodeTypes,
    C: SnapClient + 'static,
{
    let mut session = SnapSyncSession::new(client, factory, Arc::clone(&chain), bal_store);

    loop {
        match session.run_until_blocked().await.map_err(snap_error)? {
            SessionRunOutcome::Verified(at) => {
                session.accept().map_err(snap_error)?;
                return Ok(ControlFlow::Continue { block_number: at.number })
            }
            SessionRunOutcome::WaitingForPeers => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            SessionRunOutcome::WaitingForTarget => {
                waiting.send(()).map_err(|_| fatal("snap controller stopped"))?;
                target.changed().await.map_err(|_| fatal("snap controller stopped"))?;
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

#[derive(Debug)]
enum SnapBackfillState<N: ProviderNodeTypes> {
    Idle,
    Headers {
        target: PipelineTarget,
        result: oneshot::Receiver<PipelineWithResult<N>>,
    },
    Snap {
        result: oneshot::Receiver<Result<ControlFlow, PipelineError>>,
        chain: Arc<ProviderChain<ProviderFactory<N>>>,
        waiting: mpsc::UnboundedReceiver<()>,
        target: watch::Sender<Option<alloy_primitives::B256>>,
        waiting_for_target: bool,
        header_update: Option<oneshot::Receiver<HeaderUpdateResult<N>>>,
    },
}

type HeaderUpdateResult<N> = (Pipeline<N>, Result<ControlFlow, PipelineError>, PipelineTarget);

#[cfg(test)]
mod tests {
    use super::should_snap_bootstrap;

    #[test]
    fn snap_bootstrap_is_limited_to_fresh_or_interrupted_hashed_state_databases() {
        assert!(should_snap_bootstrap(true, false, true, 0, 0, false));
        assert!(should_snap_bootstrap(true, false, true, 100, 0, true));
        assert!(!should_snap_bootstrap(true, false, true, 100, 0, false));
        assert!(!should_snap_bootstrap(true, false, false, 0, 0, false));
        assert!(!should_snap_bootstrap(true, true, true, 0, 0, false));
        assert!(!should_snap_bootstrap(false, false, true, 0, 0, false));
    }
}
