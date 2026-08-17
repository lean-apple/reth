//! Storage range downloads authenticated against account storage roots.

use crate::snap::request::{SnapVerifier, VerifyingRequest};
use alloy_primitives::{B256, U256};
use futures::Future;
use reth_eth_wire_types::snap::{GetStorageRangesMessage, StorageData, MAX_HASH};
use reth_network_p2p::{
    error::RequestError,
    snap::client::{SnapClient, SnapResponse},
};
use reth_network_peers::PeerId;
use reth_tasks::Runtime;
use reth_trie_common::{range_proof::verify_range_proof, TrieAccount};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// Downloads and verifies storage ranges for accounts authenticated by an account-range response.
///
/// Responses are positional: every returned slot list belongs to the corresponding requested
/// account. Complete lists are checked directly against their storage roots, while the optional
/// proof authenticates only the final, partial list.
#[derive(Debug)]
pub struct StorageRangeDownloader<C: SnapClient>(VerifyingRequest<C, StorageProofVerifier>);

impl<C> StorageRangeDownloader<C>
where
    C: SnapClient + Unpin + 'static,
{
    /// Validates the request against `accounts`, submits it at normal priority, and uses `runtime`
    /// for proof verification.
    pub fn new(
        client: C,
        request: GetStorageRangesMessage,
        accounts: &[(B256, TrieAccount)],
        runtime: Runtime,
    ) -> Result<Self, InvalidStorageRangeRequest> {
        Self::new_excluding(client, request, accounts, runtime, Vec::new())
    }

    /// Creates a downloader that will not select peers already tried for this logical range.
    pub fn new_excluding(
        client: C,
        request: GetStorageRangesMessage,
        accounts: &[(B256, TrieAccount)],
        runtime: Runtime,
        excluded_peers: Vec<PeerId>,
    ) -> Result<Self, InvalidStorageRangeRequest> {
        let origin = request.starting_hash.unwrap_or(B256::ZERO);
        let limit = request.limit_hash.unwrap_or(MAX_HASH);
        if origin > limit {
            return Err(InvalidStorageRangeRequest::ReversedBounds { origin, limit })
        }
        if request.account_hashes.is_empty() {
            return Err(InvalidStorageRangeRequest::NoAccounts)
        }
        if request.account_hashes.len() != accounts.len() {
            return Err(InvalidStorageRangeRequest::AccountCount {
                requested: request.account_hashes.len(),
                supplied: accounts.len(),
            })
        }

        let mut storage_roots = Vec::with_capacity(accounts.len());
        for (index, (requested, (supplied, account))) in
            request.account_hashes.iter().zip(accounts).enumerate()
        {
            if requested != supplied {
                return Err(InvalidStorageRangeRequest::AccountMismatch {
                    index,
                    requested: *requested,
                    supplied: *supplied,
                })
            }
            storage_roots.push(account.storage_root);
        }

        let verifier = StorageProofVerifier { request: request.clone(), storage_roots };
        Ok(Self(VerifyingRequest::new(client, request, verifier, runtime, excluded_peers)))
    }
}

impl<C> Future for StorageRangeDownloader<C>
where
    C: SnapClient + Unpin + 'static,
{
    type Output = Result<StorageRangeOutcome, RequestError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().0.poll_verified(cx)
    }
}

// Owns the request context moved to the blocking verifier.
#[derive(Clone, Debug)]
struct StorageProofVerifier {
    request: GetStorageRangesMessage,
    storage_roots: Vec<B256>,
}

impl SnapVerifier for StorageProofVerifier {
    type Request = GetStorageRangesMessage;
    type Output = StorageRangeOutcome;

    fn verify(
        self,
        peer_id: PeerId,
        response: SnapResponse,
    ) -> Result<StorageRangeOutcome, RequestError> {
        self.verify_response(peer_id, response)
    }
}

