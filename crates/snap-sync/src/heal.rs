//! Healing: verifying block access lists and applying them to downloaded state.
//!
//! This is what EIP-8189 replaces snap/1's trie healing with. A block's access list (EIP-7928)
//! records the post-block value of every account field and storage slot the block touched, so the
//! state at the pivot can be carried to the head by replaying those values — no transaction
//! execution and no trie-node round trips.
//!
//! A BAL only carries the fields a block changed, so applying one means merging it onto the
//! account already in the database rather than overwriting it.

use crate::{error::SnapSyncError, store::SnapStateWriter};
use alloy_eip7928::{
    bal::{Bal, DecodedBal, RawBal},
    AccountChanges,
};
use alloy_primitives::{
    keccak256,
    map::{AddressMap, B256Map, B256Set},
    Address, Bytes, B256, U256,
};
use reth_db_api::transaction::DbTxMut;
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{AccountExtReader, DBProvider, StateWriter};
use reth_trie::{HashedPostState, HashedStorage};
use reth_trie_common::bal::{self, BalAccountState};

/// The state changes one block's access list commits to, in hashed-key form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BlockStateDiff {
    /// Per-account field changes with their address and hashed address.
    accounts: Vec<(Address, B256, BalAccountState)>,
    /// Post-block slot values, keyed by hashed address then hashed slot.
    storage: B256Map<B256Map<U256>>,
    /// `(code hash, code)` pairs for contracts deployed in this block.
    bytecodes: Vec<(B256, Bytes)>,
}

impl BlockStateDiff {
    /// Builds the diff for a block from its decoded access list.
    pub(crate) fn from_changes(changes: &[AccountChanges]) -> Self {
        let mut diff = Self::default();

        for account in changes {
            let hashed_address = keccak256(account.address);
            let state = BalAccountState::from_changes(account);

            if let Some(code) = bal::deployed_bytecode(account) {
                diff.bytecodes.push(code);
            }
            for (hashed_slot, value) in bal::hashed_storage_changes(account) {
                diff.storage.entry(hashed_address).or_default().insert(hashed_slot, value);
            }

            // Accounts that were only read appear in the list with no changes at all.
            if !state.is_empty() {
                diff.accounts.push((account.address, hashed_address, state));
            }
        }

        diff
    }

    /// Merges this diff onto the state already in the database and writes the result.
    ///
    /// `limit` restricts the write to accounts below that hashed address. A session moving its
    /// target uses it to carry only the prefix it has already downloaded; the rest of the trie
    /// arrives at the new root anyway, so applying to it would be wasted work at best.
    pub(crate) fn apply<F>(
        &self,
        writer: SnapStateWriter<'_, F>,
        limit: Option<B256>,
    ) -> Result<(), SnapSyncError>
    where
        F: DatabaseProviderFactory,
        F::Provider: AccountExtReader + DBProvider,
        F::ProviderRW: DBProvider<Tx: DbTxMut> + StateWriter,
    {
        let within = |address: &B256| limit.is_none_or(|limit| *address < limit);
        let existing_accounts: AddressMap<_> = writer
            .read_accounts(self.accounts.iter().filter_map(|(address, hashed_address, state)| {
                (within(hashed_address) && state.needs_parent_account()).then_some(*address)
            }))?
            .into_iter()
            .collect();

        let mut accounts = B256Map::default();
        let mut deleted = B256Set::default();
        for (address, hashed_address, state) in
            self.accounts.iter().filter(|(_, hashed_address, _)| within(hashed_address))
        {
            let existing = existing_accounts.get(address).and_then(Option::as_ref);
            let merged = state.merge_onto(existing);

            // An account left with no balance, no nonce and no code does not exist under
            // EIP-161, so it has to be removed rather than written as an empty leaf. Storing one
            // would put a node in the trie that the block's state root does not account for.
            if merged.is_empty() {
                deleted.insert(*hashed_address);
                accounts.insert(*hashed_address, None);
            } else {
                accounts.insert(*hashed_address, Some(merged));
            }
        }

        let mut storages: B256Map<HashedStorage> = self
            .storage
            .iter()
            .filter(|(address, _)| within(address))
            .map(|(address, slots)| {
                // A block access list states the slots it changed, not the ones it left alone, so
                // these merge onto what is stored.
                let mut storage = HashedStorage::new(deleted.contains(address));
                storage.storage.extend(slots.iter().map(|(key, value)| (*key, *value)));
                (*address, storage)
            })
            .collect();

        // Storage rows are only cleared for an account marked wiped, so a deleted account that
        // changed no slots still needs an entry; otherwise its slots outlive it and a later
        // recreation at the same address inherits them.
        for address in deleted {
            storages.entry(address).or_insert_with(|| HashedStorage::new(true));
        }

        // One transaction for state and code together: a crash between the two would leave an
        // account's code hash pointing at bytecode the database does not have, which the final
        // root check cannot catch because code lives outside the trie.
        writer.commit_batch(HashedPostState { accounts, storages }, &self.bytecodes)?;

        Ok(())
    }
}

