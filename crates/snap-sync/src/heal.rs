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
use alloy_eip7928::AccountChanges;
use alloy_primitives::{keccak256, map::B256Map, Bytes, B256, KECCAK256_EMPTY, U256};
use alloy_rlp::Decodable;
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_primitives_traits::Account;
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::{HashedPostState, HashedStorage};

/// The state changes one block's access list commits to, in hashed-key form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BlockStateDiff {
    /// Per-account field changes, keyed by `keccak256(address)`.
    accounts: Vec<BalAccountDiff>,
    /// Post-block slot values, keyed by hashed address then hashed slot.
    storage: B256Map<B256Map<U256>>,
    /// `(code hash, code)` pairs for contracts deployed in this block.
    bytecodes: Vec<(B256, Bytes)>,
}

impl BlockStateDiff {
    /// Builds the diff for a block from its decoded access list.
    ///
    /// The post-block value of a field is its change with the highest block access index; entries
    /// carry that index explicitly, so this does not rely on the peer having sorted them.
    pub(crate) fn from_changes(changes: &[AccountChanges]) -> Self {
        let mut diff = Self::default();

        for account in changes {
            let hashed_address = keccak256(account.address);

            let balance = account
                .balance_changes
                .iter()
                .max_by_key(|change| change.block_access_index)
                .map(|change| change.post_balance);
            let nonce = account
                .nonce_changes
                .iter()
                .max_by_key(|change| change.block_access_index)
                .map(|change| change.new_nonce);
            let bytecode_hash =
                account.code_changes.iter().max_by_key(|change| change.block_access_index).map(
                    |change| {
                        if change.new_code.is_empty() {
                            return None
                        }
                        let code_hash = keccak256(&change.new_code);
                        diff.bytecodes.push((code_hash, change.new_code.clone()));
                        Some(code_hash)
                    },
                );

            for slot in &account.storage_changes {
                if let Some(change) =
                    slot.changes.iter().max_by_key(|change| change.block_access_index)
                {
                    diff.storage
                        .entry(hashed_address)
                        .or_default()
                        .insert(keccak256(B256::from(slot.slot)), change.new_value);
                }
            }

            // Accounts that were only read appear in the list with no changes at all.
            if balance.is_some() || nonce.is_some() || bytecode_hash.is_some() {
                diff.accounts.push(BalAccountDiff {
                    hashed_address,
                    balance,
                    nonce,
                    bytecode_hash,
                });
            }
        }

        diff
    }

    /// Merges this diff onto the state already in the database and writes the result.
    pub(crate) fn apply<F>(&self, writer: SnapStateWriter<'_, F>) -> Result<(), SnapSyncError>
    where
        F: DatabaseProviderFactory,
        F::Provider: DBProvider,
        F::ProviderRW: DBProvider + StateWriter,
        <F::Provider as DBProvider>::Tx: DbTx,
        <F::ProviderRW as DBProvider>::Tx: DbTxMut,
    {
        let mut accounts = B256Map::default();
        for diff in &self.accounts {
            let existing = writer.read_account(diff.hashed_address)?;
            accounts.insert(diff.hashed_address, Some(diff.merge_onto(existing.as_ref())));
        }

        let storages = self
            .storage
            .iter()
            .map(|(address, slots)| {
                (*address, HashedStorage::from_iter(false, slots.iter().map(|(k, v)| (*k, *v))))
            })
            .collect();

        writer.write_state(HashedPostState { accounts, storages })?;
        if !self.bytecodes.is_empty() {
            writer.write_bytecodes(&self.bytecodes)?;
        }

        Ok(())
    }
}

/// Decodes the raw RLP payload of a block access list.
pub(crate) fn decode_block_access_list(
    bal: &Bytes,
    block_number: u64,
) -> Result<Vec<AccountChanges>, SnapSyncError> {
    Vec::<AccountChanges>::decode(&mut bal.as_ref()).map_err(|err| {
        SnapSyncError::RlpDecode(format!("block access list for block {block_number}: {err}"))
    })
}

/// One account's field changes within a block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BalAccountDiff {
    /// `keccak256(address)`.
    hashed_address: B256,
    /// Post-block balance, when the block changed it.
    balance: Option<U256>,
    /// Post-block nonce, when the block changed it.
    nonce: Option<u64>,
    /// Post-block code hash, when the block changed it. The inner `None` means code was cleared.
    bytecode_hash: Option<Option<B256>>,
}