impl StorageProofVerifier {
    // Decodes and authenticates every positional range in a storage response.
    fn verify_response(
        &self,
        peer_id: PeerId,
        response: SnapResponse,
    ) -> Result<StorageRangeOutcome, RequestError> {
        let SnapResponse::StorageRanges(response) = response else {
            tracing::debug!(target: "downloaders::snap", "Expected storage ranges response");
            return Err(RequestError::BadResponse)
        };
        if response.request_id != self.request.request_id {
            tracing::debug!(
                target: "downloaders::snap",
                expected = self.request.request_id,
                got = response.request_id,
                "Storage ranges response id mismatch"
            );
            return Err(RequestError::BadResponse)
        }
        if response.slots.len() > self.request.account_hashes.len() {
            tracing::debug!(target: "downloaders::snap", "Storage response contains extra ranges");
            return Err(RequestError::BadResponse)
        }
        if response.slots.is_empty() {
            if !response.proof.is_empty() {
                tracing::debug!(
                    target: "downloaders::snap",
                    "Storage response has a proof without a range"
                );
                return Err(RequestError::BadResponse)
            }
            return Ok(StorageRangeOutcome::Unavailable { peer_id })
        }

        let proof_index = (!response.proof.is_empty()).then_some(response.slots.len() - 1);
        let request_origin = self.request.starting_hash.unwrap_or(B256::ZERO);
        let request_limit = self.request.limit_hash.unwrap_or(MAX_HASH);
        let mut ranges = Vec::with_capacity(response.slots.len());
        let mut final_next = None;

        for (index, slots) in response.slots.iter().enumerate() {
            let account_hash = self.request.account_hashes[index];
            let origin = if index == 0 { request_origin } else { B256::ZERO };
            let limit = if index == 0 { request_limit } else { MAX_HASH };

            if slots.iter().filter(|slot| slot.hash > limit).nth(1).is_some() {
                tracing::debug!(
                    target: "downloaders::snap",
                    %account_hash,
                    "Storage range runs past the requested limit"
                );
                return Err(RequestError::BadResponse)
            }

            let mut decoded = Self::decode_slots(account_hash, origin, slots)?;
            let leaves = decoded.iter().map(|(hash, value)| (*hash, alloy_rlp::encode(value)));
            let proof = if proof_index == Some(index) { response.proof.as_slice() } else { &[] };
            let next = verify_range_proof(self.storage_roots[index], origin, leaves, proof)
                .map_err(|error| {
                    tracing::debug!(
                        target: "downloaders::snap",
                        %account_hash,
                        %error,
                        "Invalid storage range proof"
                    );
                    RequestError::BadResponse
                })?;

            // As with account ranges, a responder may append one boundary slot past the limit.
            // It participates in proof verification but is not part of the requested interval.
            decoded.truncate(decoded.partition_point(|(hash, _)| *hash <= limit));
            ranges.push(VerifiedStorageRange { account_hash, slots: decoded });
            final_next = next.filter(|next| *next <= limit);
        }

        let continuation = final_next
            .map(|starting_hash| StorageRangeContinuation::Partial {
                account_index: ranges.len() - 1,
                account_hash: ranges.last().expect("a response range exists").account_hash,
                starting_hash,
            })
            .or_else(|| {
                let account_index = ranges.len();
                self.request.account_hashes.get(account_index).copied().map(|account_hash| {
                    StorageRangeContinuation::NextAccount { account_index, account_hash }
                })
            });

        Ok(StorageRangeOutcome::Verified(VerifiedStorageRanges { ranges, continuation }))
    }

    // Validates slot order and decodes the RLP storage values for one account.
    fn decode_slots(
        account_hash: B256,
        origin: B256,
        slots: &[StorageData],
    ) -> Result<Vec<(B256, U256)>, RequestError> {
        let mut decoded = Vec::with_capacity(slots.len());
        let mut previous = None;

        for slot in slots {
            if slot.hash < origin || previous.is_some_and(|previous| slot.hash <= previous) {
                tracing::debug!(
                    target: "downloaders::snap",
                    %account_hash,
                    "Storage slots are outside the origin or not strictly ordered"
                );
                return Err(RequestError::BadResponse)
            }
            let value = slot.value().map_err(|error| {
                tracing::debug!(
                    target: "downloaders::snap",
                    %account_hash,
                    %error,
                    "Invalid storage slot value"
                );
                RequestError::BadResponse
            })?;
            if value.is_zero() {
                tracing::debug!(
                    target: "downloaders::snap",
                    %account_hash,
                    "Storage range contains a zero-valued trie leaf"
                );
                return Err(RequestError::BadResponse)
            }
            previous = Some(slot.hash);
            decoded.push((slot.hash, value));
        }
        Ok(decoded)
    }
}