/// Decodes a block access list whose hash has already been checked against its header.
///
/// Rejects trailing bytes, which a raw `Vec<AccountChanges>` decode would ignore.
pub(crate) fn decode_block_access_list(
    bal: RawBal,
    block_number: u64,
) -> Result<Bal, SnapSyncError> {
    DecodedBal::from_raw_bal(bal).map(|decoded| decoded.split().0).map_err(|err| {
        SnapSyncError::RlpDecode(format!("block access list for block {block_number}: {err}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eip7928::{
        BalanceChange, BlockAccessIndex, CodeChange, NonceChange, SlotChanges, StorageChange,
    };
    use alloy_primitives::Address;
    use reth_db_api::{models::StorageSettings, tables, transaction::DbTx};
    use reth_primitives_traits::Account;
    use reth_provider::{test_utils::create_test_provider_factory, StorageSettingsCache};

    fn index(value: u64) -> BlockAccessIndex {
        BlockAccessIndex::new(value)
    }

    #[test]
    fn last_change_by_index_wins() {
        let address = Address::repeat_byte(0xaa);
        let mut changes = AccountChanges::new(address);
        // Deliberately out of order: the index decides, not the position.
        changes.balance_changes.push(BalanceChange::new(index(3), U256::from(30)));
        changes.balance_changes.push(BalanceChange::new(index(1), U256::from(10)));
        changes.nonce_changes.push(NonceChange::new(index(2), 7));
        changes.nonce_changes.push(NonceChange::new(index(1), 5));

        let diff = BlockStateDiff::from_changes(&[changes]);

        assert_eq!(diff.accounts.len(), 1);
        assert_eq!(diff.accounts[0].0, address);
        assert_eq!(diff.accounts[0].1, keccak256(address));
        assert_eq!(diff.accounts[0].2.balance, Some(U256::from(30)));
        assert_eq!(diff.accounts[0].2.nonce, Some(7));
    }

    #[test]
    fn storage_slots_are_hashed_and_take_the_final_value() {
        let address = Address::repeat_byte(0xbb);
        let slot = U256::from(1);
        let mut changes = AccountChanges::new(address);
        changes.storage_changes.push(SlotChanges::new(
            slot,
            vec![
                StorageChange::new(index(1), U256::from(11)),
                StorageChange::new(index(4), U256::from(44)),
            ],
        ));

        let diff = BlockStateDiff::from_changes(&[changes]);

        assert_eq!(diff.storage[&keccak256(address)][&keccak256(B256::from(slot))], U256::from(44));
    }

    #[test]
    fn deployed_code_is_collected_with_its_hash() {
        let address = Address::repeat_byte(0xcc);
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let mut changes = AccountChanges::new(address);
        changes.code_changes.push(CodeChange::new(index(1), code.clone()));

        let diff = BlockStateDiff::from_changes(&[changes]);

        assert_eq!(diff.bytecodes, vec![(keccak256(&code), code.clone())]);
        assert_eq!(diff.accounts[0].2.code_hash, Some(Some(keccak256(&code))));
    }

    #[test]
    fn read_only_accounts_produce_no_diff() {
        let mut changes = AccountChanges::new(Address::repeat_byte(0xdd));
        changes.storage_reads.push(U256::from(1));

        let diff = BlockStateDiff::from_changes(&[changes]);

        assert!(diff.accounts.is_empty());
        assert!(diff.storage.is_empty());
    }

    #[test]
    fn partial_changes_merge_with_the_stored_account() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v2());
        let writer = SnapStateWriter::new(&factory);
        let address = Address::repeat_byte(0xdd);
        let hashed_address = keccak256(address);
        let existing = Account {
            nonce: 7,
            balance: U256::from(10),
            bytecode_hash: Some(B256::repeat_byte(0xee)),
        };
        writer
            .commit_batch(
                HashedPostState {
                    accounts: B256Map::from_iter([(hashed_address, Some(existing))]),
                    storages: B256Map::default(),
                },
                &[],
            )
            .unwrap();

        let changes = AccountChanges::new(address)
            .with_balance_change(BalanceChange::new(index(1), U256::from(20)));
        BlockStateDiff::from_changes(&[changes]).apply(writer, None).unwrap();

        let provider = factory.database_provider_ro().unwrap();
        let merged =
            provider.tx_ref().get::<tables::HashedAccounts>(hashed_address).unwrap().unwrap();
        assert_eq!(merged.balance, U256::from(20));
        assert_eq!(merged.nonce, existing.nonce);
        assert_eq!(merged.bytecode_hash, existing.bytecode_hash);
    }

    #[test]
    fn decode_rejects_malformed_payloads() {
        assert!(
            decode_block_access_list(RawBal::new(Bytes::from_static(&[0xff, 0xff])), 1).is_err()
        );
    }

    #[test]
    fn decode_round_trips_an_encoded_list() {
        let mut changes = AccountChanges::new(Address::repeat_byte(0xee));
        changes.balance_changes.push(BalanceChange::new(index(0), U256::from(5)));
        let list = vec![changes];

        let mut encoded = Vec::new();
        alloy_rlp::encode_list(&list, &mut encoded);

        assert_eq!(
            decode_block_access_list(RawBal::new(encoded.into()), 1).unwrap().into_inner(),
            list
        );
    }
}