impl BalAccountDiff {
    /// Applies the changed fields on top of the account currently in the database.
    ///
    /// A field the block did not touch keeps its stored value, which is why this cannot be a plain
    /// overwrite: a BAL entry that only changes a balance says nothing about the nonce.
    fn merge_onto(&self, existing: Option<&Account>) -> Account {
        Account {
            balance: self
                .balance
                .or_else(|| existing.map(|account| account.balance))
                .unwrap_or_default(),
            nonce: self.nonce.or_else(|| existing.map(|account| account.nonce)).unwrap_or_default(),
            bytecode_hash: match self.bytecode_hash {
                // The database stores "no code" as `None`, so normalise the empty-code hash.
                Some(Some(hash)) if hash != KECCAK256_EMPTY => Some(hash),
                Some(_) => None,
                None => existing.and_then(|account| account.bytecode_hash),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eip7928::{
        BalanceChange, BlockAccessIndex, CodeChange, NonceChange, SlotChanges, StorageChange,
    };
    use alloy_primitives::Address;

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
        assert_eq!(diff.accounts[0].balance, Some(U256::from(30)));
        assert_eq!(diff.accounts[0].nonce, Some(7));
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
        assert_eq!(diff.accounts[0].bytecode_hash, Some(Some(keccak256(&code))));
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
    fn untouched_fields_keep_their_stored_values() {
        let existing =
            Account { nonce: 4, balance: U256::from(9), bytecode_hash: Some(B256::repeat_byte(1)) };
        let diff = BalAccountDiff {
            hashed_address: B256::ZERO,
            balance: Some(U256::from(99)),
            nonce: None,
            bytecode_hash: None,
        };

        let merged = diff.merge_onto(Some(&existing));

        assert_eq!(merged.balance, U256::from(99));
        assert_eq!(merged.nonce, 4);
        assert_eq!(merged.bytecode_hash, existing.bytecode_hash);
    }

    #[test]
    fn new_accounts_default_their_untouched_fields() {
        let diff = BalAccountDiff {
            hashed_address: B256::ZERO,
            balance: Some(U256::from(1)),
            nonce: None,
            bytecode_hash: None,
        };

        let merged = diff.merge_onto(None);

        assert_eq!(merged.nonce, 0);
        assert_eq!(merged.bytecode_hash, None);
    }

    #[test]
    fn cleared_code_is_stored_as_no_code() {
        let existing =
            Account { nonce: 1, balance: U256::ZERO, bytecode_hash: Some(B256::repeat_byte(2)) };
        let diff = BalAccountDiff {
            hashed_address: B256::ZERO,
            balance: None,
            nonce: None,
            bytecode_hash: Some(None),
        };

        assert_eq!(diff.merge_onto(Some(&existing)).bytecode_hash, None);
    }

    #[test]
    fn empty_code_hash_normalises_to_no_code() {
        let diff = BalAccountDiff {
            hashed_address: B256::ZERO,
            balance: None,
            nonce: None,
            bytecode_hash: Some(Some(KECCAK256_EMPTY)),
        };

        assert_eq!(diff.merge_onto(None).bytecode_hash, None);
    }

    #[test]
    fn decode_rejects_malformed_payloads() {
        assert!(decode_block_access_list(&Bytes::from_static(&[0xff, 0xff]), 1).is_err());
    }

    #[test]
    fn decode_round_trips_an_encoded_list() {
        let mut changes = AccountChanges::new(Address::repeat_byte(0xee));
        changes.balance_changes.push(BalanceChange::new(index(0), U256::from(5)));
        let list = vec![changes];

        let mut encoded = Vec::new();
        alloy_rlp::encode_list(&list, &mut encoded);

        assert_eq!(decode_block_access_list(&encoded.into(), 1).unwrap(), list);
    }
}
