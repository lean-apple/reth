//! Streaming download of accounts, storage and bytecodes at a fixed state root.
//!
//! [`StateDownloader`] walks the account trie in hashed order. Each account batch is verified
//! against the pivot root, written, and immediately followed by that batch's storage and
//! bytecodes before the next range is requested, so peak memory stays at one batch regardless of
//! how large the state is.

use crate::{
    proof::verify_range_proof, storage::SnapStateWriter, SnapSyncError, SNAP_RESPONSE_BYTES_LIMIT,
};
use alloy_primitives::{
    keccak256,
    map::{B256Map, B256Set},
    Bytes, B256, KECCAK256_EMPTY, U256,
};
use reth_db_api::transaction::DbTxMut;
use reth_eth_wire_types::snap::{
    AccountData, GetAccountRangeMessage, GetByteCodesMessage, GetStorageRangesMessage, StorageData,
};
use reth_network_p2p::snap::client::{SnapClient, SnapResponse};
use reth_primitives_traits::Account;
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::{root::storage_root, HashedPostState, HashedStorage, TrieAccount};
use tracing::debug;

/// Maximum number of account hashes per storage range request.
const STORAGE_BATCH_SIZE: usize = 20;

/// Maximum number of code hashes per bytecode request.
const BYTECODE_BATCH_SIZE: usize = 50;

/// Upper bound of the hashed key space.
const MAX_HASH: B256 = B256::new([0xff; 32]);

/// Downloads the hashed state at one state root from snap peers.
#[derive(Debug)]
pub struct StateDownloader<'a, C, F> {
    /// Peer client used for every snap request.
    client: &'a C,
    /// Sink for verified state.
    writer: SnapStateWriter<'a, F>,
    /// The state root every response is verified against.
    root_hash: B256,
    /// Monotonic counter correlating requests with responses.
    request_id: u64,
}

