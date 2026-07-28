//! Recovering from a reorg that lands while block access lists are being applied.
//!
//! Applying a BAL writes post-block values without recording what they replaced, so catch-up
//! cannot be rewound the way an executed block can. When blocks that were already applied stop
//! being canonical, the state they wrote is still in the database, and re-applying the new
//! chain's BALs only corrects the keys that chain happens to touch.
//!
//! [`AppliedChain`] closes that gap by remembering which keys each applied block wrote. On a
//! reorg it yields the keys the orphaned blocks wrote; the new chain's BALs are then applied over
//! them, and whatever they do not overwrite is state from a chain that no longer exists and has to
//! be re-read from peers.

use crate::bal::BlockStateDiff;
use alloy_primitives::{
    map::{B256Map, B256Set},
    B256,
};
use std::collections::BTreeMap;

/// The blocks whose access lists have been applied, and the keys each of them wrote.
#[derive(Debug, Default)]
pub struct AppliedChain {
    blocks: BTreeMap<u64, AppliedBlock>,
    stale: StaleKeys,
}

impl AppliedChain {
    /// Creates an empty record.
    pub fn new() -> Self {
        Self::default()
    }

    /// Remembers that `hash` was applied at `number`, and which keys its access list wrote.
    ///
    /// Anything this block rewrites stops being stale: whatever an orphaned chain left there has
    /// just been replaced by a value from the chain that survived.
    pub(crate) fn record(&mut self, number: u64, hash: B256, diff: &BlockStateDiff) {
        self.stale.clear_covered(diff);

        let accounts = diff.changed_accounts().collect();
        let storage = diff
            .changed_storage()
            .iter()
            .map(|(address, slots)| (*address, slots.keys().copied().collect()))
            .collect();

        self.blocks.insert(number, AppliedBlock { hash, accounts, storage });
    }

    /// Keys still holding values written by a chain that is no longer canonical.
    ///
    /// Empty unless a reorg happened and the new chain has not rewritten everything the old one
    /// touched. Non-empty means those keys must be re-read from peers before the state can be
    /// trusted; the final state-root check would otherwise fail.
    pub const fn stale_keys(&self) -> &StaleKeys {
        &self.stale
    }

    /// Returns the highest applied block, or `None` before any block has been applied.
    pub fn tip(&self) -> Option<(u64, B256)> {
        self.blocks.iter().next_back().map(|(number, block)| (*number, block.hash))
    }

    /// Reports whether a block at `number` with `parent_hash` extends what was applied.
    ///
    /// A parent that does not match the block recorded at `number - 1` means the chain moved out
    /// from under catch-up; the mismatching height is where recovery has to start.
    pub fn divergence(&self, number: u64, parent_hash: B256) -> Option<u64> {
        let parent_number = number.checked_sub(1)?;
        let applied = self.blocks.get(&parent_number)?;

        (applied.hash != parent_hash).then_some(parent_number)
    }

    /// Clears the stale set once its keys have been re-read, returning how many there were.
    pub fn clear_stale(&mut self) -> usize {
        let count = self.stale.accounts.len() +
            self.stale.storage.values().map(B256Set::len).sum::<usize>();
        self.stale = StaleKeys::default();
        count
    }

    /// Drops every applied block from `from_block` upward, marking the keys they wrote as stale.
    ///
    /// Catch-up then re-applies the new chain, and [`record`](Self::record) clears whatever it
    /// rewrites. What remains in [`stale_keys`](Self::stale_keys) is state the new chain never
    /// corrects.
    pub fn orphan_from(&mut self, from_block: u64) {
        for block in self.blocks.split_off(&from_block).into_values() {
            self.stale.accounts.extend(block.accounts);
            for (address, slots) in block.storage {
                self.stale.storage.entry(address).or_default().extend(slots);
            }
        }
    }
}

/// Keys left holding values written by blocks that are no longer canonical.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StaleKeys {
    /// Hashed addresses whose account fields need re-reading.
    accounts: B256Set,
    /// Hashed slots needing re-reading, keyed by hashed address.
    storage: B256Map<B256Set>,
}

impl StaleKeys {
    /// Drops the keys `diff` rewrites, since the new chain's value for them is authoritative.
    pub(crate) fn clear_covered(&mut self, diff: &BlockStateDiff) {
        for address in diff.changed_accounts() {
            self.accounts.remove(&address);
        }

        for (address, slots) in diff.changed_storage() {
            let Some(stale_slots) = self.storage.get_mut(address) else { continue };
            for slot in slots.keys() {
                stale_slots.remove(slot);
            }
            if stale_slots.is_empty() {
                self.storage.remove(address);
            }
        }
    }

    /// Returns `true` when the new chain corrected everything the orphaned one wrote.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.storage.is_empty()
    }

    /// Hashed addresses that still need re-reading from peers.
    pub fn accounts(&self) -> impl ExactSizeIterator<Item = B256> + '_ {
        self.accounts.iter().copied()
    }

    /// Hashed slots that still need re-reading, keyed by hashed address.
    pub const fn storage(&self) -> &B256Map<B256Set> {
        &self.storage
    }
}

/// One applied block and the keys its access list wrote.
#[derive(Debug)]
struct AppliedBlock {
    /// Hash of the block whose access list was applied.
    hash: B256,
    /// Hashed addresses whose account fields it wrote.
    accounts: B256Set,
    /// Hashed slots it wrote, keyed by hashed address.
    storage: B256Map<B256Set>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eip7928::{
        AccountChanges, BalanceChange, BlockAccessIndex, SlotChanges, StorageChange,
    };
    use alloy_primitives::{keccak256, Address, U256};

