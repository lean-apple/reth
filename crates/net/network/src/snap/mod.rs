//! The `snap/2` sync protocol (EIP-8189).
//!
//! `snap/2` is negotiated alongside `eth` on the same `RLPx` connection and carried by a dedicated
//! [`EthSnapStream`](reth_eth_wire::EthSnapStream), surfaced to the session as
//! [`EthRlpxConnection::EthSnap`](crate::session::EthRlpxConnection). Requests are dispatched
//! through [`SnapClient`], which routes them to a snap-capable session via the
//! [`SessionManager`](crate::session::SessionManager); the session sends them on the stream and
//! correlates the response by `request_id`.

pub mod bal;

mod client;
pub use client::{SnapClient, SnapPeerRequest, SnapResponseFuture};

mod server;
pub(crate) use server::serve_snap_request;

pub mod sync;
pub mod verify;