impl<'a, C, F> StateDownloader<'a, C, F>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    /// Creates a downloader for the state at `root_hash`.
    pub const fn new(client: &'a C, factory: &'a F, root_hash: B256) -> Self {
        Self { client, writer: SnapStateWriter::new(factory), root_hash, request_id: 0 }
    }

    /// Downloads accounts, storage and bytecodes starting from `starting_hash`.
    pub async fn run(
        &mut self,
        starting_hash: B256,
    ) -> Result<DownloadStateOutcome, SnapSyncError> {
        let mut cursor = starting_hash;

        loop {
            // Retrying a stale root restarts at the batch boundary, not mid-batch, so an account's
            // storage and code are never left half-written against a root we stopped trusting.
            let batch_start = cursor;

            let request_id = self.next_request_id();
            let response = self
                .client
                .get_account_range(GetAccountRangeMessage {
                    request_id,
                    root_hash: self.root_hash,
                    starting_hash: cursor,
                    limit_hash: MAX_HASH,
                    response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
                })
                .await
                .map_err(|err| {
                    SnapSyncError::Network(format!("snap account range request failed: {err}"))
                })?;

            let SnapResponse::AccountRange(msg) = response.into_data() else {
                return Err(SnapSyncError::Network("expected an account range response".into()))
            };

            if msg.accounts.is_empty() {
                // A server that cannot serve the root replies fully empty; an absence proof
                // instead means the range really is past the last account.
                if msg.proof.is_empty() {
                    return Ok(DownloadStateOutcome::Stale { resume_from: cursor })
                }
                self.verify_account_range(cursor, &[], &msg.proof)?;
                return Ok(DownloadStateOutcome::Done)
            }

            let decoded = Self::decode_account_range(&msg.accounts, cursor)?;
            self.verify_account_range(cursor, &decoded, &msg.proof)?;

            let accounts = decoded
                .iter()
                .map(|(hash, account)| (*hash, Some(Account::from(*account))))
                .collect::<B256Map<_>>();
            let code_hashes = decoded
                .iter()
                .map(|(_, account)| account.code_hash)
                .filter(|hash| *hash != KECCAK256_EMPTY)
                .collect::<B256Set>();
            let storage_roots = StorageRoots(
                decoded.iter().map(|(hash, account)| (*hash, account.storage_root)).collect(),
            );

            debug!(
                target: "engine::snap",
                accounts = accounts.len(),
                root_hash = %self.root_hash,
                "Downloaded account range"
            );
            self.writer.write_state(HashedPostState { accounts, storages: B256Map::default() })?;

            let account_hashes: Vec<B256> = decoded.iter().map(|(hash, _)| *hash).collect();
            if self.download_storage(&account_hashes, &storage_roots).await? {
                return Ok(DownloadStateOutcome::Stale { resume_from: batch_start })
            }

            self.download_bytecodes(&code_hashes).await?;

            // No boundary proof means the server exhausted the trie from a zero origin, which
            // `verify_account_range` already checked against the root.
            let last_hash = account_hashes.last().copied().expect("checked non-empty above");
            if msg.proof.is_empty() {
                return Ok(DownloadStateOutcome::Done)
            }
            let Some(next) = next_hash(last_hash) else { return Ok(DownloadStateOutcome::Done) };
            cursor = next;
        }
    }

    /// Fetches and writes storage for one account batch.
    ///
    /// Returns `true` when the serving peer no longer has the root.
    async fn download_storage(
        &mut self,
        account_hashes: &[B256],
        storage_roots: &StorageRoots,
    ) -> Result<bool, SnapSyncError> {
        let mut idx = 0;

        while idx < account_hashes.len() {
            let end = (idx + STORAGE_BATCH_SIZE).min(account_hashes.len());
            let chunk = &account_hashes[idx..end];

            let request_id = self.next_request_id();
            let response = self
                .client
                .get_storage_ranges(GetStorageRangesMessage {
                    request_id,
                    root_hash: self.root_hash,
                    account_hashes: chunk.to_vec(),
                    starting_hash: B256::ZERO.into(),
                    limit_hash: MAX_HASH.into(),
                    response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
                })
                .await
                .map_err(|err| {
                    SnapSyncError::Network(format!("snap storage range request failed: {err}"))
                })?;

            let SnapResponse::StorageRanges(msg) = response.into_data() else {
                return Err(SnapSyncError::Network("expected a storage ranges response".into()))
            };

            if msg.slots.len() > chunk.len() {
                return Err(SnapSyncError::Network(
                    "snap storage range returned more slot lists than requested".into(),
                ))
            }

            // Servers answer with nothing at all when an account is missing at this root, rather
            // than skipping it and shifting the rest, so an empty response means the root is gone.
            let returned = msg.slots.len();
            if returned == 0 {
                return Ok(true)
            }

            // A proof is only attached to the last returned account, and only when its range is
            // partial; everything before it is a complete zero-origin range.
            let truncated_index = (!msg.proof.is_empty()).then_some(returned - 1);
            let mut storages = B256Map::default();

            for (i, slots) in msg.slots.iter().enumerate() {
                let account_hash = chunk[i];
                storage_roots.validate_slots(account_hash, B256::ZERO, slots)?;

                let account_slots = if Some(i) == truncated_index {
                    let decoded = storage_roots.verify_partial(
                        account_hash,
                        B256::ZERO,
                        slots,
                        &msg.proof,
                    )?;

                    // An empty slot list with a proof is an absence proof for the whole storage
                    // trie, which `verify_partial` already checked.
                    match slots.last().and_then(|last| next_hash(last.hash)) {
                        Some(resume_from) => {
                            match self
                                .continue_storage(account_hash, storage_roots, resume_from, decoded)
                                .await?
                            {
                                StorageContinuation::Complete(slots) => slots,
                                StorageContinuation::Stale => return Ok(true),
                            }
                        }
                        None => decoded,
                    }
                } else {
                    storage_roots.verify_complete(account_hash, slots)?
                };

                storages.insert(account_hash, HashedStorage::from_iter(false, account_slots));
            }

            self.writer.write_state(HashedPostState { accounts: B256Map::default(), storages })?;

            idx += returned;
        }

        Ok(false)
    }

    /// Requests the remainder of one account's storage until it verifies against its storage root.
    async fn continue_storage(
        &mut self,
        account_hash: B256,
        storage_roots: &StorageRoots,
        mut starting_hash: B256,
        mut collected: DecodedSlots,
    ) -> Result<StorageContinuation, SnapSyncError> {
        loop {
            let request_id = self.next_request_id();
            let response = self
                .client
                .get_storage_ranges(GetStorageRangesMessage {
                    request_id,
                    root_hash: self.root_hash,
                    account_hashes: vec![account_hash],
                    starting_hash: starting_hash.into(),
                    limit_hash: MAX_HASH.into(),
                    response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
                })
                .await
                .map_err(|err| {
                    SnapSyncError::Network(format!("snap storage continuation failed: {err}"))
                })?;

            let SnapResponse::StorageRanges(msg) = response.into_data() else {
                return Err(SnapSyncError::Network("expected a storage ranges response".into()))
            };

            if msg.slots.len() > 1 {
                return Err(SnapSyncError::Network(
                    "snap storage continuation returned multiple slot lists".into(),
                ))
            }

            let Some(slots) = msg.slots.first() else { return Ok(StorageContinuation::Stale) };

            storage_roots.validate_slots(account_hash, starting_hash, slots)?;
            collected.extend(storage_roots.verify_partial(
                account_hash,
                starting_hash,
                slots,
                &msg.proof,
            )?);

            // Without a boundary proof the peer reached the end of this account's storage.
            let next = slots.last().filter(|_| !msg.proof.is_empty()).map(|last| last.hash);
            let Some(next) = next.and_then(next_hash) else {
                storage_roots.verify_root(account_hash, &collected)?;
                return Ok(StorageContinuation::Complete(collected))
            };

            starting_hash = next;
        }
    }

    /// Fetches and writes bytecodes for a set of code hashes.
    async fn download_bytecodes(&mut self, code_hashes: &B256Set) -> Result<(), SnapSyncError> {
        let hashes: Vec<B256> = code_hashes.iter().copied().collect();

        for chunk in hashes.chunks(BYTECODE_BATCH_SIZE) {
            let request_id = self.next_request_id();
            let response = self
                .client
                .get_byte_codes(GetByteCodesMessage {
                    request_id,
                    hashes: chunk.to_vec(),
                    response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
                })
                .await
                .map_err(|err| {
                    SnapSyncError::Network(format!("snap bytecode request failed: {err}"))
                })?;

            let SnapResponse::ByteCodes(msg) = response.into_data() else {
                return Err(SnapSyncError::Network("expected a byte codes response".into()))
            };

            let codes = Self::match_bytecodes(chunk, &msg.codes)?;
            if !codes.is_empty() {
                self.writer.write_bytecodes(&codes)?;
            }
        }

        Ok(())
    }

    const fn next_request_id(&mut self) -> u64 {
        self.request_id += 1;
        self.request_id
    }
}

