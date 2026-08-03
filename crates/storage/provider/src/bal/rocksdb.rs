use crate::providers::RocksDBProvider;
use alloy_eip7928::BAL_RETENTION_PERIOD_SLOTS;
use alloy_eips::NumHash;
use alloy_primitives::{BlockHash, BlockNumber, Bytes};
use parking_lot::RwLock;
use reth_db_api::{
    models::{StoredBlockAccessList, StoredBlockAccessListKey},
    table::{Decode, Decompress},
    tables, DatabaseError,
};
use reth_prune_types::PruneMode;
use reth_storage_api::{BalNotification, BalNotificationStream, BalStore, RawBal};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use reth_tokio_util::EventSender;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

const DEFAULT_BAL_BUFFER_RETENTION_DISTANCE: u64 = 32;

/// RocksDB-backed BAL store with a recent hash-indexed buffer.
#[derive(Clone)]
pub struct RocksDBBalStore {
    retention: PruneMode,
    buffer_retention: PruneMode,
    rocksdb: RocksDBProvider,
    buffer: Arc<RwLock<RocksDBBalStoreBuffer>>,
    notifications: EventSender<BalNotification>,
}

impl RocksDBBalStore {
    /// Creates a store with the EIP-7928 retention distance.
    pub fn new(rocksdb: RocksDBProvider) -> Self {
        Self::with_retention_distance(rocksdb, BAL_RETENTION_PERIOD_SLOTS)
    }

    /// Creates a store with the given persisted retention distance.
    pub fn with_retention_distance(rocksdb: RocksDBProvider, blocks: u64) -> Self {
        Self {
            retention: PruneMode::Distance(blocks),
            buffer_retention: PruneMode::Distance(
                blocks.min(DEFAULT_BAL_BUFFER_RETENTION_DISTANCE),
            ),
            rocksdb,
            buffer: Arc::new(RwLock::new(RocksDBBalStoreBuffer::default())),
            notifications: EventSender::new(super::DEFAULT_BAL_NOTIFICATION_CHANNEL_SIZE),
        }
    }

    /// Sets the recent hash-only cache distance without reducing disk retention.
    pub fn with_buffer_retention_distance(rocksdb: RocksDBProvider, blocks: u64) -> Self {
        let mut store = Self::new(rocksdb);
        store.buffer_retention = PruneMode::Distance(blocks);
        store
    }

    fn keys_to_prune(&self, tip: BlockNumber) -> ProviderResult<Vec<StoredBlockAccessListKey>> {
        let mut keys = Vec::new();
        let iter = self.rocksdb.raw_key_iter_from::<tables::BlockAccessLists>(
            StoredBlockAccessListKey::first_at_number(0),
        )?;

        for key in iter {
            let key = StoredBlockAccessListKey::decode(&key?)
                .map_err(|_| ProviderError::Database(DatabaseError::Decode))?;
            if !self.retention.should_prune(key.number(), tip) {
                break
            }
            keys.push(key);
        }
        Ok(keys)
    }

    fn read_from_disk(&self, block: NumHash) -> ProviderResult<Option<Bytes>> {
        let key = StoredBlockAccessListKey::new(block);
        let Some(value) = self.rocksdb.get_raw::<tables::BlockAccessLists>(key)? else {
            return Ok(None)
        };
        let stored = StoredBlockAccessList::decompress(&value)
            .map_err(|_| ProviderError::Database(DatabaseError::Decode))?;
        stored.into_verified_raw().map(|raw| Some(raw.into_raw())).map_err(ProviderError::other)
    }

    #[cfg(test)]
    const fn rocksdb_provider(&self) -> &RocksDBProvider {
        &self.rocksdb
    }
}

