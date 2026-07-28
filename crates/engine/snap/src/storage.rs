//! Database writes for downloaded hashed state and bytecodes.

use crate::SnapSyncError;
use alloy_primitives::{Bytes, B256};
use reth_db_api::{
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::HashedPostState;

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

    /// Writes hashed accounts and storage slots.
    pub(crate) fn write_state(&self, state: HashedPostState) -> Result<(), SnapSyncError> {
        if state.is_empty() {
            return Ok(())
        }

        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        provider.write_hashed_state(&state.into_sorted()).map_err(db_err)?;
        provider.commit().map_err(db_err)?;
        Ok(())
    }

    /// Writes contract bytecodes, skipping empty code.
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
        provider.tx_ref().get::<tables::HashedAccounts>(hashed_address).map_err(db_err)
    }
}

fn db_err(err: impl core::fmt::Display) -> SnapSyncError {
    SnapSyncError::Database(err.to_string())
}