// Verification and decoding of served responses, which need neither a client nor a database.
impl<C, F> StateDownloader<'_, C, F> {
    /// Checks a served account range against the pivot root.
    fn verify_account_range(
        &self,
        origin: B256,
        accounts: &[(B256, TrieAccount)],
        proof: &[Bytes],
    ) -> Result<(), SnapSyncError> {
        let leaves = accounts.iter().map(|(hash, account)| (*hash, alloy_rlp::encode(account)));

        verify_range_proof(self.root_hash, origin, leaves, proof).map_err(|err| {
            SnapSyncError::Network(format!("invalid snap account range proof: {err}"))
        })
    }

    /// Decodes a served account range, rejecting orderings that would let a peer hide accounts.
    fn decode_account_range(
        accounts: &[AccountData],
        origin: B256,
    ) -> Result<Vec<(B256, TrieAccount)>, SnapSyncError> {
        let mut decoded = Vec::with_capacity(accounts.len());
        let mut previous = None;

        for account in accounts {
            if account.hash < origin {
                return Err(SnapSyncError::Network(
                    "snap account range returned an account before the requested origin".into(),
                ))
            }
            if previous.is_some_and(|previous| account.hash <= previous) {
                return Err(SnapSyncError::Network(
                    "snap account range returned non-monotonic account hashes".into(),
                ))
            }
            previous = Some(account.hash);

            let account_body = account.trie_account().map_err(|err| {
                SnapSyncError::RlpDecode(format!("snap slim account body: {err}"))
            })?;
            decoded.push((account.hash, account_body));
        }

        Ok(decoded)
    }

    /// Pairs returned bytecodes with the hashes that were requested.
    ///
    /// Servers may drop entries they don't have but must keep request order, so a short reply is a
    /// valid prefix while a reordered or duplicated one is not.
    fn match_bytecodes(
        requested_hashes: &[B256],
        codes: &[Bytes],
    ) -> Result<Vec<(B256, Bytes)>, SnapSyncError> {
        let requested: B256Map<usize> =
            requested_hashes.iter().copied().enumerate().map(|(i, hash)| (hash, i)).collect();
        let mut last_position = None;
        let mut matched = Vec::with_capacity(codes.len());

        for code in codes {
            let hash = keccak256(code.as_ref());
            let Some(position) = requested.get(&hash).copied() else {
                return Err(SnapSyncError::Network(format!(
                    "snap bytecode response contained unrequested code hash {hash}"
                )))
            };
            if last_position.is_some_and(|last| position <= last) {
                return Err(SnapSyncError::Network(
                    "snap bytecode response was not in request order".into(),
                ))
            }
            last_position = Some(position);
            matched.push((hash, code.clone()));
        }

        Ok(matched)
    }
}