impl std::fmt::Debug for RocksDBBalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksDBBalStore")
            .field("retention", &self.retention)
            .field("buffer_retention", &self.buffer_retention)
            .field("rocksdb", &self.rocksdb)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct RocksDBBalStoreBuffer {
    entries: HashMap<BlockHash, RocksDBBalEntry>,
    hashes_by_number: BTreeMap<BlockNumber, Vec<BlockHash>>,
    pending: BTreeMap<StoredBlockAccessListKey, RawBal>,
    highest_block_number: Option<BlockNumber>,
}

impl RocksDBBalStoreBuffer {
    fn insert(&mut self, block: NumHash, bal: RawBal) {
        if let Some(entry) = self
            .entries
            .insert(block.hash, RocksDBBalEntry { block_number: block.number, bal: bal.clone() })
        {
            self.remove_hash_from_number(entry.block_number, block.hash);
            self.pending.remove(&StoredBlockAccessListKey::new(NumHash::new(
                entry.block_number,
                block.hash,
            )));
        }

        self.hashes_by_number.entry(block.number).or_default().push(block.hash);
        self.pending.insert(StoredBlockAccessListKey::new(block), bal);
        self.highest_block_number = Some(
            self.highest_block_number.map_or(block.number, |highest| highest.max(block.number)),
        );
    }

    fn pending_entries(&self, blocks: &[NumHash]) -> Vec<(StoredBlockAccessListKey, RawBal)> {
        blocks
            .iter()
            .filter_map(|block| {
                let key = StoredBlockAccessListKey::new(*block);
                self.pending.get(&key).map(|bal| (key, bal.clone()))
            })
            .collect()
    }

    fn get_by_hash(&self, hash: BlockHash) -> Option<Bytes> {
        self.entries.get(&hash).map(|entry| entry.bal.as_raw().clone())
    }

    fn get_by_block(&self, block: NumHash) -> Option<Bytes> {
        self.entries
            .get(&block.hash)
            .filter(|entry| entry.block_number == block.number)
            .map(|entry| entry.bal.as_raw().clone())
    }

    fn remove_flushed(&mut self, flushed: &[(StoredBlockAccessListKey, RawBal)]) {
        for (key, bal) in flushed {
            if self.pending.get(key).is_some_and(|pending| pending.as_raw() == bal.as_raw()) {
                self.pending.remove(key);
            }
        }
    }

    fn prune_cache(&mut self, mode: PruneMode, tip: BlockNumber) -> Vec<StoredBlockAccessListKey> {
        let numbers = self
            .hashes_by_number
            .keys()
            .copied()
            .take_while(|number| mode.should_prune(*number, tip))
            .collect::<Vec<_>>();
        let mut removed = Vec::new();

        for number in numbers {
            let Some(hashes) = self.hashes_by_number.remove(&number) else { continue };
            for hash in hashes {
                if self.entries.remove(&hash).is_some() {
                    removed.push(StoredBlockAccessListKey::new(NumHash::new(number, hash)));
                }
            }
        }
        removed
    }

    fn prune_pending(
        &mut self,
        mode: PruneMode,
        tip: BlockNumber,
    ) -> Vec<StoredBlockAccessListKey> {
        let keys = self
            .pending
            .keys()
            .copied()
            .take_while(|key| mode.should_prune(key.number(), tip))
            .collect::<Vec<_>>();
        for key in &keys {
            self.pending.remove(key);
        }
        keys
    }

    fn remove_hash_from_number(&mut self, number: BlockNumber, hash: BlockHash) {
        let empty = self.hashes_by_number.get_mut(&number).is_some_and(|hashes| {
            hashes.retain(|candidate| *candidate != hash);
            hashes.is_empty()
        });
        if empty {
            self.hashes_by_number.remove(&number);
        }
    }
}

#[derive(Debug)]
struct RocksDBBalEntry {
    block_number: BlockNumber,
    bal: RawBal,
}

impl BalStore for RocksDBBalStore {
    fn insert(&self, block: NumHash, bal: RawBal) -> ProviderResult<()> {
        self.buffer.write().insert(block, bal.clone());
        self.notifications.notify(BalNotification::new(block, bal));
        Ok(())
    }