/// The result of an authenticated storage-ranges request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageRangeOutcome {
    /// The selected peer does not have the requested state or one of the requested accounts.
    Unavailable {
        /// Peer that answered without the requested state.
        peer_id: PeerId,
    },
    /// Storage ranges authenticated against their accounts' storage roots.
    Verified(VerifiedStorageRanges),
}

/// Positional storage ranges authenticated against their accounts' storage roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedStorageRanges {
    /// Returned ranges in the same order as the request's account hashes.
    pub ranges: Vec<VerifiedStorageRange>,
    /// Where the next request must resume, if this response did not finish the request.
    pub continuation: Option<StorageRangeContinuation>,
}

/// Decoded storage slots authenticated against one account's storage root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedStorageRange {
    /// Hashed account address owning this storage trie.
    pub account_hash: B256,
    /// Non-zero slots in strictly increasing hashed-key order.
    pub slots: Vec<(B256, U256)>,
}

/// Position from which a subsequent storage-ranges request must resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageRangeContinuation {
    /// The final returned account was only partially covered.
    Partial {
        /// Index in the original request's account list.
        account_index: usize,
        /// Hashed account address at `account_index`.
        account_hash: B256,
        /// Inclusive storage-key origin for the next request.
        starting_hash: B256,
    },
    /// The returned accounts are complete, but a later requested account was not served.
    NextAccount {
        /// Index in the original request's account list.
        account_index: usize,
        /// Hashed account address at `account_index`.
        account_hash: B256,
    },
}

