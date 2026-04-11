//! EraE group for erae file content
//!
//! See also <https://github.com/eth-clients/e2store-format-specs/blob/main/formats/ere.md>

use crate::{
    common::file_ops::{EraFileId, EraFileType},
    e2s::{error::E2sError, types::Entry},
    erae::types::execution::{Accumulator, BlockTuple, MAX_BLOCKS_PER_ERAE},
};
use alloy_primitives::BlockNumber;

/// `BlockIndex` record: ['g', '2']
pub const BLOCK_INDEX: [u8; 2] = [0x67, 0x32];

/// File content in an EraE file
///
/// Format: `block-tuple* | other-entries* | Accumulator | BlockIndex`
#[derive(Debug)]
pub struct EraEGroup {
    /// Blocks in this erae group
    pub blocks: Vec<BlockTuple>,

    /// Other entries that don't fit into the standard categories
    pub other_entries: Vec<Entry>,

    /// Accumulator is hash tree root of block headers and difficulties
    pub accumulator: Accumulator,

    /// Block index, required
    pub block_index: BlockIndex,
}

impl EraEGroup {
    /// Create a new [`EraEGroup`]
    pub const fn new(
        blocks: Vec<BlockTuple>,
        accumulator: Accumulator,
        block_index: BlockIndex,
    ) -> Self {
        Self { blocks, accumulator, block_index, other_entries: Vec::new() }
    }

    /// Add another entry to this group
    pub fn add_entry(&mut self, entry: Entry) {
        self.other_entries.push(entry);
    }
}

/// EraE block index with dynamic component count.
///
/// Format:
/// `starting-number | indexes | indexes | ... | component-count | count`
///
/// Where each `indexes` group contains offsets for one block:
/// `header-index | body-index | receipts-index? | difficulty-index? | proof-index?`
///
/// `component-count` is 2-5 depending on which optional components are present.
#[derive(Debug, Clone)]
pub struct BlockIndex {
    /// Starting block number
    starting_number: BlockNumber,

    /// Number of index components per block (2-5)
    component_count: u64,

    /// Flat array of offsets: `[h0, b0, (r0)?, (d0)?, (p0)?, h1, b1, ...]`
    /// Length = block_count * component_count
    offsets: Vec<i64>,
}

impl BlockIndex {
    /// Create a new [`BlockIndex`] with the given component count
    pub fn new(starting_number: u64, component_count: u64, offsets: Vec<i64>) -> Self {
        Self { starting_number, component_count, offsets }
    }

    /// Get the starting block number
    pub const fn starting_number(&self) -> u64 {
        self.starting_number
    }

    /// Get the component count per block
    pub const fn component_count(&self) -> u64 {
        self.component_count
    }

    /// Get the number of blocks in this index
    pub fn block_count(&self) -> usize {
        if self.component_count == 0 {
            return 0;
        }
        self.offsets.len() / self.component_count as usize
    }

    /// Get all offsets
    pub fn offsets(&self) -> &[i64] {
        &self.offsets
    }

    /// Get the offsets for a specific block number.
    ///
    /// Returns a slice of `component_count` offsets:
    /// `[header_offset, body_offset, (receipts_offset)?, (difficulty_offset)?, (proof_offset)?]`
    pub fn offsets_for_block(&self, block_number: BlockNumber) -> Option<&[i64]> {
        if block_number < self.starting_number {
            return None;
        }
        let index = (block_number - self.starting_number) as usize;
        let cc = self.component_count as usize;
        let start = index * cc;
        let end = start + cc;
        if end > self.offsets.len() {
            return None;
        }
        Some(&self.offsets[start..end])
    }

    /// Get the header offset for a specific block number
    pub fn header_offset(&self, block_number: BlockNumber) -> Option<i64> {
        self.offsets_for_block(block_number).map(|o| o[0])
    }

    /// Get the body offset for a specific block number
    pub fn body_offset(&self, block_number: BlockNumber) -> Option<i64> {
        self.offsets_for_block(block_number).map(|o| o[1])
    }

    /// Convert to an [`Entry`] for storage in an e2store file.
    ///
    /// Format: `starting-number | offsets... | component-count | count`
    pub fn to_entry(&self) -> Entry {
        let block_count = self.block_count();
        let mut data = Vec::with_capacity(8 + self.offsets.len() * 8 + 8 + 8);

        data.extend_from_slice(&self.starting_number.to_le_bytes());
        data.extend(self.offsets.iter().flat_map(|o| o.to_le_bytes()));
        data.extend_from_slice(&self.component_count.to_le_bytes());
        data.extend_from_slice(&(block_count as i64).to_le_bytes());

        Entry::new(BLOCK_INDEX, data)
    }