    fn insert_many(&self, entries: Vec<(NumHash, RawBal)>) -> ProviderResult<()> {
        if entries.is_empty() {
            return Ok(())
        }

        let mut buffer = self.buffer.write();
        buffer.entries.reserve(entries.len());
        for (block, bal) in &entries {
            buffer.insert(*block, bal.clone());
        }
        drop(buffer);

        for (block, bal) in entries {
            self.notifications.notify(BalNotification::new(block, bal));
        }
        Ok(())
    }

    fn flush(&self, blocks: &[NumHash]) -> ProviderResult<()> {
        let mut buffer = self.buffer.write();
        let pending = buffer.pending_entries(blocks);
        if !pending.is_empty() {
            let mut batch = self.rocksdb.batch();
            for (key, bal) in &pending {
                batch.put::<tables::BlockAccessLists>(
                    *key,
                    &StoredBlockAccessList::new(bal.clone()),
                )?;
            }
            batch.commit()?;
            buffer.remove_flushed(&pending);
        }

        if let Some(tip) = buffer.highest_block_number {
            buffer.prune_cache(self.buffer_retention, tip);
        }
        Ok(())
    }

    fn prune(&self, tip: BlockNumber) -> ProviderResult<usize> {
        let keys = self.keys_to_prune(tip)?;
        if !keys.is_empty() {
            let mut batch = self.rocksdb.batch();
            for key in &keys {
                batch.delete::<tables::BlockAccessLists>(*key)?;
            }
            batch.commit()?;
        }

        let mut pruned = keys.into_iter().collect::<BTreeSet<_>>();
        let mut buffer = self.buffer.write();
        pruned.extend(buffer.prune_cache(self.retention, tip));
        pruned.extend(buffer.prune_pending(self.retention, tip));
        Ok(pruned.len())
    }

    fn get_by_hashes(&self, hashes: &[BlockHash]) -> ProviderResult<Vec<Option<Bytes>>> {
        let buffer = self.buffer.read();
        Ok(hashes.iter().map(|hash| buffer.get_by_hash(*hash)).collect())
    }

    fn get_by_block_num_hash(&self, block: NumHash) -> ProviderResult<Option<Bytes>> {
        if let Some(bal) = self.buffer.read().get_by_block(block) {
            return Ok(Some(bal))
        }
        self.read_from_disk(block)
    }

    fn bal_stream(&self) -> BalNotificationStream {
        self.notifications.new_listener()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::RocksDBBuilder;
    use alloy_primitives::B256;

    fn test_store() -> (tempfile::TempDir, RocksDBBalStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = RocksDBBuilder::new(dir.path()).with_default_tables().build().unwrap();
        (dir, RocksDBBalStore::new(db))
    }

    fn read(store: &RocksDBBalStore, block: NumHash) -> Option<Bytes> {
        store.get_by_block_num_hash(block).unwrap()
    }

    #[test]
    fn flush_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let block = NumHash::new(7, B256::with_last_byte(1));
        let bal = Bytes::from_static(&[0xc1, 0x01]);

        {
            let db = RocksDBBuilder::new(dir.path()).with_default_tables().build().unwrap();
            let store = RocksDBBalStore::new(db);
            store.insert(block, RawBal::from(bal.clone())).unwrap();
            store.flush(&[block]).unwrap();
        }

        let db = RocksDBBuilder::new(dir.path()).with_default_tables().build().unwrap();
        assert_eq!(read(&RocksDBBalStore::new(db), block), Some(bal));
    }

