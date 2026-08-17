//! Post-block state values committed to by an EIP-7928 block access list entry.
//!
//! A list entry only carries the fields its block changed, so `None` means untouched rather than
//! zero, and consuming these values means merging them onto the account state that came before
//! the block. This is the shared reading of an entry; how the result is used — streamed into a
//! state-root job, or written to hashed tables — stays with the caller.
//!
//! Post-block values are taken by highest block access index rather than by position. Alloy's
//! `AccountChanges::storage_post_states` reads the last entry instead, which is the same answer
//! only for a list in canonical EIP-7928 order. Not depending on that ordering means these
//! readers are also correct for a list that has not been checked against a header commitment.

use alloc::vec::Vec;
use alloy_eip7928::AccountChanges;
use alloy_primitives::{keccak256, Bytes, B256, KECCAK256_EMPTY, U256};
use reth_primitives_traits::Account;

/// The post-block account-level values one block access list entry commits to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BalAccountState {
    /// Post-block balance, when the block changed it.
    pub balance: Option<U256>,
    /// Post-block nonce, when the block changed it.
    pub nonce: Option<u64>,
    /// Post-block code hash, when the block changed the code.
    ///
    /// The inner `None` means the code was removed or set empty.
    pub code_hash: Option<Option<B256>>,
}

impl BalAccountState {
    /// Extracts the post-block value of every changed account-level field.
    ///
    /// The post-block value of a field is its change with the highest block access index; entries
    /// carry that index explicitly, so this does not rely on the list being sorted.
    pub fn from_changes(changes: &AccountChanges) -> Self {
        Self {
            balance: changes
                .balance_changes
                .iter()
                .max_by_key(|change| change.block_access_index)
                .map(|change| change.post_balance),
            nonce: changes
                .nonce_changes
                .iter()
                .max_by_key(|change| change.block_access_index)
                .map(|change| change.new_nonce),
            code_hash: changes
                .code_changes
                .iter()
                .max_by_key(|change| change.block_access_index)
                .map(|change| (!change.new_code.is_empty()).then(|| keccak256(&change.new_code))),
        }
    }

    /// Returns `true` when the entry changed no account-level field.
    ///
    /// Accounts that were only read appear in a list with no changes at all; such an entry says
    /// nothing about the account and must not overwrite it.
    pub const fn is_empty(&self) -> bool {
        self.balance.is_none() && self.nonce.is_none() && self.code_hash.is_none()
    }

    /// Returns `true` when merging needs the pre-block account.
    ///
    /// A field the block did not touch keeps its previous value, which only the pre-block account
    /// can supply.
    pub const fn needs_parent_account(&self) -> bool {
        self.balance.is_none() || self.nonce.is_none() || self.code_hash.is_none()
    }

    /// Applies the changed fields on top of `existing`, the account before the block.
    ///
    /// This cannot be a plain overwrite: an entry that only changes a balance says nothing about
    /// the nonce. Follows the database convention of `None` for "no code", so the empty-code hash
    /// normalises away.
    pub fn merge_onto(&self, existing: Option<&Account>) -> Account {
        Account {
            balance: self
                .balance
                .or_else(|| existing.map(|account| account.balance))
                .unwrap_or_default(),
            nonce: self.nonce.or_else(|| existing.map(|account| account.nonce)).unwrap_or_default(),
            bytecode_hash: match self.code_hash {
                Some(Some(hash)) if hash != KECCAK256_EMPTY => Some(hash),
                Some(_) => None,
                None => existing.and_then(|account| account.bytecode_hash),
            },
        }
    }
}

/// Returns `(hashed slot, post-block value)` for every slot the entry changed.
///
/// The post-block value of a slot is its change with the highest block access index, as with
/// account fields.
pub fn hashed_storage_changes(changes: &AccountChanges) -> Vec<(B256, U256)> {
    changes
        .storage_changes
        .iter()
        .filter_map(|slot| {
            slot.changes
                .iter()
                .max_by_key(|change| change.block_access_index)
                .map(|change| (keccak256(B256::from(slot.slot)), change.new_value))
        })
        .collect()
}

