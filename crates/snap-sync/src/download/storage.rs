//! Storage range, continuation and single-slot requests.

use super::{next_hash, StateDownloader, MAX_HASH, STORAGE_BATCH_SIZE};
use crate::{
    error::SnapSyncError, proof::verify_range_proof, MAX_REQUEST_ATTEMPTS,
    SNAP_RESPONSE_BYTES_LIMIT,
};
use alloy_primitives::{map::B256Map, Bytes, B256, U256};
use reth_db_api::transaction::DbTxMut;
use reth_eth_wire_types::snap::{GetStorageRangesMessage, StorageData, StorageRangesMessage};
use reth_network_p2p::snap::client::{SnapClient, SnapResponse};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::{root::storage_root, HashedPostState, HashedStorage};

impl<C, F> StateDownloader<'_, C, F>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    /// Fetches and writes storage for one account batch.
    ///
    /// Returns `true` when the serving peer no longer has the root.
    pub(super) async fn download_storage(
        &mut self,
        account_hashes: &[B256],
        storage_roots: &StorageRoots,
    ) -> Result<bool, SnapSyncError> {
        let mut idx = 0;

        while idx < account_hashes.len() {
            let end = (idx + STORAGE_BATCH_SIZE).min(account_hashes.len());
            let chunk = &account_hashes[idx..end];

            let Some(msg) = self.fetch_storage_ranges(chunk, B256::ZERO, storage_roots).await?
            else {
                // Servers answer with nothing at all when an account is missing at this root,
                // rather than skipping it, so an empty response means the root is gone.
                return Ok(true)
            };

            let returned = msg.slots.len();
            // A proof is only attached to the last returned account, and only when its range is
            // partial; everything before it is a complete zero-origin range.
            let truncated_index = (!msg.proof.is_empty()).then_some(returned - 1);
            let mut storages = B256Map::default();

            for (i, slots) in msg.slots.iter().enumerate() {
                let account_hash = chunk[i];

                let account_slots = if Some(i) == truncated_index {
                    let decoded = storage_roots.decode_slots(slots)?;

                    // An empty slot list with a proof is an absence proof for the whole storage
                    // trie, which the fetch already checked.
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
                    storage_roots.decode_slots(slots)?
                };

                // A complete zero-origin trie replaces whatever was stored for this account:
                // merging would keep slots that the trie being downloaded does not contain.
                storages.insert(account_hash, HashedStorage::from_iter(true, account_slots));
            }

            self.writer.write_state(HashedPostState { accounts: B256Map::default(), storages })?;

            idx += returned;
        }

        Ok(false)
    }

    /// Requests storage for `accounts`, retrying with another peer on an untrustworthy response.
    ///
    /// Returns `None` when the peer cannot serve the root. Every returned slot list has been
    /// checked against its account's storage root, or against the boundary proof when the last
    /// one was truncated.
    async fn fetch_storage_ranges(
        &mut self,
        accounts: &[B256],
        origin: B256,
        storage_roots: &StorageRoots,
    ) -> Result<Option<StorageRangesMessage>, SnapSyncError> {
        let mut last_error = None;
        let mut unavailable = false;

        for _ in 0..MAX_REQUEST_ATTEMPTS {
            let request_id = self.next_request_id();
            let response = match self
                .client
                .get_storage_ranges(GetStorageRangesMessage {
                    request_id,
                    root_hash: self.root_hash,
                    account_hashes: accounts.to_vec(),
                    starting_hash: origin.into(),
                    limit_hash: MAX_HASH.into(),
                    response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
                })
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(SnapSyncError::Network(format!(
                        "snap storage range request failed: {err}"
                    )));
                    continue
                }
            };

            let (peer, data) = response.split();
            let SnapResponse::StorageRanges(msg) = data else {
                last_error = Some(self.penalize(
                    peer,
                    SnapSyncError::Network("expected a storage ranges response".into()),
                ));
                continue
            };

            if msg.slots.len() > accounts.len() {
                last_error = Some(self.penalize(
                    peer,
                    SnapSyncError::Network(
                        "snap storage range returned more slot lists than requested".into(),
                    ),
                ));
                continue
            }

            // This peer cannot serve the root, but another still might, so keep the attempt
            // budget rather than ending the request on the first empty reply.
            if msg.slots.is_empty() {
                unavailable = true;
                continue
            }

            match storage_roots.verify_response(accounts, origin, &msg) {
                Ok(()) => return Ok(Some(msg)),
                Err(err) => last_error = Some(self.penalize(peer, err)),
            }
        }

        if unavailable {
            return Ok(None)
        }
        Err(last_error.expect("at least one attempt was made"))
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
            let Some(msg) =
                self.fetch_storage_ranges(&[account_hash], starting_hash, storage_roots).await?
            else {
                return Ok(StorageContinuation::Stale)
            };

            let slots = msg.slots.first().expect("a non-empty response was verified");
            collected.extend(storage_roots.decode_slots(slots)?);

            // Without a boundary proof the peer reached the end of this account's storage.
            let next = slots.last().filter(|_| !msg.proof.is_empty()).map(|last| last.hash);
            let Some(next) = next.and_then(next_hash) else {
                storage_roots.verify_root(account_hash, &collected)?;
                return Ok(StorageContinuation::Complete(collected))
            };

            starting_hash = next;
        }
    }
}

// Checks that need neither a client nor a database.
impl<C, F> StateDownloader<'_, C, F> {}

/// The storage roots committed to by an account range, used to check the storage served for it.
pub(super) struct StorageRoots(pub(super) B256Map<B256>);

impl StorageRoots {
    /// Checks every slot list in a storage-ranges response.
    ///
    /// All but the last are complete zero-origin ranges checked against their storage root; the
    /// last is checked against the boundary proof when one is attached, because a truncated range
    /// cannot rebuild the root on its own.
    fn verify_response(
        &self,
        accounts: &[B256],
        origin: B256,
        msg: &StorageRangesMessage,
    ) -> Result<(), SnapSyncError> {
        let truncated_index = (!msg.proof.is_empty()).then_some(msg.slots.len() - 1);

        for (i, slots) in msg.slots.iter().enumerate() {
            let account_hash = *accounts.get(i).ok_or_else(|| {
                SnapSyncError::Network("snap storage range returned an unrequested list".into())
            })?;

            self.validate_slots(account_hash, origin, slots)?;

            if Some(i) == truncated_index {
                self.verify_partial(account_hash, origin, slots, &msg.proof)?;
            } else {
                self.verify_complete(account_hash, slots)?;
            }
        }

        Ok(())
    }

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

/// Decoded storage slots for one account, in the order the peer served them.
pub(super) type DecodedSlots = Vec<(B256, U256)>;

#[cfg(test)]
mod tests {
    use super::*;
    use reth_trie::EMPTY_ROOT_HASH;

    fn b256(value: u64) -> B256 {
        B256::left_padding_from(&value.to_be_bytes())
    }

    fn slot(hash: B256, value: u64) -> StorageData {
        StorageData::from_value(hash, U256::from(value))
    }

    fn storage_roots(account: B256, root: B256) -> StorageRoots {
        StorageRoots(B256Map::from_iter([(account, root)]))
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
}
