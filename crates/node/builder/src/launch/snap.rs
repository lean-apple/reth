//! Snap/2 backfill orchestration.

use reth_engine_tree::backfill::{BackfillAction, BackfillEvent, BackfillSync};
use reth_network_p2p::snap::client::SnapClient;
use reth_provider::{providers::ProviderNodeTypes, BalStoreHandle, ProviderFactory};
use reth_snap_sync::{ProviderChain, SessionRunOutcome, SnapSyncError, SnapSyncSession};
use reth_stages::{
    ControlFlow, Pipeline, PipelineError, PipelineTarget, PipelineWithResult, StageError,
};
use reth_tasks::Runtime;
use std::{
    pin::Pin,
    task::{ready, Context, Poll},
    time::Duration,
};
use tokio::sync::oneshot;

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
    client: C,
    factory: ProviderFactory<N>,
    bal_store: BalStoreHandle,
    pending_target: Option<PipelineTarget>,
    state: SnapBackfillState<N>,
}

impl<N: ProviderNodeTypes, C> SnapPipelineSync<N, C> {
    pub(crate) fn new(
        header_pipeline: Pipeline<N>,
        client: C,
        factory: ProviderFactory<N>,
        bal_store: BalStoreHandle,
        runtime: Runtime,
    ) -> Self {
        Self {
            runtime,
            header_pipeline: Some(Box::new(header_pipeline)),
            client,
            factory,
            bal_store,
            pending_target: None,
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
            Ok(_) => {
                self.spawn_snap(target);
                self.poll_snap(cx)
            }
        }
    }

    fn spawn_snap(&mut self, target: PipelineTarget)
    where
        C: SnapClient + Clone + 'static,
    {
        let client = self.client.clone();
        let factory = self.factory.clone();
        let bal_store = self.bal_store.clone();
        let (tx, rx) = oneshot::channel();

        self.runtime.spawn_critical_blocking_task("snap state sync", async move {
            let result = run_snap_session(target, client, factory, bal_store).await;
            let _ = tx.send(result);
        });
        self.state = SnapBackfillState::Snap(rx);
    }

    fn poll_snap(&mut self, cx: &mut Context<'_>) -> Poll<BackfillEvent> {
        let SnapBackfillState::Snap(result) = &mut self.state else { return Poll::Pending };
        let response = ready!(Pin::new(result).poll(cx));
        self.state = SnapBackfillState::Idle;

        Poll::Ready(match response {
            Ok(result) => BackfillEvent::Finished(result),
            Err(err) => BackfillEvent::TaskDropped(err.to_string()),
        })
    }
}

impl<N, C> BackfillSync for SnapPipelineSync<N, C>
where
    N: ProviderNodeTypes,
    C: SnapClient + Clone + 'static,
{
    fn on_action(&mut self, action: BackfillAction) {
        match action {
            BackfillAction::Start(target) => self.set_target(target),
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<BackfillEvent> {
        if let Some(event) = self.try_spawn_headers() {
            return Poll::Ready(event)
        }
        if matches!(self.state, SnapBackfillState::Headers { .. }) {
            return self.poll_headers(cx)
        }
        if matches!(self.state, SnapBackfillState::Snap(_)) {
            return self.poll_snap(cx)
        }
        Poll::Pending
    }
}

async fn run_snap_session<N, C>(
    target: PipelineTarget,
    client: C,
    factory: ProviderFactory<N>,
    bal_store: BalStoreHandle,
) -> Result<ControlFlow, PipelineError>
where
    N: ProviderNodeTypes,
    C: SnapClient + 'static,
{
    let hash = target.sync_target().ok_or_else(|| fatal("snap sync cannot unwind"))?;
    let chain = ProviderChain::new(factory.clone(), hash)
        .map_err(|error| PipelineError::Stage(StageError::Fatal(Box::new(error))))?;
    let mut session = SnapSyncSession::new(client, factory, chain, bal_store);

    loop {
        match session.run_until_blocked().await.map_err(snap_error)? {
            SessionRunOutcome::Verified(at) => {
                session.accept().map_err(snap_error)?;
                return Ok(ControlFlow::Continue { block_number: at.number })
            }
            SessionRunOutcome::WaitingForPeers | SessionRunOutcome::WaitingForTarget => {
                tokio::time::sleep(Duration::from_secs(1)).await;
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
    Headers { target: PipelineTarget, result: oneshot::Receiver<PipelineWithResult<N>> },
    Snap(oneshot::Receiver<Result<ControlFlow, PipelineError>>),
}

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