/// Returns the code the entry deployed, keyed by its hash.
///
/// `None` when the block did not change the code, or removed it. The hash matches what
/// [`BalAccountState::from_changes`] puts in `code_hash`, so an account is never left pointing at
/// code this did not return.
pub fn deployed_bytecode(changes: &AccountChanges) -> Option<(B256, Bytes)> {
    changes
        .code_changes
        .iter()
        .max_by_key(|change| change.block_access_index)
        .filter(|change| !change.new_code.is_empty())
        .map(|change| (keccak256(&change.new_code), change.new_code.clone()))
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
        let mut changes = AccountChanges::new(Address::repeat_byte(0xaa));
        // Deliberately out of order: the index decides, not the position.
        changes.balance_changes.push(BalanceChange::new(index(3), U256::from(30)));
        changes.balance_changes.push(BalanceChange::new(index(1), U256::from(10)));
        changes.nonce_changes.push(NonceChange::new(index(2), 7));
        changes.nonce_changes.push(NonceChange::new(index(1), 5));

        let state = BalAccountState::from_changes(&changes);

        assert_eq!(state.balance, Some(U256::from(30)));
        assert_eq!(state.nonce, Some(7));
    }

    #[test]
    fn storage_slots_are_hashed_and_take_the_final_value() {
        let slot = U256::from(1);
        let mut changes = AccountChanges::new(Address::repeat_byte(0xbb));
        changes.storage_changes.push(SlotChanges::new(
            slot,
            vec![
                StorageChange::new(index(1), U256::from(11)),
                StorageChange::new(index(4), U256::from(44)),
            ],
        ));

        let hashed = hashed_storage_changes(&changes);

        assert_eq!(hashed, vec![(keccak256(B256::from(slot)), U256::from(44))]);
    }

    #[test]
    fn deployed_code_matches_the_extracted_code_hash() {
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let mut changes = AccountChanges::new(Address::repeat_byte(0xcc));
        changes.code_changes.push(CodeChange::new(index(1), code.clone()));

        let state = BalAccountState::from_changes(&changes);
        let deployed = deployed_bytecode(&changes).unwrap();

        assert_eq!(state.code_hash, Some(Some(deployed.0)));
        assert_eq!(deployed, (keccak256(&code), code));
    }

    #[test]
    fn read_only_entries_are_empty() {
        let mut changes = AccountChanges::new(Address::repeat_byte(0xdd));
        changes.storage_reads.push(U256::from(1));

        assert!(BalAccountState::from_changes(&changes).is_empty());
        assert!(hashed_storage_changes(&changes).is_empty());
        assert!(deployed_bytecode(&changes).is_none());
    }

    #[test]
    fn untouched_fields_keep_their_stored_values() {
        let existing =
            Account { nonce: 4, balance: U256::from(9), bytecode_hash: Some(B256::repeat_byte(1)) };
        let state = BalAccountState { balance: Some(U256::from(99)), nonce: None, code_hash: None };

        let merged = state.merge_onto(Some(&existing));

        assert_eq!(merged.balance, U256::from(99));
        assert_eq!(merged.nonce, 4);
        assert_eq!(merged.bytecode_hash, existing.bytecode_hash);
    }

    #[test]
    fn new_accounts_default_their_untouched_fields() {
        let state = BalAccountState { balance: Some(U256::from(1)), nonce: None, code_hash: None };

        let merged = state.merge_onto(None);

        assert_eq!(merged.nonce, 0);
        assert_eq!(merged.bytecode_hash, None);
    }

    #[test]
    fn cleared_and_empty_code_normalise_to_no_code() {
        let existing =
            Account { nonce: 1, balance: U256::ZERO, bytecode_hash: Some(B256::repeat_byte(2)) };
        let cleared = BalAccountState { balance: None, nonce: None, code_hash: Some(None) };

        assert_eq!(cleared.merge_onto(Some(&existing)).bytecode_hash, None);

        let empty_hash =
            BalAccountState { balance: None, nonce: None, code_hash: Some(Some(KECCAK256_EMPTY)) };
        assert_eq!(empty_hash.merge_onto(None).bytecode_hash, None);
    }
}
