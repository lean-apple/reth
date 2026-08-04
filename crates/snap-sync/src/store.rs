//! The writer boundary: generation lifecycle, write modes, and finalization.

use crate::error::SnapSyncError;
use alloy_primitives::{Address, Bytes, B256};
use reth_db_api::{
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{
    DatabaseProviderFactory, StaticFileProviderFactory, StaticFileSegment, StaticFileWriter,
};
use reth_stages_types::{StageCheckpoint, StageId};
use reth_storage_api::{
    AccountExtReader, DBProvider, StageCheckpointWriter, StateWriter, StorageSettingsCache,
    TrieWriter,
};
use reth_trie::HashedPostState;
use reth_trie_db::{state_root_with_committed_updates, STATE_ROOT_COMMIT_THRESHOLD};

/// Persists verified snap state to the database.
///
/// Each write commits on its own: a batch is only durable once it has been checked against the
/// pivot root, so a download interrupted mid-range leaves behind verified state rather than a
/// partially written range.
#[derive(Debug)]
pub struct SnapStateWriter<'a, F> {
    factory: &'a F,
}

/// Stage slot marking a snap sync generation whose state root has not been checked yet.
///
/// A generation starts by wiping the hashed state, so a crash part-way leaves tables that look
/// like a healthy node's while holding a partial download. This is present exactly while that is
/// the case.
const SNAP_SYNC_STAGE: StageId = StageId::Other("SnapSync");

/// Persisted identity of an unverified snap generation.
///
/// Restart detects this marker, then rebuilds from scratch instead of resuming partial state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, alloy_rlp::RlpEncodable, alloy_rlp::RlpDecodable)]
pub struct SnapGeneration {
    /// Height of the target block, matching the stage checkpoint row.
    pub target_block: u64,
    /// Hash of the target block. The identity of the generation.
    pub target_hash: B256,
    /// State root the generation is assembling toward.
    pub state_root: B256,
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
    pub fn begin_generation(&self, generation: SnapGeneration) -> Result<(), SnapSyncError>
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
            // The checkpoint row keeps the marker visible to standard stage tooling; the progress
            // blob carries what a height alone cannot say.
            tx.put::<tables::StageCheckpoints>(
                SNAP_SYNC_STAGE.to_string(),
                StageCheckpoint::new(generation.target_block),
            )
            .map_err(db_err)?;
            tx.put::<tables::StageCheckpointProgresses>(
                SNAP_SYNC_STAGE.to_string(),
                alloy_rlp::encode(generation),
            )
            .map_err(db_err)?;
        }
        provider.commit().map_err(db_err)?;
        Ok(())
    }

    /// Rewrites the generation marker for a target the session moved to.
    ///
    /// The rolling transition changes which block the partial state is converging on; the marker
    /// has to follow, or a restart would blame the wrong block for the state on disk. Leaves the
    /// tables alone: the downloaded prefix is exactly what the transition carries over.
    pub fn update_generation(&self, generation: SnapGeneration) -> Result<(), SnapSyncError> {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        {
            let tx = provider.tx_ref();
            tx.put::<tables::StageCheckpoints>(
                SNAP_SYNC_STAGE.to_string(),
                StageCheckpoint::new(generation.target_block),
            )
            .map_err(db_err)?;
            tx.put::<tables::StageCheckpointProgresses>(
                SNAP_SYNC_STAGE.to_string(),
                alloy_rlp::encode(generation),
            )
            .map_err(db_err)?;
        }
        provider.commit().map_err(db_err)?;
        Ok(())
    }

    /// Accepts the state and aligns Reth's pipeline and static-file frontiers with it.
    pub fn accept_generation(&self, block_number: u64) -> Result<(), SnapSyncError>
    where
        F::ProviderRW: StageCheckpointWriter + StaticFileProviderFactory,
    {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        provider.update_pipeline_stages(block_number, false).map_err(db_err)?;
        {
            let tx = provider.tx_ref();
            tx.delete::<tables::StageCheckpoints>(SNAP_SYNC_STAGE.to_string(), None)
                .map_err(db_err)?;
            tx.delete::<tables::StageCheckpointProgresses>(SNAP_SYNC_STAGE.to_string(), None)
                .map_err(db_err)?;
        }

        // Snap supplies state but not historical block data. Empty advancement lets the normal
        // persistence path append the first post-snap block without a static-file gap.
        let static_files = provider.static_file_provider();
        for segment in [
            StaticFileSegment::Transactions,
            StaticFileSegment::TransactionSenders,
            StaticFileSegment::Receipts,
            StaticFileSegment::AccountChangeSets,
            StaticFileSegment::StorageChangeSets,
        ] {
            static_files
                .latest_writer(segment)
                .map_err(db_err)?
                .ensure_at_block(block_number)
                .map_err(db_err)?;
        }
        static_files.commit().map_err(db_err)?;
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
}

