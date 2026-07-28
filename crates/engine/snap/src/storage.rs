//! Database write helpers for downloaded hashed state and bytecodes.

use crate::SnapSyncError;
use alloy_primitives::{map::B256Map, Bytes, B256, U256};
use reth_db_api::{tables, transaction::DbTxMut};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::{HashedPostStateSorted, HashedStorageSorted};

/// Writes a batch of hashed accounts.
pub(crate) fn write_hashed_accounts<F>(
    factory: &F,
    accounts: &[(B256, Account)],
) -> Result<(), SnapSyncError>
where
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let mut sorted: Vec<_> =
        accounts.iter().map(|(hash, account)| (*hash, Some(*account))).collect();
    sorted.sort_unstable_by_key(|(hash, _)| *hash);

    write_hashed_state(factory, HashedPostStateSorted::new(sorted, B256Map::default()))
}

/// Writes a batch of hashed storage slots, keyed by hashed account.
pub(crate) fn write_hashed_storages<F>(
    factory: &F,
    entries: &[(B256, B256, U256)],
) -> Result<(), SnapSyncError>
where
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
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

    write_hashed_state(factory, HashedPostStateSorted::new(Vec::new(), storages))
}

/// Writes a batch of contract bytecodes, skipping empty code.
pub(crate) fn write_bytecodes<F>(factory: &F, codes: &[(B256, Bytes)]) -> Result<(), SnapSyncError>
where
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let provider = factory.database_provider_rw().map_err(db_err)?;
    {
        let tx = provider.tx_ref();
        for (hash, code) in codes.iter().filter(|(_, code)| !code.is_empty()) {
            tx.put::<tables::Bytecodes>(*hash, Bytecode::new_raw(code.clone())).map_err(db_err)?;
        }
    }
    provider.commit().map_err(db_err)?;
    Ok(())
}

fn write_hashed_state<F>(
    factory: &F,
    hashed_state: HashedPostStateSorted,
) -> Result<(), SnapSyncError>
where
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let provider = factory.database_provider_rw().map_err(db_err)?;
    provider.write_hashed_state(&hashed_state).map_err(db_err)?;
    provider.commit().map_err(db_err)?;
    Ok(())
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

pub(crate) fn db_err(err: impl core::fmt::Display) -> SnapSyncError {
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