    #[test]
    fn multiple_forks_survive_flush() {
        let (_dir, store) = test_store();
        let first = NumHash::new(10, B256::with_last_byte(1));
        let second = NumHash::new(10, B256::with_last_byte(2));

        store.insert(first, RawBal::from(Bytes::from_static(&[0xc0]))).unwrap();
        store.insert(second, RawBal::from(Bytes::from_static(&[0xc1, 0x02]))).unwrap();
        store.flush(&[first, second]).unwrap();
        store.buffer.write().entries.clear();

        assert_eq!(read(&store, first), Some(Bytes::from_static(&[0xc0])));
        assert_eq!(read(&store, second), Some(Bytes::from_static(&[0xc1, 0x02])));
    }

    #[test]
    fn flush_writes_only_requested_blocks() {
        let (_dir, store) = test_store();
        let canonical = NumHash::new(10, B256::with_last_byte(1));
        let fork = NumHash::new(10, B256::with_last_byte(2));

        store.insert(canonical, RawBal::from(Bytes::from_static(&[0xc0]))).unwrap();
        store.insert(fork, RawBal::from(Bytes::from_static(&[0xc1, 0x02]))).unwrap();
        store.flush(&[canonical]).unwrap();
        store.buffer.write().entries.clear();

        assert!(read(&store, canonical).is_some());
        assert_eq!(read(&store, fork), None);
    }

    #[test]
    fn cache_eviction_keeps_unflushed_bals_pending() {
        let dir = tempfile::tempdir().unwrap();
        let db = RocksDBBuilder::new(dir.path()).with_default_tables().build().unwrap();
        let store = RocksDBBalStore::with_buffer_retention_distance(db, 1);
        let old = NumHash::new(1, B256::with_last_byte(1));
        let tip = NumHash::new(3, B256::with_last_byte(3));

        store.insert(old, RawBal::from(Bytes::from_static(&[0xc0]))).unwrap();
        store.insert(tip, RawBal::from(Bytes::from_static(&[0xc1, 0x03]))).unwrap();
        store.flush(&[tip]).unwrap();
        assert!(store.buffer.read().get_by_block(old).is_none());

        store.flush(&[old]).unwrap();
        assert_eq!(read(&store, old), Some(Bytes::from_static(&[0xc0])));
    }

    #[test]
    fn prune_removes_buffer_only_bals() {
        let dir = tempfile::tempdir().unwrap();
        let db = RocksDBBuilder::new(dir.path()).with_default_tables().build().unwrap();
        let store = RocksDBBalStore::with_retention_distance(db, 2);
        let old = NumHash::new(7, B256::with_last_byte(1));

        store.insert(old, RawBal::from(Bytes::from_static(&[0xc0]))).unwrap();

        assert_eq!(store.prune(10).unwrap(), 1);
        assert_eq!(read(&store, old), None);
    }

    #[test]
    fn prune_uses_configured_retention() {
        let dir = tempfile::tempdir().unwrap();
        let db = RocksDBBuilder::new(dir.path()).with_default_tables().build().unwrap();
        let store = RocksDBBalStore::with_retention_distance(db, 2);
        let old = NumHash::new(7, B256::with_last_byte(1));
        let kept = NumHash::new(8, B256::with_last_byte(2));

        store.insert(old, RawBal::from(Bytes::from_static(&[0xc0]))).unwrap();
        store.insert(kept, RawBal::from(Bytes::from_static(&[0xc1, 0x02]))).unwrap();
        store.flush(&[old, kept]).unwrap();

        assert_eq!(store.prune(10).unwrap(), 1);
        assert_eq!(read(&store, old), None);
        assert!(read(&store, kept).is_some());
    }

    #[test]
    fn corrupt_payload_is_rejected() {
        let (_dir, store) = test_store();
        let block = NumHash::new(1, B256::with_last_byte(1));
        let mut encoded = B256::ZERO.to_vec();
        encoded.push(0xc0);
        let value = StoredBlockAccessList::decompress(&encoded).unwrap();

        store
            .rocksdb_provider()
            .put::<tables::BlockAccessLists>(StoredBlockAccessListKey::new(block), &value)
            .unwrap();

        assert!(store.get_by_block_num_hash(block).is_err());
    }
}