impl<F> SnapStateWriter<'_, F>
where
    F: DatabaseProviderFactory,
    F::Provider: AccountExtReader + DBProvider,
    <F::Provider as DBProvider>::Tx: DbTx,
{
    /// Reads accounts in one provider transaction for block access list merging.
    pub fn read_accounts(
        &self,
        addresses: impl IntoIterator<Item = Address>,
    ) -> Result<Vec<(Address, Option<Account>)>, SnapSyncError> {
        let provider = self.factory.database_provider_ro().map_err(db_err)?;
        provider.basic_accounts(addresses).map_err(db_err)
    }
}

impl<F> SnapStateWriter<'_, F>
where
    F: DatabaseProviderFactory,
    F::Provider: DBProvider,
    <F::Provider as DBProvider>::Tx: DbTx,
{
    /// Returns the generation that was interrupted before it was verified.
    ///
    /// `Some` means the hashed state on disk is a partial download and must not be read as though
    /// it were a synced node's state.
    pub fn interrupted_generation(&self) -> Result<Option<SnapGeneration>, SnapSyncError> {
        let provider = self.factory.database_provider_ro().map_err(db_err)?;
        let Some(blob) = provider
            .tx_ref()
            .get::<tables::StageCheckpointProgresses>(SNAP_SYNC_STAGE.to_string())
            .map_err(db_err)?
        else {
            return Ok(None)
        };

        alloy_rlp::Decodable::decode(&mut blob.as_slice())
            .map(Some)
            .map_err(|err| SnapSyncError::Database(format!("snap generation marker: {err}")))
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
    /// Trie updates are committed in chunks while the generation marker keeps them untrusted.
    /// This bounds MDBX dirty pages and makes each completed chunk crash-durable.
    pub fn finalize_sync(&self, block_number: u64, expected: B256) -> Result<(), SnapSyncError> {
        self.finalize_sync_chunked(block_number, expected, None)
    }

    /// [`Self::finalize_sync`], with an explicit number of hashed entries per chunk.
    ///
    /// `None` uses Reth's shared state-root commit threshold.
    fn finalize_sync_chunked(
        &self,
        block_number: u64,
        expected: B256,
        entries_per_chunk: Option<u64>,
    ) -> Result<(), SnapSyncError> {
        self.clear_trie()?;
        let computed = state_root_with_committed_updates(
            self.factory,
            entries_per_chunk.unwrap_or(STATE_ROOT_COMMIT_THRESHOLD),
        )
        .map_err(|err| SnapSyncError::Database(format!("state root computation: {err}")))?;

        if computed != expected {
            self.clear_trie()?;
            return Err(SnapSyncError::StateRootMismatch { block: block_number, expected, computed })
        }

        // The generation marker is deliberately left in place: a matching root proves the state
        // is block `block_number`'s, not that the node accepted it — the block can have been
        // orphaned while the trie was being walked.
        Ok(())
    }

    fn clear_trie(&self) -> Result<(), SnapSyncError> {
        let provider = self.factory.database_provider_rw().map_err(db_err)?;
        provider.tx_ref().clear::<tables::AccountsTrie>().map_err(db_err)?;
        provider.tx_ref().clear::<tables::StoragesTrie>().map_err(db_err)?;
        provider.commit().map_err(db_err)?;
        Ok(())
    }
}

fn db_err(err: impl core::fmt::Display) -> SnapSyncError {
    SnapSyncError::Database(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{map::B256Map, U256};
    use reth_db_api::cursor::DbCursorRO;
    use reth_provider::{
        test_utils::create_test_provider_factory, ProviderError, StaticFileProviderFactory,
    };
    use reth_storage_api::StageCheckpointReader;
    use reth_trie::{test_utils::state_root_prehashed, HashedStorage};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    fn generation(target_block: u64) -> SnapGeneration {
        SnapGeneration {
            target_block,
            target_hash: b256(target_block),
            state_root: b256(target_block + 1),
        }
    }

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

    #[derive(Debug)]
    struct LimitedRwFactory<F> {
        inner: F,
        remaining: AtomicUsize,
    }

    impl<F> DatabaseProviderFactory for LimitedRwFactory<F>
    where
        F: DatabaseProviderFactory,
    {
        type DB = F::DB;
        type Provider = F::Provider;
        type ProviderRW = F::ProviderRW;

        fn database_provider_ro(&self) -> Result<Self::Provider, ProviderError> {
            self.inner.database_provider_ro()
        }

        fn database_provider_rw(&self) -> Result<Self::ProviderRW, ProviderError> {
            self.remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .map_err(|_| {
                    ProviderError::other(std::io::Error::other("injected interruption"))
                })?;
            self.inner.database_provider_rw()
        }
    }

    #[test]
    fn matching_root_persists_the_trie_tables() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.commit_batch(state, &[]).unwrap();

        writer.finalize_sync(100, root).unwrap();

        // The node cannot serve proofs from hashed state alone, so the rebuilt nodes must land.
        assert!(!trie_is_empty(&factory));
    }

    #[test]
    fn mismatched_root_reports_both_roots_and_writes_nothing() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.commit_batch(state, &[]).unwrap();

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
        writer.commit_batch(state, &[]).unwrap();

        factory.set_storage_settings_cache(reth_db_api::models::StorageSettings::v1());

        // Refusing after the wipe would destroy a v1 node's hashed tables for nothing.
        assert!(matches!(
            writer.begin_generation(generation(1)),
            Err(SnapSyncError::UnsupportedStorageLayout)
        ));
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

        writer.begin_generation(generation(4242)).unwrap();
        // Everything between here and acceptance is not this node's state yet.
        assert_eq!(writer.interrupted_generation().unwrap(), Some(generation(4242)));

        writer.commit_batch(state, &[]).unwrap();
        writer.finalize_sync(4242, root).unwrap();

        // A matching root is not acceptance: only pipeline handoff clears the marker.
        assert_eq!(writer.interrupted_generation().unwrap(), Some(generation(4242)));

        writer.accept_generation(4242).unwrap();
        assert_eq!(writer.interrupted_generation().unwrap(), None);
    }

    #[test]
    fn acceptance_advances_pipeline_checkpoints_and_clears_the_marker_atomically() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(reth_db_api::models::StorageSettings::v2());
        let writer = SnapStateWriter::new(&factory);
        writer.begin_generation(generation(4242)).unwrap();

        writer.accept_generation(4242).unwrap();

        assert_eq!(writer.interrupted_generation().unwrap(), None);
        let provider = factory.database_provider_ro().unwrap();
        for stage in StageId::ALL {
            assert_eq!(provider.get_stage_checkpoint(stage).unwrap().unwrap().block_number, 4242);
        }
        let static_files = provider.static_file_provider();
        for segment in [
            StaticFileSegment::Transactions,
            StaticFileSegment::TransactionSenders,
            StaticFileSegment::Receipts,
            StaticFileSegment::AccountChangeSets,
            StaticFileSegment::StorageChangeSets,
        ] {
            assert_eq!(static_files.get_highest_static_file_block(segment), Some(4242));
        }
    }

    #[test]
    fn a_rejected_generation_stays_marked() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(reth_db_api::models::StorageSettings::v2());
        let writer = SnapStateWriter::new(&factory);
        let (state, _) = fixture();

        writer.begin_generation(generation(4242)).unwrap();
        writer.commit_batch(state, &[]).unwrap();
        writer.finalize_sync(4242, b256(0xdead)).unwrap_err();

        // The state is still a partial download, so a restart must not trust it.
        assert_eq!(writer.interrupted_generation().unwrap(), Some(generation(4242)));
    }

    #[test]
    fn a_moved_target_rewrites_the_marker_in_place() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(reth_db_api::models::StorageSettings::v2());
        let writer = SnapStateWriter::new(&factory);
        let (state, _) = fixture();

        writer.begin_generation(generation(7)).unwrap();
        writer.commit_batch(state, &[]).unwrap();

        writer.update_generation(generation(9)).unwrap();

        // The marker follows the rolling target; the downloaded prefix stays.
        assert_eq!(writer.interrupted_generation().unwrap(), Some(generation(9)));
        let provider = factory.database_provider_ro().unwrap();
        let mut cursor = provider.tx_ref().cursor_read::<tables::HashedAccounts>().unwrap();
        assert!(cursor.first().unwrap().is_some());
    }

    #[test]
    fn chunked_walk_reaches_the_same_root_as_a_single_pass() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.commit_batch(state, &[]).unwrap();

        // One entry per chunk, so the walk resumes from an intermediate state many times over.
        writer.finalize_sync_chunked(100, root, Some(1)).unwrap();

        assert!(!trie_is_empty(&factory));
    }

    #[test]
    fn rebuilding_after_more_state_changes_discards_the_previous_trie() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.commit_batch(state, &[]).unwrap();
        writer.finalize_sync(100, root).unwrap();

        let replacement = account(999);
        writer
            .commit_batch(
                HashedPostState {
                    accounts: B256Map::from_iter([(hashed_address(7), Some(replacement))]),
                    storages: B256Map::default(),
                },
                &[],
            )
            .unwrap();

        let slots = [(b256(0x10), U256::from(1)), (b256(0x11), U256::from(2))];
        let new_root = state_root_prehashed((0..64).map(|i| {
            let hash = hashed_address(i);
            let account = if i == 7 { replacement } else { account(i + 1) };
            let storage = if hash == storage_owner() { slots.to_vec() } else { Vec::new() };
            (hash, (account, storage))
        }));

        writer.finalize_sync(101, new_root).unwrap();
        assert!(!trie_is_empty(&factory));
    }

    #[test]
    fn a_chunked_walk_that_mismatches_writes_nothing() {
        let factory = create_test_provider_factory();
        let (state, _) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.commit_batch(state, &[]).unwrap();

        // Chunks are written as the walk goes, so the mismatch has to discard the earlier ones too.
        assert!(matches!(
            writer.finalize_sync_chunked(100, b256(0xdead), Some(1)),
            Err(SnapSyncError::StateRootMismatch { .. })
        ));
        assert!(trie_is_empty(&factory));
    }

    #[test]
    fn interrupted_chunked_rebuild_stays_marked_until_restart_clears_it() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(reth_db_api::models::StorageSettings::v2());
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);
        writer.begin_generation(generation(100)).unwrap();
        writer.commit_batch(state, &[]).unwrap();

        // Clear, adapter selection, and one rebuild chunk succeed before the injected failure.
        let limited = LimitedRwFactory { inner: factory, remaining: AtomicUsize::new(3) };
        assert!(matches!(
            SnapStateWriter::new(&limited).finalize_sync_chunked(100, root, Some(1)),
            Err(SnapSyncError::Database(_))
        ));
        assert!(!trie_is_empty(&limited));
        assert_eq!(
            SnapStateWriter::new(&limited).interrupted_generation().unwrap(),
            Some(generation(100))
        );

        SnapStateWriter::new(&limited.inner).begin_generation(generation(100)).unwrap();
        assert!(trie_is_empty(&limited.inner));
    }

    #[test]
    fn missing_state_does_not_pass_as_a_matching_root() {
        let factory = create_test_provider_factory();
        let (state, root) = fixture();
        let writer = SnapStateWriter::new(&factory);

        // Drop one account, as a peer withholding a range would.
        let mut partial = state;
        partial.accounts.remove(&hashed_address(7));
        writer.commit_batch(partial, &[]).unwrap();

        assert!(matches!(
            writer.finalize_sync(100, root),
            Err(SnapSyncError::StateRootMismatch { .. })
        ));
    }
}