/// Result of a [`StateDownloader::run`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum DownloadStateOutcome {
    /// The whole account range was iterated and written; the state at the root is complete.
    Done,
    /// No peer could serve the requested root any more.
    ///
    /// Carries the account hash to resume from once the caller has a fresher root. State written
    /// before this point stays valid, because every batch was verified against the root it was
    /// served at.
    Stale {
        /// Account hash to resume the download from.
        resume_from: B256,
    },
}

/// Decoded storage slots for one account, in the order the peer served them.
type DecodedSlots = Vec<(B256, U256)>;

/// The storage roots committed to by an account range, used to check the storage served for it.
struct StorageRoots(B256Map<B256>);

impl StorageRoots {
    /// Checks a partial storage range against its boundary proof and returns the decoded slots.
    fn verify_partial(
        &self,
        account_hash: B256,
        origin: B256,
        slots: &[StorageData],
        proof: &[Bytes],
    ) -> Result<DecodedSlots, SnapSyncError> {
        let root = self.get(account_hash)?;
        // The trie leaf is the RLP-encoded slot value, which is exactly what the server sent.
        let leaves = slots.iter().map(|slot| (slot.hash, slot.data.clone()));

        verify_range_proof(root, origin, leaves, proof).map_err(|err| {
            SnapSyncError::Network(format!("invalid snap storage range proof: {err}"))
        })?;

        self.decode_slots(slots)
    }

    /// Checks that `slots` is the complete storage trie for `account_hash`.
    fn verify_complete(
        &self,
        account_hash: B256,
        slots: &[StorageData],
    ) -> Result<DecodedSlots, SnapSyncError> {
        let decoded = self.decode_slots(slots)?;
        self.verify_root(account_hash, &decoded)?;
        Ok(decoded)
    }

    /// Rebuilds the storage trie from `slots` and checks it against the account's storage root.
    fn verify_root(&self, account_hash: B256, slots: &DecodedSlots) -> Result<(), SnapSyncError> {
        let expected = self.get(account_hash)?;
        // Safe to treat as sorted: `validate_slots` rejected any non-monotonic response.
        let computed = storage_root(slots.iter().copied());

        if computed != expected {
            return Err(SnapSyncError::Network(format!(
                "snap storage for account {account_hash} rebuilds to {computed}, not {expected}"
            )))
        }
        Ok(())
    }

    /// Rejects slot orderings that would let a peer hide storage.
    fn validate_slots(
        &self,
        account_hash: B256,
        origin: B256,
        slots: &[StorageData],
    ) -> Result<(), SnapSyncError> {
        let mut previous = None;
        for slot in slots {
            if slot.hash < origin {
                return Err(SnapSyncError::Network(format!(
                    "snap storage range for account {account_hash} returned a slot before the origin"
                )))
            }
            if previous.is_some_and(|previous| slot.hash <= previous) {
                return Err(SnapSyncError::Network(format!(
                    "snap storage range for account {account_hash} returned non-monotonic slots"
                )))
            }
            previous = Some(slot.hash);
        }
        Ok(())
    }

    fn decode_slots(&self, slots: &[StorageData]) -> Result<DecodedSlots, SnapSyncError> {
        slots
            .iter()
            .map(|slot| {
                let value = slot
                    .value()
                    .map_err(|err| SnapSyncError::RlpDecode(format!("snap storage slot: {err}")))?;
                Ok((slot.hash, value))
            })
            .collect()
    }

    fn get(&self, account_hash: B256) -> Result<B256, SnapSyncError> {
        self.0.get(&account_hash).copied().ok_or_else(|| {
            SnapSyncError::Network(format!(
                "snap storage response for unrequested account {account_hash}"
            ))
        })
    }
}

/// Outcome of continuing a single account's truncated storage range.
enum StorageContinuation {
    /// The account's storage is complete and matches its storage root.
    Complete(DecodedSlots),
    /// The serving peer no longer has the requested root.
    Stale,
}

