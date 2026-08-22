//! Test [`SnapClient`] implementation.

use crate::{
    download::DownloadClient,
    error::{PeerRequestResult, RequestError},
    priority::Priority,
    snap::client::{SnapClient, SnapRequestOptions, SnapResponse},
};
use futures::future::{ready, Ready};
use reth_eth_wire_types::snap::SnapProtocolMessage;
use reth_network_peers::PeerId;
use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard},
};

/// A [`SnapClient`] that answers from a scripted queue of responses.
///
/// Every request kind draws from the same queue, in request order. An exhausted queue answers
/// [`RequestError::UnsupportedCapability`], which is how the network layer reports that no
/// connected peer serves snap, so [`Self::unavailable`] is just an empty one.
#[derive(Debug)]
pub struct TestSnapClient {
    responses: Mutex<VecDeque<PeerRequestResult<SnapResponse>>>,
    reported: Mutex<Vec<PeerId>>,
    priorities: Mutex<Vec<Priority>>,
    connected_peers: usize,
}

impl TestSnapClient {
    /// Creates a client that answers with `responses`, in order, from one connected peer.
    pub fn new(responses: impl IntoIterator<Item = PeerRequestResult<SnapResponse>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            reported: Mutex::new(Vec::new()),
            priorities: Mutex::new(Vec::new()),
            connected_peers: 1,
        }
    }

    /// Creates a client standing in for a network with no snap peer connected, which fails every
    /// request outright rather than queueing it.
    pub fn unavailable() -> Self {
        Self { connected_peers: 0, ..Self::new([]) }
    }

    /// Sets how many peers the client reports as connected.
    pub const fn with_connected_peers(mut self, peers: usize) -> Self {
        self.connected_peers = peers;
        self
    }

    /// Returns the peers reported through [`DownloadClient::report_bad_message`], in order.
    pub fn reported(&self) -> MutexGuard<'_, Vec<PeerId>> {
        self.reported.lock().unwrap()
    }

    /// Returns the priority each request was issued at, in order.
    pub fn priorities(&self) -> MutexGuard<'_, Vec<Priority>> {
        self.priorities.lock().unwrap()
    }

    fn next(&self, priority: Priority) -> Ready<PeerRequestResult<SnapResponse>> {
        self.priorities.lock().unwrap().push(priority);
        ready(
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(RequestError::UnsupportedCapability)),
        )
    }
}

impl DownloadClient for TestSnapClient {
    fn report_bad_message(&self, peer_id: PeerId) {
        self.reported.lock().unwrap().push(peer_id);
    }

    fn num_connected_peers(&self) -> usize {
        self.connected_peers
    }
}

impl SnapClient for TestSnapClient {
    type Output = Ready<PeerRequestResult<SnapResponse>>;

    fn request_snap(
        &self,
        _request: SnapProtocolMessage,
        options: SnapRequestOptions,
    ) -> Self::Output {
        self.next(options.priority)
    }
}
