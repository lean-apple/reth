//! The writer boundary: generation lifecycle, write modes, and finalization.

use crate::error::SnapSyncError;
use alloy_primitives::{Bytes, B256};
use reth_db_api::{
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::DatabaseProviderFactory;
use reth_stages_types::{StageCheckpoint, StageId};
use reth_storage_api::{DBProvider, StateWriter, StorageSettingsCache, TrieWriter};
use reth_trie::{HashedPostState, StateRoot, StateRootProgress};
use reth_trie_db::DatabaseStateRoot;

/// Stage slot marking a snap sync generation whose state root has not been checked yet.
///
/// A generation starts by wiping the hashed state, so a crash part-way leaves tables that look
/// like a healthy node's while holding a partial download. This is present exactly while that is
/// the case.
const SNAP_SYNC_STAGE: StageId = StageId::Other("SnapSync");

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

    /// Clears the hashed state and trie tables so a session starts from a clean generation, and
    /// records that the state left behind is not yet verified.
    ///
    /// Without the clear a session inherits whatever was there — a genesis allocation, or the
    /// partial state of an attempt that failed — and the final root check cannot tell the
    /// difference between that and downloaded state.
    ///
    /// The marker goes in the same transaction as the clear, so there is no instant at which the
    /// tables are wiped without something on disk saying so.
    ///
    /// Fails on the legacy plain-state layout before touching anything: its state providers read
    /// plain tables, which snap data — hashed keys with no preimages — can never fill.
    pub fn begin_generation(&self, target_block: u64) -> Result<(), SnapSyncError>
    where
        F::ProviderRW: StorageSettingsCache,
    {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        if !provider.cached_storage_settings().use_hashed_state() {
            return Err(SnapSyncError::UnsupportedStorageLayout)
        }
        {
            let tx = provider.tx_ref();
            tx.clear::<tables::HashedAccounts>().map_err(db_err)?;
            tx.clear::<tables::HashedStorages>().map_err(db_err)?;
            tx.clear::<tables::AccountsTrie>().map_err(db_err)?;
            tx.clear::<tables::StoragesTrie>().map_err(db_err)?;
            tx.put::<tables::StageCheckpoints>(
                SNAP_SYNC_STAGE.to_string(),
                StageCheckpoint::new(target_block),
            )
            .map_err(db_err)?;
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

    /// Returns the target block of a generation that was interrupted before it was verified.
    ///
    /// `Some` means the hashed state on disk is a partial download and must not be read as though
    /// it were a synced node's state.
    pub fn interrupted_generation(&self) -> Result<Option<u64>, SnapSyncError> {
        let provider = self.factory.database_provider_ro().map_err(db_err)?;
        let checkpoint = provider
            .tx_ref()
            .get::<tables::StageCheckpoints>(SNAP_SYNC_STAGE.to_string())
            .map_err(db_err)?;

        Ok(checkpoint.map(|checkpoint| checkpoint.block_number))
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
    /// the node cannot serve proofs or extend the chain from hashed state alone.
    ///
    /// The walk is chunked so peak memory does not scale with total state size. All chunks share
    /// one transaction, committed only once the root matches.
    pub fn finalize_sync(&self, block_number: u64, expected: B256) -> Result<(), SnapSyncError> {
        self.finalize_sync_chunked(block_number, expected, None)
    }

    /// [`Self::finalize_sync`], with an explicit number of hashed entries per chunk.
    ///
    /// `None` keeps the trie crate's default. Only the chunk size varies: the root, the written
    /// nodes and the all-or-nothing commit are identical whatever it is.
    fn finalize_sync_chunked(
        &self,
        block_number: u64,
        expected: B256,
        entries_per_chunk: Option<u64>,
    ) -> Result<(), SnapSyncError> {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;

        let mut intermediate = None;
        let computed = loop {
            let progress = reth_trie_db::with_adapter!(provider, |A| {
                let mut state_root = DbStateRoot::<_, A>::from_tx(provider.tx_ref())
                    .with_intermediate_state(intermediate.take());
                if let Some(entries) = entries_per_chunk {
                    state_root = state_root.with_threshold(entries);
                }
                state_root.root_with_progress()
            })
            .map_err(|err| SnapSyncError::Database(format!("state root computation: {err}")))?;

            match progress {
                StateRootProgress::Progress(state, _, updates) => {
                    provider.write_trie_updates(updates).map_err(db_err)?;
                    intermediate = Some(*state);
                }
                StateRootProgress::Complete(root, _, updates) => {
                    provider.write_trie_updates(updates).map_err(db_err)?;
                    break root
                }
            }
        };

        if computed != expected {
            // Dropping the provider without committing discards every chunk written above, so a
            // retry at a later pivot starts from the hashed state rather than a half-built trie.
            return Err(SnapSyncError::StateRootMismatch { block: block_number, expected, computed })
        }

        // Cleared in the same transaction as the nodes that make the state usable, so the marker
        // outlives every state the root check has not vouched for.
        provider
            .tx_ref()
            .delete::<tables::StageCheckpoints>(SNAP_SYNC_STAGE.to_string(), None)
            .map_err(db_err)?;
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
    fn legacy_plain_state_layout_is_refused_before_the_wipe() {
        let factory = create_test_provider_factory();
        let writer = SnapStateWriter::new(&factory);
        let (state, _) = fixture();
        writer.write_state(state).unwrap();

        factory.set_storage_settings_cache(reth_db_api::models::StorageSettings::v1());

        // Refusing after the wipe would destroy a v1 node's hashed tables for nothing.
        assert!(matches!(writer.begin_generation(1), Err(SnapSyncError::UnsupportedStorageLayout)));
        let provider = factory.database_provider_ro().unwrap();
        let mut cursor = provider.tx_ref().cursor_read::<tables::HashedAccounts>().unwrap();
        assert!(cursor.first().unwrap().is_some(), "existing state must be left untouched");
    }

    #[test]
    fn an_unverified_generation_is_marked_on_disk() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(reth_db_api::models::StorageSettings::v2());
        let writer = SnapStateWriter::new(&factory);
        let (state, root) = fixture();

        writer.begin_generation(4242).unwrap();
        // Everything between here and the root check is a partial download.
        assert_eq!(writer.interrupted_generation().unwrap(), Some(4242));

        writer.write_state(state).unwrap();
        writer.finalize_sync(4242, root).unwrap();

        assert_eq!(writer.interrupted_generation().unwrap(), None);
    }

    #[test]
    fn a_rejected_generation_stays_marked() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(reth_db_api::models::StorageSettings::v2());
        let writer = SnapStateWriter::new(&factory);
        let (state, _) = fixture();

        writer.begin_generation(4242).unwrap();
        writer.write_state(state).unwrap();
        writer.finalize_sync(4242, b256(0xdead)).unwrap_err();

        // The state is still a partial download, so a restart must not trust it.
        assert_eq!(writer.interrupted_generation().unwrap(), Some(4242));
    }

    #[test]
    fn chunked_walk_reaches_the_same_root_as_a_single_pass() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.write_state(state).unwrap();

        // One entry per chunk, so the walk resumes from an intermediate state many times over.
        writer.finalize_sync_chunked(100, root, Some(1)).unwrap();

        assert!(!trie_is_empty(&factory));
    }

    #[test]
    fn a_chunked_walk_that_mismatches_writes_nothing() {
        let factory = create_test_provider_factory();
        let (state, _) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.write_state(state).unwrap();

        // Chunks are written as the walk goes, so the mismatch has to discard the earlier ones too.
        assert!(matches!(
            writer.finalize_sync_chunked(100, b256(0xdead), Some(1)),
            Err(SnapSyncError::StateRootMismatch { .. })
        ));
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