/// Returns the next hash after `hash`, or `None` at the end of the key space.
fn next_hash(hash: B256) -> Option<B256> {
    U256::from_be_bytes(hash.0).checked_add(U256::from(1)).map(B256::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_trie::EMPTY_ROOT_HASH;

    type Downloader<'a> = StateDownloader<'a, (), ()>;

    fn b256(value: u64) -> B256 {
        B256::left_padding_from(&value.to_be_bytes())
    }

    fn slot(hash: B256, value: u64) -> StorageData {
        StorageData::from_value(hash, U256::from(value))
    }

    fn account_data(hash: B256, nonce: u64) -> AccountData {
        AccountData::from_trie_account(
            hash,
            &TrieAccount {
                nonce,
                balance: U256::from(1),
                storage_root: EMPTY_ROOT_HASH,
                code_hash: KECCAK256_EMPTY,
            },
        )
    }

    fn storage_roots(account: B256, root: B256) -> StorageRoots {
        StorageRoots(B256Map::from_iter([(account, root)]))
    }

    #[test]
    fn next_hash_steps_and_stops_at_the_end() {
        assert_eq!(next_hash(B256::ZERO), Some(b256(1)));
        assert_eq!(next_hash(MAX_HASH), None);
    }

    #[test]
    fn account_range_round_trips_through_the_slim_encoding() {
        let decoded =
            Downloader::decode_account_range(&[account_data(b256(1), 7)], B256::ZERO).unwrap();

        assert_eq!(decoded[0].0, b256(1));
        assert_eq!(decoded[0].1.nonce, 7);
        assert_eq!(decoded[0].1.storage_root, EMPTY_ROOT_HASH);
        assert_eq!(decoded[0].1.code_hash, KECCAK256_EMPTY);
    }

    #[test]
    fn account_range_rejects_out_of_order_accounts() {
        let accounts = [account_data(b256(2), 0), account_data(b256(1), 0)];

        assert!(Downloader::decode_account_range(&accounts, B256::ZERO).is_err());
    }

    #[test]
    fn account_range_rejects_accounts_before_origin() {
        let accounts = [account_data(b256(1), 0)];

        assert!(Downloader::decode_account_range(&accounts, b256(2)).is_err());
    }

    #[test]
    fn complete_storage_range_must_rebuild_the_storage_root() {
        let account = b256(1);
        let slots = [slot(b256(2), 2), slot(b256(3), 3)];
        let roots = storage_roots(
            account,
            storage_root([(b256(2), U256::from(2)), (b256(3), U256::from(3))]),
        );

        assert!(roots.verify_complete(account, &slots).is_ok());
        // Dropping a slot must not still verify, otherwise a peer could withhold storage.
        assert!(roots.verify_complete(account, &slots[..1]).is_err());
    }

    #[test]
    fn empty_storage_verifies_against_the_empty_root() {
        let account = b256(1);

        assert!(storage_roots(account, EMPTY_ROOT_HASH).verify_complete(account, &[]).is_ok());
    }

    #[test]
    fn storage_for_an_unrequested_account_is_rejected() {
        let roots = storage_roots(b256(1), EMPTY_ROOT_HASH);

        assert!(roots.verify_complete(b256(2), &[]).is_err());
    }

    #[test]
    fn storage_slots_must_be_ordered_from_the_origin() {
        let account = b256(1);
        let roots = storage_roots(account, EMPTY_ROOT_HASH);
        let first = slot(b256(2), 2);
        let second = slot(b256(3), 3);

        assert!(roots.validate_slots(account, b256(2), &[first.clone(), second.clone()]).is_ok());
        assert!(roots.validate_slots(account, b256(2), &[second.clone(), first]).is_err());
        assert!(roots.validate_slots(account, b256(4), &[second]).is_err());
    }

    #[test]
    fn bytecode_matching_accepts_a_short_prefix() {
        let first = Bytes::from_static(&[1, 2, 3]);
        let second = Bytes::from_static(&[4, 5, 6]);
        let requested = [keccak256(first.as_ref()), keccak256(second.as_ref())];

        let matched =
            Downloader::match_bytecodes(&requested, std::slice::from_ref(&first)).unwrap();

        assert_eq!(matched, vec![(keccak256(first.as_ref()), first)]);
    }

    #[test]
    fn bytecode_matching_rejects_unrequested_code() {
        let requested = [keccak256([1, 2, 3])];

        assert!(Downloader::match_bytecodes(&requested, &[Bytes::from_static(&[4, 5, 6])]).is_err());
    }

    #[test]
    fn bytecode_matching_rejects_out_of_order_and_duplicate_codes() {
        let first = Bytes::from_static(&[1, 2, 3]);
        let second = Bytes::from_static(&[4, 5, 6]);
        let requested = [keccak256(first.as_ref()), keccak256(second.as_ref())];

        assert!(Downloader::match_bytecodes(&requested, &[second, first.clone()]).is_err());
        assert!(Downloader::match_bytecodes(&requested, &[first.clone(), first]).is_err());
    }
}
