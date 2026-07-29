//! The writer boundary: session reset, write modes, and finalization.

use crate::error::SnapSyncError;
use alloy_primitives::{Bytes, B256};
use reth_db_api::{
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter, StorageSettingsCache, TrieWriter};
use reth_trie::{HashedPostState, StateRoot};
use reth_trie_db::DatabaseStateRoot;

/// Persists verified snap state to the database.
///
/// Each write commits on its own: a batch is only durable once it has been checked against the
/// pivot root, so a download interrupted mid-range leaves behind verified state rather than a
/// partially written range.
#[derive(Debug)]
pub struct SnapStateWriter<'a, F> {
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
    pub const fn new(factory: &'a F) -> Self {
        Self { factory }
    }

    /// Clears the hashed state and trie tables so a session starts from a clean generation.
    ///
    /// Without this a session inherits whatever was there — a genesis allocation, or the partial
    /// state of an attempt that failed — and the final root check cannot tell the difference
    /// between that and downloaded state.
    pub fn reset(&self) -> Result<(), SnapSyncError> {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        {
            let tx = provider.tx_ref();
            tx.clear::<tables::HashedAccounts>().map_err(db_err)?;
            tx.clear::<tables::HashedStorages>().map_err(db_err)?;
            tx.clear::<tables::AccountsTrie>().map_err(db_err)?;
            tx.clear::<tables::StoragesTrie>().map_err(db_err)?;
        }
        provider.commit().map_err(db_err)?;
        Ok(())
    }

    /// Writes hashed state and the bytecodes it references in a single transaction.
    ///
    /// One transaction is what makes a downloaded batch all-or-nothing: an account is never
    /// durable without the storage and code it commits to, so an interrupted download leaves a
    /// shorter prefix rather than an inconsistent one.
    pub fn commit_batch(
        &self,
        state: HashedPostState,
        codes: &[(B256, Bytes)],
    ) -> Result<(), SnapSyncError> {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;

        if !state.is_empty() {
            provider.write_hashed_state(&state.into_sorted()).map_err(db_err)?;
        }
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

    /// Writes hashed accounts and storage slots.
    pub fn write_state(&self, state: HashedPostState) -> Result<(), SnapSyncError> {
        if state.is_empty() {
            return Ok(())
        }

        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        provider.write_hashed_state(&state.into_sorted()).map_err(db_err)?;
        provider.commit().map_err(db_err)?;
        Ok(())
    }

    /// Writes contract bytecodes, skipping empty code.
    pub fn write_bytecodes(&self, codes: &[(B256, Bytes)]) -> Result<(), SnapSyncError> {
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
    pub fn read_account(&self, hashed_address: B256) -> Result<Option<Account>, SnapSyncError> {
        let provider = self.factory.database_provider_ro().map_err(db_err)?;
        provider.tx_ref().get::<tables::HashedAccounts>(hashed_address).map_err(db_err)
    }
}

impl<F> SnapStateWriter<'_, F>
where
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + TrieWriter + StorageSettingsCache,
    <F::ProviderRW as DBProvider>::Tx: DbTx + DbTxMut,
{
    /// Rebuilds the state trie over the downloaded hashed state and checks its root against
    /// `expected`, persisting the trie tables only if they match.
    ///
    /// This is the end-to-end check on everything snap sync assembled: the per-range proofs only
    /// prove each response against the root it was served at, so nothing before this point rules
    /// out gaps between ranges served at different pivots, or a block access list applied wrongly.
    ///
    /// The same pass produces the intermediate trie nodes, which are written on success because
    /// the node cannot serve proofs or extend the chain from hashed state alone. Walking the whole
    /// trie is proportional to total state size, so this runs once at the end of a sync.
    pub fn finalize_sync(&self, block_number: u64, expected: B256) -> Result<(), SnapSyncError> {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;

        let (computed, updates) = reth_trie_db::with_adapter!(provider, |A| {
            DbStateRoot::<_, A>::from_tx(provider.tx_ref()).root_with_updates()
        })
        .map_err(|err| SnapSyncError::Database(format!("state root computation: {err}")))?;

        if computed != expected {
            // Dropping the provider without committing leaves the trie tables untouched, so a
            // retry at a later pivot starts from the hashed state rather than a half-built trie.
            return Err(SnapSyncError::StateRootMismatch { block: block_number, expected, computed })
        }

        provider.write_trie_updates(updates).map_err(db_err)?;
        provider.commit().map_err(db_err)?;
        Ok(())
    }
}

/// State root calculator over the database's hashed-state tables.
type DbStateRoot<'a, TX, A> = StateRoot<
    reth_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    reth_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;

fn db_err(err: impl core::fmt::Display) -> SnapSyncError {
    SnapSyncError::Database(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{map::B256Map, U256};
    use reth_db_api::cursor::DbCursorRO;
    use reth_provider::test_utils::create_test_provider_factory;
    use reth_trie::{test_utils::state_root_prehashed, HashedStorage};

    fn b256(value: u64) -> B256 {
        B256::left_padding_from(&value.to_be_bytes())
    }

    fn account(nonce: u64) -> Account {
        Account { nonce, balance: U256::from(nonce), bytecode_hash: None }
    }

    /// Hashed address of the account holding storage in the fixture.
    fn storage_owner() -> B256 {
        hashed_address(0)
    }

    /// Spreads accounts across the trie the way real hashed addresses do, so the rebuilt trie has
    /// intermediate branch nodes rather than collapsing to a single root node.
    fn hashed_address(index: u64) -> B256 {
        alloy_primitives::keccak256(index.to_be_bytes())
    }

    /// A trie-sized set of accounts, one of them with storage, plus the state root they hash to.
    fn fixture() -> (HashedPostState, B256) {
        let slots = [(b256(0x10), U256::from(1)), (b256(0x11), U256::from(2))];

        let accounts: Vec<(B256, Account)> =
            (0..64).map(|i| (hashed_address(i), account(i + 1))).collect();

        let state = HashedPostState {
            accounts: accounts.iter().map(|(hash, account)| (*hash, Some(*account))).collect(),
            storages: B256Map::from_iter([(
                storage_owner(),
                HashedStorage::from_iter(false, slots),
            )]),
        };

        let root = state_root_prehashed(accounts.iter().map(|(hash, account)| {
            let storage = if *hash == storage_owner() { slots.to_vec() } else { Vec::new() };
            (*hash, (*account, storage))
        }));

        (state, root)
    }

    fn trie_is_empty(factory: &impl DatabaseProviderFactory<Provider: DBProvider>) -> bool {
        let provider = factory.database_provider_ro().unwrap();
        let mut cursor = provider.tx_ref().cursor_read::<tables::AccountsTrie>().unwrap();
        cursor.first().unwrap().is_none()
    }

    #[test]
    fn matching_root_persists_the_trie_tables() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.write_state(state).unwrap();

        writer.finalize_sync(100, root).unwrap();

        // The node cannot serve proofs from hashed state alone, so the rebuilt nodes must land.
        assert!(!trie_is_empty(&factory));
    }

    #[test]
    fn mismatched_root_reports_both_roots_and_writes_nothing() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.write_state(state).unwrap();

        let err = writer.finalize_sync(100, b256(0xdead)).unwrap_err();

        match err {
            SnapSyncError::StateRootMismatch { block, expected, computed } => {
                assert_eq!(block, 100);
                assert_eq!(expected, b256(0xdead));
                assert_eq!(computed, root);
            }
            other => panic!("expected a state root mismatch, got {other:?}"),
        }
        // A rejected sync must not leave a half-built trie behind for the next attempt.
        assert!(trie_is_empty(&factory));
    }

    #[test]
    fn missing_state_does_not_pass_as_a_matching_root() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);

        // Drop one account, as a peer withholding a range would.
        let mut partial = state;
        partial.accounts.remove(&hashed_address(7));
        writer.write_state(partial).unwrap();

        assert!(matches!(
            writer.finalize_sync(100, root),
            Err(SnapSyncError::StateRootMismatch { .. })
        ));
    }
}
