//! The retry-and-verify loop shared by the snap range downloaders.

use futures::FutureExt;
use reth_eth_wire_types::snap::{
    GetAccountRangeMessage, GetStorageRangesMessage, SnapProtocolMessage,
};
use reth_network_p2p::{
    error::RequestError,
    priority::Priority,
    snap::client::{SnapClient, SnapRequestOptions, SnapResponse},
};
use reth_network_peers::PeerId;
use reth_tasks::Runtime;
use std::{
    fmt,
    task::{ready, Context, Poll},
};
use tracing::debug;

/// How many times a request is reissued before its error reaches the caller.
pub(super) const MAX_RETRIES: u8 = 2;

/// Drives one snap request to a verified response.
///
/// Peer-controlled proof work runs on the blocking pool, and a response that fails verification
/// is attributed to its responder before the request is reissued at high priority — the network
/// layer then routes the retry elsewhere.
pub(super) struct VerifyingRequest<C: SnapClient, V: SnapVerifier> {
    client: C,
    runtime: Runtime,
    request: V::Request,
    verifier: V,
    fut: C::Output,
    verification: Option<VerificationTask<V::Output>>,
    excluded_peers: Vec<PeerId>,
    rejected_response: bool,
    retries: u8,
}

impl<C, V> VerifyingRequest<C, V>
where
    C: SnapClient + Unpin + 'static,
    V: SnapVerifier,
{
    /// Submits `request` at normal priority and verifies its response with `verifier`.
    pub(super) fn new(
        client: C,
        request: V::Request,
        verifier: V,
        runtime: Runtime,
        excluded_peers: Vec<PeerId>,
    ) -> Self {
        let fut =
            request.send(&client, SnapRequestOptions::default().excluding(excluded_peers.clone()));
        Self {
            client,
            runtime,
            request,
            verifier,
            fut,
            verification: None,
            excluded_peers,
            rejected_response: false,
            retries: 0,
        }
    }

    /// Polls until the request yields a verified response or runs out of retries.
    pub(super) fn poll_verified(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<V::Output, RequestError>> {
        loop {
            if self.verification.is_some() {
                match ready!(self.poll_verification(cx)) {
                    Ok(Some(output)) => return Poll::Ready(Ok(output)),
                    Ok(None) => {}
                    Err(error) => return Poll::Ready(Err(error)),
                }
            }

            match ready!(self.fut.poll_unpin(cx)) {
                Ok(response) => {
                    let (peer_id, response) = response.split();
                    let verifier = self.verifier.clone();
                    let fut =
                        self.runtime.spawn_blocking(move || verifier.verify(peer_id, response));
                    self.verification = Some(VerificationTask { peer_id, fut });
                }
                // A peer that answered badly was already reported by the verifier.
                Err(error) if error.is_retryable() || error == RequestError::BadResponse => {
                    debug!(target: "downloaders::snap", %error, "Snap request failed, retrying");
                    if !self.retry() {
                        return Poll::Ready(Err(error))
                    }
                }
                Err(RequestError::NoEligiblePeers) if self.rejected_response => {
                    return Poll::Ready(Err(RequestError::BadResponse))
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }

    /// Reissues at high priority so a retry is not queued behind newly issued work.
    fn retry(&mut self) -> bool {
        if self.retries >= MAX_RETRIES {
            return false
        }
        self.retries += 1;
        self.fut = self.request.send(
            &self.client,
            SnapRequestOptions::new(Priority::High).excluding(self.excluded_peers.clone()),
        );
        true
    }

    /// Resolves the blocking verification, keeping the responder attributable until it finishes.
    fn poll_verification(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<V::Output>, RequestError>> {
        let verification = self.verification.as_mut().expect("verification task is present");
        let result = ready!(verification.fut.poll_unpin(cx));
        let peer_id = verification.peer_id;
        self.verification = None;

        match result {
            Ok(Ok(output)) => Poll::Ready(Ok(Some(output))),
            Ok(Err(error)) => {
                debug!(target: "downloaders::snap", ?peer_id, %error, "Invalid snap response");
                self.client.report_bad_message(peer_id);
                if !self.excluded_peers.contains(&peer_id) {
                    self.excluded_peers.push(peer_id);
                }
                self.rejected_response = true;
                Poll::Ready(self.retry().then_some(None).ok_or(error))
            }
            // The task panicked or the runtime is shutting down. Neither is the peer's doing, so
            // this must not read as a peer or session failure to callers that branch on it.
            Err(error) => {
                debug!(target: "downloaders::snap", %error, "Snap verification task failed");
                Poll::Ready(Err(RequestError::Internal))
            }
        }
    }
}

// `C::Output` is an opaque future, so it is described rather than printed.
impl<C, V> fmt::Debug for VerifyingRequest<C, V>
where
    C: SnapClient,
    V: SnapVerifier + fmt::Debug,
    V::Request: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifyingRequest")
            .field("client", &self.client)
            .field("request", &self.request)
            .field("verifier", &self.verifier)
            .field("verifying", &self.verification.is_some())
            .field("excluded_peers", &self.excluded_peers)
            .field("retries", &self.retries)
            .finish_non_exhaustive()
    }
}

/// A snap request message that can be reissued at a chosen priority.
pub(super) trait SnapRequest: Clone + Send + 'static {
    /// Sends this request through `client`.
    fn send<C: SnapClient>(&self, client: &C, options: SnapRequestOptions) -> C::Output;
}

impl SnapRequest for GetAccountRangeMessage {
    fn send<C: SnapClient>(&self, client: &C, options: SnapRequestOptions) -> C::Output {
        client.request_snap(SnapProtocolMessage::GetAccountRange(self.clone()), options)
    }
}

impl SnapRequest for GetStorageRangesMessage {
    fn send<C: SnapClient>(&self, client: &C, options: SnapRequestOptions) -> C::Output {
        client.request_snap(SnapProtocolMessage::GetStorageRanges(self.clone()), options)
    }
}

/// Authenticates a snap response against what the request asked for.
///
/// Runs on the blocking pool, so this owns everything it needs rather than borrowing it.
pub(super) trait SnapVerifier: Clone + Send + 'static {
    /// The request whose responses this authenticates.
    type Request: SnapRequest;
    /// What a verified response yields.
    type Output: Send + 'static;

    /// Returns the verified response, or an error the responder is held to account for.
    fn verify(self, peer_id: PeerId, response: SnapResponse) -> Result<Self::Output, RequestError>;
}

// Couples blocking proof work with its responder so failures remain attributable.
#[derive(Debug)]
struct VerificationTask<O> {
    peer_id: PeerId,
    fut: tokio::task::JoinHandle<Result<O, RequestError>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::poll_fn;
    use reth_eth_wire_types::snap::{AccountRangeMessage, GetAccountRangeMessage};
    use reth_network_p2p::test_utils::TestSnapClient;
    use reth_network_peers::WithPeerId;
    use std::sync::Arc;

    #[derive(Clone, Debug)]
    struct PanickingVerifier;

    impl SnapVerifier for PanickingVerifier {
        type Request = GetAccountRangeMessage;
        type Output = ();

        fn verify(
            self,
            _peer_id: PeerId,
            _response: SnapResponse,
        ) -> Result<Self::Output, RequestError> {
            panic!("local verifier panic")
        }
    }

    #[tokio::test]
    async fn verifier_panic_is_internal_and_does_not_penalize_peer() {
        let peer = PeerId::random();
        let response = SnapResponse::AccountRange(AccountRangeMessage {
            request_id: 1,
            accounts: Vec::new(),
            proof: Vec::new(),
        });
        let client = Arc::new(TestSnapClient::new([Ok(WithPeerId::new(peer, response))]));
        let request = GetAccountRangeMessage {
            request_id: 1,
            root_hash: Default::default(),
            starting_hash: Default::default(),
            limit_hash: Default::default(),
            response_bytes: 0,
        };
        let mut verifying = VerifyingRequest::new(
            Arc::clone(&client),
            request,
            PanickingVerifier,
            Runtime::test(),
            Vec::new(),
        );

        let error = poll_fn(|cx| verifying.poll_verified(cx)).await.unwrap_err();

        assert_eq!(error, RequestError::Internal);
        assert!(client.reported().is_empty());
    }
}