    /// Create from an [`Entry`]
    pub fn from_entry(entry: &Entry) -> Result<Self, E2sError> {
        if entry.entry_type != BLOCK_INDEX {
            return Err(E2sError::Ssz(format!(
                "Invalid entry type for BlockIndex: expected {:02x}{:02x}, got {:02x}{:02x}",
                BLOCK_INDEX[0], BLOCK_INDEX[1], entry.entry_type[0], entry.entry_type[1]
            )));
        }

        // Need at least: starting-number(8) + component-count(8) + count(8) = 24 bytes
        if entry.data.len() < 24 {
            return Err(E2sError::Ssz("Block index too short: need at least 24 bytes".to_string()));
        }

        let data = &entry.data;
        let len = data.len();

        // Extract count from last 8 bytes
        let count = i64::from_le_bytes(
            data[len - 8..]
                .try_into()
                .map_err(|_| E2sError::Ssz("Failed to read count bytes".to_string()))?,
        ) as usize;

        // Extract component-count from second-to-last 8 bytes
        let component_count = u64::from_le_bytes(
            data[len - 16..len - 8]
                .try_into()
                .map_err(|_| E2sError::Ssz("Failed to read component-count bytes".to_string()))?,
        );

        if !(2..=5).contains(&component_count) {
            return Err(E2sError::Ssz(format!(
                "Invalid component-count: {component_count}, expected 2-5"
            )));
        }

        // Verify data length
        let expected_len = 8 + (count * component_count as usize) * 8 + 8 + 8;
        if data.len() != expected_len {
            return Err(E2sError::Ssz(format!(
                "Block index incorrect length: expected {expected_len}, got {}",
                data.len()
            )));
        }

        // Extract starting number
        let starting_number = u64::from_le_bytes(
            data[0..8]
                .try_into()
                .map_err(|_| E2sError::Ssz("Failed to read starting_number".to_string()))?,
        );

        // Extract offsets
        let total_offsets = count * component_count as usize;
        let mut offsets = Vec::with_capacity(total_offsets);
        for i in 0..total_offsets {
            let start = 8 + i * 8;
            let end = start + 8;
            let offset = i64::from_le_bytes(
                data[start..end]
                    .try_into()
                    .map_err(|_| E2sError::Ssz(format!("Failed to read offset {i}")))?,
            );
            offsets.push(offset);
        }

        Ok(Self { starting_number, component_count, offsets })
    }
}

/// EraE file identifier
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EraEId {
    /// Network configuration name
    pub network_name: String,

    /// First block number in file
    pub start_block: BlockNumber,

    /// Number of blocks in the file
    pub block_count: u32,

    /// Optional hash identifier for this file
    /// First 4 bytes of the last historical root in the last state in the era file
    pub hash: Option<[u8; 4]>,

    /// Whether to include era count in filename
    /// It is used for custom exports when we don't use the max number of items per file
    pub include_era_count: bool,
}

impl EraEId {
    /// Create a new [`EraEId`]
    pub fn new(
        network_name: impl Into<String>,
        start_block: BlockNumber,
        block_count: u32,
    ) -> Self {
        Self {
            network_name: network_name.into(),
            start_block,
            block_count,
            hash: None,
            include_era_count: false,
        }
    }

    /// Add a hash identifier to  [`EraEId`]
    pub const fn with_hash(mut self, hash: [u8; 4]) -> Self {
        self.hash = Some(hash);
        self
    }

    /// Include era count in filename, for custom block-per-file exports
    pub const fn with_era_count(mut self) -> Self {
        self.include_era_count = true;
        self
    }
}

impl EraFileId for EraEId {
    const FILE_TYPE: EraFileType = EraFileType::EraE;

    const ITEMS_PER_ERA: u64 = MAX_BLOCKS_PER_ERAE as u64;
    fn network_name(&self) -> &str {
        &self.network_name
    }

    fn start_number(&self) -> u64 {
        self.start_block
    }

    fn count(&self) -> u32 {
        self.block_count
    }

    fn hash(&self) -> Option<[u8; 4]> {
        self.hash
    }

