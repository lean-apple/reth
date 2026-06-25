//! Serving inbound `snap/2` requests from local stores.
//!
//! Pure helpers called from the active session: given an inbound request and the session's stores,
//! they produce the response to send back. No connection/IO state lives here.

use crate::eth_requests::{MAX_BLOCK_ACCESS_LISTS_SERVE, SOFT_RESPONSE_LIMIT};
use reth_eth_wire_types::{
    snap::{BlockAccessListsMessage, GetBlockAccessListsMessage, SnapProtocolMessage},
    BlockAccessLists,
};
use reth_storage_api::{BalStoreHandle, GetBlockAccessListLimit};

/// Serves an inbound `snap/2` request, returning the response to send back, or `None` if the
/// request type is not served yet.
pub(crate) fn serve_snap_request(
    bal_store: &BalStoreHandle,
    request: SnapProtocolMessage,
) -> Option<SnapProtocolMessage> {
    match request {
        SnapProtocolMessage::GetBlockAccessLists(req) => {
            Some(SnapProtocolMessage::BlockAccessLists(serve_block_access_lists(bal_store, req)))
        }
        // TODO(snap/2 server): serve bulk-state ranges (GetAccountRange / GetStorageRanges /
        // GetByteCodes) once range iteration + boundary proofs land. Not served yet.
        _ => None,
    }
}

/// Serves a `GetBlockAccessLists` request from the BAL store.
///
/// Entries are returned in request order with unavailable BALs as empty (`None`) entries,
/// truncating from the tail once the soft byte limit is exceeded (EIP-8189). The response echoes
/// the peer's `request_id`.
fn serve_block_access_lists(
    bal_store: &BalStoreHandle,
    mut req: GetBlockAccessListsMessage,
) -> BlockAccessListsMessage {
    req.block_hashes.truncate(MAX_BLOCK_ACCESS_LISTS_SERVE);
    let soft_limit = (req.response_bytes as usize).min(SOFT_RESPONSE_LIMIT);
    let limit = GetBlockAccessListLimit::ResponseSizeSoftLimit(soft_limit);
    let lists =
        bal_store.get_by_hashes_with_limit(&req.block_hashes, limit).unwrap_or_else(|err| {
            // A backend failure must not look like a deliberate empty response; surface it for
            // observability and fall back to an empty list.
            tracing::debug!(target: "net::snap", %err, "failed to load block access lists");
            Vec::new()
        });
    BlockAccessListsMessage {
        request_id: req.request_id,
        block_access_lists: BlockAccessLists(lists),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use reth_eth_wire_types::snap::GetByteCodesMessage;

    #[test]
    fn serves_bals_in_request_order_with_missing_as_empty() {
        // The default store is a no-op: every hash resolves to a missing (None) entry.
        let store = BalStoreHandle::default();
        let hashes =
            vec![B256::with_last_byte(1), B256::with_last_byte(2), B256::with_last_byte(3)];

        let resp = serve_block_access_lists(
            &store,
            GetBlockAccessListsMessage {
                request_id: 42,
                block_hashes: hashes.clone(),
                response_bytes: u64::MAX,
            },
        );

        assert_eq!(resp.request_id, 42, "response echoes the peer's request id");
        assert_eq!(resp.block_access_lists.0.len(), hashes.len(), "one entry per requested hash");
        assert!(
            resp.block_access_lists.0.iter().all(Option::is_none),
            "missing BALs are empty entries"
        );
    }

    #[test]
    fn serves_bals_truncates_from_tail_on_soft_limit() {
        let store = BalStoreHandle::default();
        let resp = serve_block_access_lists(
            &store,
            GetBlockAccessListsMessage {
                request_id: 1,
                block_hashes: vec![
                    B256::with_last_byte(1),
                    B256::with_last_byte(2),
                    B256::with_last_byte(3),
                ],
                response_bytes: 1,
            },
        );
        // Soft 1-byte limit: each missing entry counts 1 byte, the entry that crosses the limit is
        // included, and the tail is dropped.
        assert_eq!(resp.block_access_lists.0.len(), 2);
    }

    #[test]
    fn serve_dispatches_block_access_lists() {
        let store = BalStoreHandle::default();
        let response = serve_snap_request(
            &store,
            SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                request_id: 7,
                block_hashes: vec![B256::with_last_byte(1)],
                response_bytes: u64::MAX,
            }),
        );
        assert!(matches!(
            response,
            Some(SnapProtocolMessage::BlockAccessLists(m)) if m.request_id == 7
        ));
    }

    #[test]
    fn does_not_serve_bulk_state_requests_yet() {
        let store = BalStoreHandle::default();
        let request = SnapProtocolMessage::GetByteCodes(GetByteCodesMessage {
            request_id: 1,
            hashes: Vec::new(),
            response_bytes: 0,
        });
        assert!(serve_snap_request(&store, request).is_none());
    }
}