/// Error returned when a storage-ranges request does not match its authenticated accounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidStorageRangeRequest {
    /// The request contains no account hashes.
    #[error("storage range request contains no accounts")]
    NoAccounts,
    /// The inclusive storage bounds are reversed.
    #[error("storage range origin {origin} exceeds limit {limit}")]
    ReversedBounds {
        /// Inclusive origin requested for the first account.
        origin: B256,
        /// Inclusive limit requested for the first account.
        limit: B256,
    },
    /// The request and authenticated account batch have different lengths.
    #[error("storage range request has {requested} accounts but {supplied} were supplied")]
    AccountCount {
        /// Number of account hashes in the wire request.
        requested: usize,
        /// Number of authenticated accounts supplied for verification.
        supplied: usize,
    },
    /// An account hash does not match the same position in the authenticated account batch.
    #[error(
        "storage range account {index} requests {requested}, but authenticated account is {supplied}"
    )]
    AccountMismatch {
        /// Position of the mismatched account.
        index: usize,
        /// Account hash in the wire request.
        requested: B256,
        /// Account hash supplied with its storage root.
        supplied: B256,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snap::request::MAX_RETRIES;
    use alloy_primitives::{Bytes, KECCAK256_EMPTY};
    use reth_eth_wire_types::snap::{AccountRangeMessage, StorageRangesMessage};
    use reth_network_p2p::{
        error::PeerRequestResult, priority::Priority, test_utils::TestSnapClient,
    };
    use reth_network_peers::WithPeerId;
    use reth_trie_common::{proof::ProofRetainer, HashBuilder, Nibbles, EMPTY_ROOT_HASH};
    use std::sync::Arc;

    fn key(value: u64) -> B256 {
        B256::left_padding_from(&value.to_be_bytes())
    }

    fn account(hash: B256, storage_root: B256) -> (B256, TrieAccount) {
        (
            hash,
            TrieAccount { nonce: 0, balance: U256::ZERO, storage_root, code_hash: KECCAK256_EMPTY },
        )
    }

    fn storage_root(slots: &[(B256, U256)]) -> B256 {
        let mut builder = HashBuilder::default();
        for (hash, value) in slots {
            builder.add_leaf(Nibbles::unpack(*hash), &alloy_rlp::encode(value));
        }
        builder.root()
    }

    fn root_and_proof(slots: &[(B256, U256)], targets: &[B256]) -> (B256, Vec<Bytes>) {
        let targets = targets.iter().copied().map(Nibbles::unpack).collect();
        let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(targets));
        for (hash, value) in slots {
            builder.add_leaf(Nibbles::unpack(*hash), &alloy_rlp::encode(value));
        }
        let root = builder.root();
        let proof = builder
            .take_proof_nodes()
            .into_nodes_sorted()
            .into_iter()
            .map(|(_, node)| node)
            .collect();
        (root, proof)
    }

    fn request(accounts: &[(B256, TrieAccount)]) -> GetStorageRangesMessage {
        GetStorageRangesMessage {
            request_id: 1,
            root_hash: B256::repeat_byte(0xaa),
            account_hashes: accounts.iter().map(|(hash, _)| *hash).collect(),
            starting_hash: B256::ZERO.into(),
            limit_hash: MAX_HASH.into(),
            response_bytes: 512 * 1024,
        }
    }

    fn message(slots: Vec<Vec<StorageData>>, proof: Vec<Bytes>) -> StorageRangesMessage {
        StorageRangesMessage { request_id: 1, slots, proof }
    }

    fn response(peer_id: PeerId, message: StorageRangesMessage) -> PeerRequestResult<SnapResponse> {
        Ok(WithPeerId::new(peer_id, SnapResponse::StorageRanges(message)))
    }

    fn storage_data(slots: &[(B256, U256)]) -> Vec<StorageData> {
        slots.iter().map(|(hash, value)| StorageData::from_value(*hash, *value)).collect()
    }

    fn downloader(
        client: Arc<TestSnapClient>,
        request: GetStorageRangesMessage,
        accounts: &[(B256, TrieAccount)],
    ) -> Result<StorageRangeDownloader<Arc<TestSnapClient>>, InvalidStorageRangeRequest> {
        StorageRangeDownloader::new(client, request, accounts, Runtime::test())
    }

    #[tokio::test]
    async fn verifies_complete_storage_for_multiple_accounts() {
        let first_slots = vec![(key(1), U256::from(1)), (key(2), U256::from(2))];
        let second_slots = vec![(key(3), U256::from(3))];
        let accounts = vec![
            account(key(10), storage_root(&first_slots)),
            account(key(11), storage_root(&second_slots)),
        ];
        let peer_id = PeerId::random();
        let response = response(
            peer_id,
            message(vec![storage_data(&first_slots), storage_data(&second_slots)], Vec::new()),
        );
        let client = Arc::new(TestSnapClient::new([response]));

        let outcome =
            downloader(Arc::clone(&client), request(&accounts), &accounts).unwrap().await.unwrap();

        assert_eq!(
            outcome,
            StorageRangeOutcome::Verified(VerifiedStorageRanges {
                ranges: vec![
                    VerifiedStorageRange { account_hash: key(10), slots: first_slots },
                    VerifiedStorageRange { account_hash: key(11), slots: second_slots },
                ],
                continuation: None,
            })
        );
        assert!(client.reported().is_empty());
    }

    #[tokio::test]
    async fn partial_final_range_returns_a_slot_continuation() {
        let slots = vec![(key(1), U256::from(1)), (key(2), U256::from(2)), (key(3), U256::from(3))];
        let (root, proof) = root_and_proof(&slots, &[key(1), key(2)]);
        let accounts = vec![account(key(10), root)];
        let peer_id = PeerId::random();
        let client = Arc::new(TestSnapClient::new([response(
            peer_id,
            message(vec![storage_data(&slots[..2])], proof),
        )]));

        let outcome =
            downloader(Arc::clone(&client), request(&accounts), &accounts).unwrap().await.unwrap();

        assert_eq!(
            outcome,
            StorageRangeOutcome::Verified(VerifiedStorageRanges {
                ranges: vec![VerifiedStorageRange {
                    account_hash: key(10),
                    slots: slots[..2].to_vec(),
                }],
                continuation: Some(StorageRangeContinuation::Partial {
                    account_index: 0,
                    account_hash: key(10),
                    starting_hash: key(3),
                }),
            })
        );
    }

    #[tokio::test]
    async fn short_response_continues_at_the_next_account() {
        let first_slots = vec![(key(1), U256::from(1))];
        let accounts =
            vec![account(key(10), storage_root(&first_slots)), account(key(11), EMPTY_ROOT_HASH)];
        let peer_id = PeerId::random();
        let client = Arc::new(TestSnapClient::new([response(
            peer_id,
            message(vec![storage_data(&first_slots)], Vec::new()),
        )]));

        let outcome =
            downloader(Arc::clone(&client), request(&accounts), &accounts).unwrap().await.unwrap();

        let StorageRangeOutcome::Verified(verified) = outcome else { panic!("verified response") };
        assert_eq!(
            verified.continuation,
            Some(StorageRangeContinuation::NextAccount { account_index: 1, account_hash: key(11) })
        );
    }

    #[tokio::test]
    async fn authenticates_then_trims_a_slot_past_the_limit() {
        let slots = vec![(key(1), U256::from(1)), (key(3), U256::from(3)), (key(4), U256::from(4))];
        let (root, proof) = root_and_proof(&slots, &[key(1), key(3)]);
        let accounts = vec![account(key(10), root)];
        let mut request = request(&accounts);
        request.limit_hash = key(2).into();
        let peer_id = PeerId::random();
        let client = Arc::new(TestSnapClient::new([response(
            peer_id,
            message(vec![storage_data(&slots[..2])], proof),
        )]));

        let outcome = downloader(Arc::clone(&client), request, &accounts).unwrap().await.unwrap();

        assert_eq!(
            outcome,
            StorageRangeOutcome::Verified(VerifiedStorageRanges {
                ranges: vec![VerifiedStorageRange {
                    account_hash: key(10),
                    slots: slots[..1].to_vec(),
                }],
                continuation: None,
            })
        );
    }

    #[tokio::test]
    async fn empty_storage_trie_is_a_verified_range() {
        let accounts = vec![account(key(10), EMPTY_ROOT_HASH)];
        let peer_id = PeerId::random();
        let client = Arc::new(TestSnapClient::new([response(
            peer_id,
            message(vec![Vec::new()], Vec::new()),
        )]));

        let outcome =
            downloader(Arc::clone(&client), request(&accounts), &accounts).unwrap().await.unwrap();

        assert_eq!(
            outcome,
            StorageRangeOutcome::Verified(VerifiedStorageRanges {
                ranges: vec![VerifiedStorageRange { account_hash: key(10), slots: Vec::new() }],
                continuation: None,
            })
        );
    }

    #[tokio::test]
    async fn unavailable_state_names_the_peer_without_penalizing_it() {
        let accounts = vec![account(key(10), EMPTY_ROOT_HASH)];
        let peer_id = PeerId::random();
        let client =
            Arc::new(TestSnapClient::new([response(peer_id, message(Vec::new(), Vec::new()))]));

        let outcome =
            downloader(Arc::clone(&client), request(&accounts), &accounts).unwrap().await.unwrap();

        assert_eq!(outcome, StorageRangeOutcome::Unavailable { peer_id });
        assert!(client.reported().is_empty());
    }

    #[tokio::test]
    async fn invalid_response_is_reported_and_retried() {
        let slots = vec![(key(1), U256::from(1))];
        let accounts = vec![account(key(10), storage_root(&slots))];
        let bad_peer = PeerId::random();
        let good_peer = PeerId::random();
        let bad = Ok(WithPeerId::new(
            bad_peer,
            SnapResponse::AccountRange(AccountRangeMessage {
                request_id: 1,
                accounts: Vec::new(),
                proof: Vec::new(),
            }),
        ));
        let good = response(good_peer, message(vec![storage_data(&slots)], Vec::new()));
        let client = Arc::new(TestSnapClient::new([bad, good]));

        let outcome =
            downloader(Arc::clone(&client), request(&accounts), &accounts).unwrap().await.unwrap();

        assert!(matches!(outcome, StorageRangeOutcome::Verified(_)));
        assert_eq!(*client.reported(), [bad_peer]);
        assert_eq!(*client.priorities(), [Priority::Normal, Priority::High]);
    }

    #[tokio::test]
    async fn invalid_slots_exhaust_the_retry_budget() {
        let slots = vec![(key(1), U256::ZERO)];
        let accounts = vec![account(key(10), storage_root(&slots))];
        let peer_id = PeerId::random();
        let invalid = message(vec![storage_data(&slots)], Vec::new());
        let attempts = usize::from(MAX_RETRIES) + 1;
        let client = Arc::new(TestSnapClient::new(
            std::iter::repeat_with(|| response(peer_id, invalid.clone())).take(attempts),
        ));

        let error = downloader(Arc::clone(&client), request(&accounts), &accounts)
            .unwrap()
            .await
            .unwrap_err();

        assert_eq!(error, RequestError::BadResponse);
        assert_eq!(client.reported().len(), attempts);
    }

    #[test]
    fn rejects_invalid_slot_encoding_order_and_origin() {
        let account_hash = key(10);
        let first = StorageData::from_value(key(1), U256::from(1));
        let second = StorageData::from_value(key(2), U256::from(2));

        assert!(StorageProofVerifier::decode_slots(
            account_hash,
            key(1),
            &[first.clone(), second.clone()],
        )
        .is_ok());
        assert!(
            StorageProofVerifier::decode_slots(account_hash, key(1), &[second, first.clone()],)
                .is_err()
        );
        assert!(StorageProofVerifier::decode_slots(account_hash, key(2), &[first],).is_err());

        let malformed = StorageData { hash: key(2), data: Bytes::from_static(&[0x81]) };
        assert!(StorageProofVerifier::decode_slots(account_hash, key(1), &[malformed],).is_err());
    }

    #[test]
    fn storage_must_match_the_authenticated_account_root() {
        let committed = vec![(key(1), U256::from(1))];
        let served = vec![(key(1), U256::from(2))];
        let accounts = vec![account(key(10), storage_root(&committed))];
        let peer_id = PeerId::random();
        let message = message(vec![storage_data(&served)], Vec::new());
        let verifier = StorageProofVerifier {
            request: request(&accounts),
            storage_roots: vec![accounts[0].1.storage_root],
        };

        assert_eq!(
            verifier.verify_response(peer_id, SnapResponse::StorageRanges(message)).unwrap_err(),
            RequestError::BadResponse
        );
    }

    #[test]
    fn request_must_match_the_authenticated_accounts() {
        let accounts = vec![account(key(10), EMPTY_ROOT_HASH)];
        let client = Arc::new(TestSnapClient::new(std::iter::empty()));

        let mut empty = request(&accounts);
        empty.account_hashes.clear();
        assert!(matches!(
            downloader(Arc::clone(&client), empty, &[]),
            Err(InvalidStorageRangeRequest::NoAccounts)
        ));

        let mut reversed = request(&accounts);
        reversed.starting_hash = key(2).into();
        reversed.limit_hash = key(1).into();
        assert!(matches!(
            downloader(Arc::clone(&client), reversed, &accounts),
            Err(InvalidStorageRangeRequest::ReversedBounds { .. })
        ));

        let mut mismatched = request(&accounts);
        mismatched.account_hashes[0] = key(11);
        assert!(matches!(
            downloader(client, mismatched, &accounts),
            Err(InvalidStorageRangeRequest::AccountMismatch { .. })
        ));
    }
}