    fn include_era_count(&self) -> bool {
        self.include_era_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::decode::DecodeCompressedRlp,
        test_utils::{create_sample_block, create_test_block_with_compressed_data},
    };
    use alloy_consensus::ReceiptWithBloom;
    use alloy_primitives::{B256, U256};

    #[test]
    fn test_alloy_components_decode_and_receipt_in_bloom() {
        // Create a block tuple from compressed data
        let block: BlockTuple = create_test_block_with_compressed_data(30);

        // Decode and decompress the block header
        let header: alloy_consensus::Header = block.header.decode().unwrap();
        assert_eq!(header.number, 30, "Header block number should match");
        assert_eq!(header.difficulty, U256::from(30 * 1000), "Header difficulty should match");
        assert_eq!(header.gas_limit, 5000000, "Gas limit should match");
        assert_eq!(header.gas_used, 21000, "Gas used should match");
        assert_eq!(header.timestamp, 1609459200 + 30, "Timestamp should match");
        assert_eq!(header.base_fee_per_gas, Some(10), "Base fee per gas should match");
        assert!(header.withdrawals_root.is_some(), "Should have withdrawals root");
        assert!(header.blob_gas_used.is_none(), "Should not have blob gas used");
        assert!(header.excess_blob_gas.is_none(), "Should not have excess blob gas");

        let body: alloy_consensus::BlockBody<alloy_primitives::Bytes> =
            block.body.decode().unwrap();
        assert_eq!(body.ommers.len(), 0, "Should have no ommers");
        assert!(body.withdrawals.is_some(), "Should have withdrawals field");

        let receipts: Vec<ReceiptWithBloom> = block.receipts.decode().unwrap();
        assert_eq!(receipts.len(), 1, "Should have exactly 1 receipt");
    }

    #[test]
    fn test_block_index_roundtrip() {
        let starting_number = 1000;
        let component_count = 4;
        // 2 blocks, 4 components each = 8 offsets
        let offsets = vec![100, 200, 300, 400, 500, 600, 700, 800];

        let block_index = BlockIndex::new(starting_number, component_count, offsets.clone());

        let entry = block_index.to_entry();
        assert_eq!(entry.entry_type, BLOCK_INDEX);

        let recovered = BlockIndex::from_entry(&entry).unwrap();
        assert_eq!(recovered.starting_number, starting_number);
        assert_eq!(recovered.component_count, component_count);
        assert_eq!(recovered.offsets, offsets);
        assert_eq!(recovered.block_count(), 2);
    }

    #[test]
    fn test_block_index_offset_lookup() {
        let starting_number = 1000;
        let component_count = 3;
        // 3 blocks, 3 components each = 9 offsets
        let offsets = vec![10, 20, 30, 40, 50, 60, 70, 80, 90];

        let block_index = BlockIndex::new(starting_number, component_count, offsets);

        // Block 1000: [10, 20, 30]
        assert_eq!(block_index.offsets_for_block(1000), Some(&[10, 20, 30][..]));
        assert_eq!(block_index.header_offset(1000), Some(10));
        assert_eq!(block_index.body_offset(1000), Some(20));

        // Block 1002: [70, 80, 90]
        assert_eq!(block_index.offsets_for_block(1002), Some(&[70, 80, 90][..]));

        // Out of range
        assert_eq!(block_index.offsets_for_block(999), None);
        assert_eq!(block_index.offsets_for_block(1003), None);
    }

    #[test]
    fn test_erae_group_basic_construction() {
        let blocks =
            vec![create_sample_block(10), create_sample_block(15), create_sample_block(20)];

        let root_bytes = [0xDD; 32];
        let accumulator = Accumulator::new(B256::from(root_bytes));
        let block_index = BlockIndex::new(1000, 2, vec![100, 200, 300, 400, 500, 600]);

        let erae_group = EraEGroup::new(blocks, accumulator.clone(), block_index);

        // Verify initial state
        assert_eq!(erae_group.blocks.len(), 3);
        assert_eq!(erae_group.other_entries.len(), 0);
        assert_eq!(erae_group.accumulator.root, accumulator.root);
        assert_eq!(erae_group.block_index.starting_number, 1000);
        assert_eq!(erae_group.block_index.offsets, vec![100, 200, 300, 400, 500, 600]);
    }