    fn address(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    fn hashed(byte: u8) -> B256 {
        keccak256(address(byte))
    }

    fn hashed_slot(slot: u64) -> B256 {
        keccak256(B256::from(U256::from(slot)))
    }

    /// A diff touching one account's balance and, optionally, some of its storage slots.
    fn diff(account: u8, slots: &[u64]) -> BlockStateDiff {
        let mut changes = AccountChanges::new(address(account));
        changes
            .balance_changes
            .push(BalanceChange::new(BlockAccessIndex::PRE_EXECUTION, U256::from(1)));
        for slot in slots {
            changes.storage_changes.push(SlotChanges::new(
                U256::from(*slot),
                vec![StorageChange::new(BlockAccessIndex::PRE_EXECUTION, U256::from(*slot))],
            ));
        }

        BlockStateDiff::from_changes(&[changes])
    }

    #[test]
    fn matching_parent_is_not_a_divergence() {
        let mut chain = AppliedChain::new();
        chain.record(10, B256::repeat_byte(0xaa), &diff(1, &[]));

        assert_eq!(chain.divergence(11, B256::repeat_byte(0xaa)), None);
    }

    #[test]
    fn mismatched_parent_points_at_the_fork_height() {
        let mut chain = AppliedChain::new();
        chain.record(10, B256::repeat_byte(0xaa), &diff(1, &[]));

        assert_eq!(chain.divergence(11, B256::repeat_byte(0xbb)), Some(10));
    }

    #[test]
    fn unapplied_heights_report_no_divergence() {
        let chain = AppliedChain::new();

        // Nothing was applied at height 9, so there is no claim to contradict.
        assert_eq!(chain.divergence(10, B256::repeat_byte(0xbb)), None);
        // Genesis has no parent to compare against.
        assert_eq!(chain.divergence(0, B256::repeat_byte(0xbb)), None);
    }

    #[test]
    fn tip_follows_the_highest_applied_block() {
        let mut chain = AppliedChain::new();
        assert_eq!(chain.tip(), None);

        chain.record(10, B256::repeat_byte(0xaa), &diff(1, &[]));
        chain.record(11, B256::repeat_byte(0xbb), &diff(2, &[]));

        assert_eq!(chain.tip(), Some((11, B256::repeat_byte(0xbb))));
    }

    #[test]
    fn nothing_is_stale_before_a_reorg() {
        let mut chain = AppliedChain::new();
        chain.record(10, B256::repeat_byte(0xa0), &diff(1, &[1]));

        assert!(chain.stale_keys().is_empty());
    }

    #[test]
    fn orphaned_keys_union_every_dropped_block() {
        let mut chain = AppliedChain::new();
        chain.record(10, B256::repeat_byte(0xa0), &diff(1, &[1]));
        chain.record(11, B256::repeat_byte(0xa1), &diff(2, &[2]));
        chain.record(12, B256::repeat_byte(0xa2), &diff(3, &[3]));

        chain.orphan_from(11);

        // Block 10 stays canonical, so its keys are not stale.
        let stale = chain.stale_keys();
        assert_eq!(
            stale.accounts().collect::<B256Set>(),
            B256Set::from_iter([hashed(2), hashed(3)])
        );
        assert_eq!(stale.storage().len(), 2);
        assert_eq!(chain.tip(), Some((10, B256::repeat_byte(0xa0))));
    }

    #[test]
    fn keys_the_new_chain_rewrites_are_not_stale() {
        let mut chain = AppliedChain::new();
        chain.record(11, B256::repeat_byte(0xa1), &diff(2, &[2]));
        chain.orphan_from(11);

        // The new chain writes the same account and slot, so its value is authoritative.
        chain.record(11, B256::repeat_byte(0xb1), &diff(2, &[2]));

        assert!(chain.stale_keys().is_empty());
    }

    #[test]
    fn keys_the_new_chain_misses_stay_stale() {
        let mut chain = AppliedChain::new();
        chain.record(11, B256::repeat_byte(0xa1), &diff(2, &[2, 3]));
        chain.orphan_from(11);

        // The new chain touches a different account entirely.
        chain.record(11, B256::repeat_byte(0xb1), &diff(9, &[2, 3]));

        let stale = chain.stale_keys();
        assert!(!stale.is_empty());
        assert_eq!(stale.accounts().collect::<Vec<_>>(), vec![hashed(2)]);
        assert_eq!(
            stale.storage()[&hashed(2)],
            B256Set::from_iter([hashed_slot(2), hashed_slot(3)])
        );
    }

    #[test]
    fn partially_rewritten_storage_keeps_only_the_untouched_slots() {
        let mut chain = AppliedChain::new();
        chain.record(11, B256::repeat_byte(0xa1), &diff(2, &[2, 3]));
        chain.orphan_from(11);

        // The new chain rewrites slot 2 but never touches slot 3.
        chain.record(11, B256::repeat_byte(0xb1), &diff(2, &[2]));

        let stale = chain.stale_keys();
        assert_eq!(stale.accounts().len(), 0);
        assert_eq!(stale.storage()[&hashed(2)], B256Set::from_iter([hashed_slot(3)]));
    }
}
