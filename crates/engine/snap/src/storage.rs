//! Database writes for downloaded hashed state and bytecodes.

use crate::SnapSyncError;
use alloy_primitives::{map::B256Map, Bytes, B256, U256};
use reth_db_api::{
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::{HashedPostStateSorted, HashedStorageSorted};

/// Persists verified snap state to the database.
///
/// Each write commits on its own: a batch is only durable once it has been checked against the
/// pivot root, so a download interrupted mid-range leaves behind verified state rather than a
/// partially written range.
#[derive(Debug)]
pub(crate) struct SnapStateWriter<'a, F> {
    factory: &'a F,
}

// Hand-written so the writer stays copyable regardless of whether `F` is: deriving would bound
// `Clone`/`Copy` on `F` even though the struct only holds a reference to it.
impl<F> Clone for SnapStateWriter<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<F> Copy for SnapStateWriter<'_, F> {}

impl<'a, F> SnapStateWriter<'a, F>
where
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    /// Creates a writer over the given provider factory.
    pub(crate) const fn new(factory: &'a F) -> Self {
        Self { factory }
    }

    /// Writes a batch of hashed accounts.
    pub(crate) fn write_accounts(&self, accounts: &[(B256, Account)]) -> Result<(), SnapSyncError> {
        let mut sorted: Vec<_> =
            accounts.iter().map(|(hash, account)| (*hash, Some(*account))).collect();
        sorted.sort_unstable_by_key(|(hash, _)| *hash);

        self.write_hashed_state(HashedPostStateSorted::new(sorted, B256Map::default()))
    }

    /// Writes a batch of hashed storage slots, keyed by hashed account.
    pub(crate) fn write_storages(
        &self,
        entries: &[(B256, B256, U256)],
    ) -> Result<(), SnapSyncError> {
        let mut slots_by_account: B256Map<Vec<(B256, U256)>> = B256Map::default();
        for &(account_hash, slot_hash, value) in entries {
            slots_by_account.entry(account_hash).or_default().push((slot_hash, value));
        }

        let storages = slots_by_account
            .into_iter()
            .map(|(account_hash, mut storage_slots)| {
                storage_slots.sort_unstable_by_key(|(slot_hash, _)| *slot_hash);
                (account_hash, HashedStorageSorted { storage_slots, wiped: false })
            })
            .collect();

        self.write_hashed_state(HashedPostStateSorted::new(Vec::new(), storages))
    }

    /// Writes a batch of contract bytecodes, skipping empty code.
    pub(crate) fn write_bytecodes(&self, codes: &[(B256, Bytes)]) -> Result<(), SnapSyncError> {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        {
            let tx = provider.tx_ref();
            for (hash, code) in codes.iter().filter(|(_, code)| !code.is_empty()) {
                tx.put::<tables::Bytecodes>(*hash, Bytecode::new_raw(code.clone()))
                    .map_err(db_err)?;
            }
        }
        provider.commit().map_err(db_err)?;
        Ok(())
    }

    fn write_hashed_state(&self, hashed_state: HashedPostStateSorted) -> Result<(), SnapSyncError> {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        provider.write_hashed_state(&hashed_state).map_err(db_err)?;
        provider.commit().map_err(db_err)?;
        Ok(())
    }
}

impl<F> SnapStateWriter<'_, F>
where
    F: DatabaseProviderFactory,
    F::Provider: DBProvider,
    <F::Provider as DBProvider>::Tx: DbTx,
{
    /// Reads a hashed account, used to merge partial block access list changes onto stored state.
    pub(crate) fn read_account(
        &self,
        hashed_address: B256,
    ) -> Result<Option<Account>, SnapSyncError> {
        let provider = self.factory.database_provider_ro().map_err(db_err)?;
        let account = provider.tx_ref().get::<tables::HashedAccounts>(hashed_address);
        account.map_err(db_err)
    }
}

/// Returns the next hash after `hash`, used to resume a range past an already-received key.
///
/// Wraps to zero at `0xff..ff`; callers detect that boundary before paginating further.
pub(crate) fn increment_b256(hash: B256) -> B256 {
    let mut bytes = hash.0;
    for byte in bytes.iter_mut().rev() {
        if *byte == 0xff {
            *byte = 0;
        } else {
            *byte += 1;
            return B256::from(bytes)
        }
    }
    B256::ZERO
}

fn db_err(err: impl core::fmt::Display) -> SnapSyncError {
    SnapSyncError::Database(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_steps_to_next_hash() {
        assert_eq!(increment_b256(B256::ZERO), B256::left_padding_from(&[1]));
    }

    #[test]
    fn increment_carries_across_bytes() {
        let mut bytes = [0u8; 32];
        bytes[31] = 0xff;
        let mut expected = [0u8; 32];
        expected[30] = 1;

        assert_eq!(increment_b256(B256::from(bytes)), B256::from(expected));
    }

    #[test]
    fn increment_wraps_at_max() {
        assert_eq!(increment_b256(B256::repeat_byte(0xff)), B256::ZERO);
    }
}