    #[test]
    fn test_erae_group_add_entries() {
        let blocks = vec![create_sample_block(10)];

        let root_bytes = [0xDD; 32];
        let accumulator = Accumulator::new(B256::from(root_bytes));

        let block_index = BlockIndex::new(1000, 2, vec![100, 200]);

        // Create and verify group
        let mut erae_group = EraEGroup::new(blocks, accumulator, block_index);
        assert_eq!(erae_group.other_entries.len(), 0);

        // Create custom entries with different types
        let entry1 = Entry::new([0x01, 0x01], vec![1, 2, 3, 4]);
        let entry2 = Entry::new([0x02, 0x02], vec![5, 6, 7, 8]);

        // Add those entries
        erae_group.add_entry(entry1);
        erae_group.add_entry(entry2);

        // Verify entries were added correctly
        assert_eq!(erae_group.other_entries.len(), 2);
        assert_eq!(erae_group.other_entries[0].entry_type, [0x01, 0x01]);
        assert_eq!(erae_group.other_entries[0].data, vec![1, 2, 3, 4]);
        assert_eq!(erae_group.other_entries[1].entry_type, [0x02, 0x02]);
        assert_eq!(erae_group.other_entries[1].data, vec![5, 6, 7, 8]);
    }

    #[test]
    fn test_erae_group_with_mismatched_index() {
        let blocks =
            vec![create_sample_block(10), create_sample_block(15), create_sample_block(20)];

        let root_bytes = [0xDD; 32];
        let accumulator = Accumulator::new(B256::from(root_bytes));

        // Create block index with different starting number
        let block_index = BlockIndex::new(2000, 2, vec![100, 200, 300, 400, 500, 600]);

        // This should create a valid EraEGroup
        // even though the block numbers don't match the block index
        // validation not at the erae group level
        let erae_group = EraEGroup::new(blocks, accumulator, block_index);

        // Verify the mismatch exists but the group was created
        assert_eq!(erae_group.blocks.len(), 3);
        assert_eq!(erae_group.block_index.starting_number, 2000);
    }

    #[test_case::test_case(
        EraEId::new("mainnet", 0, 8192).with_hash([0x5e, 0xc1, 0xff, 0xb8]),
        "mainnet-00000-5ec1ffb8.erae";
        "Mainnet era 0"
    )]
    #[test_case::test_case(
        EraEId::new("mainnet", 8192, 8192).with_hash([0x5e, 0xcb, 0x9b, 0xf9]),
        "mainnet-00001-5ecb9bf9.erae";
        "Mainnet era 1"
    )]
    #[test_case::test_case(
        EraEId::new("sepolia", 0, 8192).with_hash([0x90, 0x91, 0x84, 0x72]),
        "sepolia-00000-90918472.erae";
        "Sepolia era 0"
    )]
    #[test_case::test_case(
        EraEId::new("sepolia", 155648, 8192).with_hash([0xfa, 0x77, 0x00, 0x19]),
        "sepolia-00019-fa770019.erae";
        "Sepolia era 19"
    )]
    #[test_case::test_case(
        EraEId::new("mainnet", 1000, 100),
        "mainnet-00000-00000000.erae";
        "ID without hash"
    )]
    #[test_case::test_case(
        EraEId::new("sepolia", 101130240, 8192).with_hash([0xab, 0xcd, 0xef, 0x12]),
        "sepolia-12345-abcdef12.erae";
        "Large block number era 12345"
    )]
    fn test_erae_id_file_naming(id: EraEId, expected_file_name: &str) {
        let actual_file_name = id.to_file_name();
        assert_eq!(actual_file_name, expected_file_name);
    }

    // File naming with era-count, for custom exports
    #[test_case::test_case(
        EraEId::new("mainnet", 0, 8192).with_hash([0x5e, 0xc1, 0xff, 0xb8]).with_era_count(),
        "mainnet-00000-00001-5ec1ffb8.erae";
        "Mainnet era 0 with count"
    )]
    #[test_case::test_case(
        EraEId::new("mainnet", 8000, 500).with_hash([0xab, 0xcd, 0xef, 0x12]).with_era_count(),
        "mainnet-00000-00002-abcdef12.erae";
        "Spanning two eras with count"
    )]
    fn test_erae_id_file_naming_with_era_count(id: EraEId, expected_file_name: &str) {
        let actual_file_name = id.to_file_name();
        assert_eq!(actual_file_name, expected_file_name);
    }
}
