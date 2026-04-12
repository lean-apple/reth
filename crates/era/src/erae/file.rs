//! Represents a complete `EraE` file
//!
//! The structure of an `EraE` file follows the specification:
//! `Version | CompressedHeader+ | CompressedBody+ | CompressedSlimReceipts+ | Proofs+ |
//! TotalDifficulty* | other-entries* | Accumulator? | BlockIndex`
//!
//! See also <https://github.com/eth-clients/e2store-format-specs/blob/main/formats/ere.md>.

use crate::{
    common::file_ops::{EraFileFormat, FileReader, StreamReader, StreamWriter},
    e2s::{
        error::E2sError,
        file::{E2StoreReader, E2StoreWriter},
        types::{Entry, Version},
    },
    erae::types::{
        execution::{
            Accumulator, BlockTuple, CompressedBody, CompressedHeader, CompressedSlimReceipts,
            TotalDifficulty, ACCUMULATOR, COMPRESSED_BODY, COMPRESSED_HEADER,
            COMPRESSED_SLIM_RECEIPTS, MAX_BLOCKS_PER_ERAE, TOTAL_DIFFICULTY,
        },
        group::{BlockIndex, EraEGroup, EraEId, BLOCK_INDEX},
    },
};
use alloy_primitives::BlockNumber;
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, Write},
};

/// `EraE` file interface
#[derive(Debug)]
pub struct EraEFile {
    /// Version record, must be the first record in the file
    pub version: Version,

    /// Main content group of the `EraE` file
    pub group: EraEGroup,

    /// File identifier
    pub id: EraEId,
}

impl EraFileFormat for EraEFile {
    type EraGroup = EraEGroup;
    type Id = EraEId;

    /// Create a new [`EraEFile`]
    fn new(group: EraEGroup, id: EraEId) -> Self {
        Self { version: Version, group, id }
    }

    fn version(&self) -> &Version {
        &self.version
    }

    fn group(&self) -> &Self::EraGroup {
        &self.group
    }

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl EraEFile {
    /// Get a block by its number, if present in this file
    pub fn get_block_by_number(&self, number: BlockNumber) -> Option<&BlockTuple> {
        let index = (number - self.group.block_index.starting_number()) as usize;
        (index < self.group.blocks.len()).then(|| &self.group.blocks[index])
    }

    /// Get the range of block numbers contained in this file
    pub const fn block_range(&self) -> std::ops::RangeInclusive<BlockNumber> {
        let start = self.group.block_index.starting_number();
        let end = start + (self.group.blocks.len() as u64) - 1;
        start..=end
    }

    /// Check if this file contains a specific block number
    pub fn contains_block(&self, number: BlockNumber) -> bool {
        self.block_range().contains(&number)
    }
}

/// Reader for `EraE` files that builds on top of [`E2StoreReader`]
#[derive(Debug)]
pub struct EraEReader<R: Read> {
    reader: E2StoreReader<R>,
}

/// An iterator of [`BlockTuple`] streaming from [`E2StoreReader`].
#[derive(Debug)]
pub struct BlockTupleIterator<R: Read> {
    reader: E2StoreReader<R>,
    headers: VecDeque<CompressedHeader>,
    bodies: VecDeque<CompressedBody>,
    receipts: VecDeque<CompressedSlimReceipts>,
    difficulties: VecDeque<TotalDifficulty>,
    other_entries: Vec<Entry>,
    accumulator: Option<Accumulator>,
    block_index: Option<BlockIndex>,
}

impl<R: Read> BlockTupleIterator<R> {
    fn new(reader: E2StoreReader<R>) -> Self {
        Self {
            reader,
            headers: Default::default(),
            bodies: Default::default(),
            receipts: Default::default(),
            difficulties: Default::default(),
            other_entries: Default::default(),
            accumulator: None,
            block_index: None,
        }
    }
}

impl<R: Read + Seek> Iterator for BlockTupleIterator<R> {
    type Item = Result<BlockTuple, E2sError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_result().transpose()
    }
}

impl<R: Read + Seek> BlockTupleIterator<R> {
    fn next_result(&mut self) -> Result<Option<BlockTuple>, E2sError> {
        loop {
            let Some(entry) = self.reader.read_next_entry()? else {
                return Ok(None);
            };

            match entry.entry_type {
                COMPRESSED_HEADER => {
                    self.headers.push_back(CompressedHeader::from_entry(&entry)?);
                }
                COMPRESSED_BODY => {
                    self.bodies.push_back(CompressedBody::from_entry(&entry)?);
                }
                COMPRESSED_SLIM_RECEIPTS => {
                    self.receipts.push_back(CompressedSlimReceipts::from_entry(&entry)?);
                }
                TOTAL_DIFFICULTY => {
                    self.difficulties.push_back(TotalDifficulty::from_entry(&entry)?);
                }
                ACCUMULATOR => {
                    if self.accumulator.is_some() {
                        return Err(E2sError::Ssz("Multiple accumulator entries found".to_string()));
                    }
                    self.accumulator = Some(Accumulator::from_entry(&entry)?);
                }
                BLOCK_INDEX => {
                    if self.block_index.is_some() {
                        return Err(E2sError::Ssz("Multiple block index entries found".to_string()));
                    }
                    self.block_index = Some(BlockIndex::from_entry(&entry)?);
                }
                _ => {
                    self.other_entries.push(entry);
                }
            }

            if !self.headers.is_empty() && !self.bodies.is_empty() && !self.receipts.is_empty() {
                let header = self.headers.pop_front().unwrap();
                let body = self.bodies.pop_front().unwrap();
                let receipt = self.receipts.pop_front().unwrap();
                // Difficulties are optional (post-merge files have none)
                let difficulty = self
                    .difficulties
                    .pop_front()
                    .unwrap_or(TotalDifficulty::new(alloy_primitives::U256::ZERO));

                return Ok(Some(BlockTuple::new(header, body, receipt, difficulty)));
            }
        }
    }
}

impl<R: Read + Seek> StreamReader<R> for EraEReader<R> {
    type File = EraEFile;
    type Iterator = BlockTupleIterator<R>;

    /// Create a new [`EraEReader`]
    fn new(reader: R) -> Self {
        Self { reader: E2StoreReader::new(reader) }
    }

    /// Returns an iterator of [`BlockTuple`] streaming from `reader`.
    fn iter(self) -> BlockTupleIterator<R> {
        BlockTupleIterator::new(self.reader)
    }

    fn read(self, network_name: String) -> Result<Self::File, E2sError> {
        self.read_and_assemble(network_name)
    }
}

impl<R: Read + Seek> EraEReader<R> {
    /// Reads and parses an `EraE` file from the underlying reader, assembling all components
    /// into a complete [`EraEFile`] with an [`EraEId`] that includes the provided network name.
    pub fn read_and_assemble(mut self, network_name: String) -> Result<EraEFile, E2sError> {
        // Validate version entry
        let _version_entry = match self.reader.read_version()? {
            Some(entry) if entry.is_version() => entry,
            Some(_) => return Err(E2sError::Ssz("First entry is not a Version entry".to_string())),
            None => return Err(E2sError::Ssz("Empty EraE file".to_string())),
        };

        // Read all entries into separate vecs by type.
        // EraE groups entries by type (all headers, then all bodies, etc.)
        // so we can't use a streaming iterator that expects interleaved tuples.
        let mut headers = Vec::new();
        let mut bodies = Vec::new();
        let mut receipts = Vec::new();
        let mut difficulties = Vec::new();
        let mut other_entries = Vec::new();
        let mut accumulator = None;
        let mut block_index = None;

        while let Some(entry) = self.reader.read_next_entry()? {
            match entry.entry_type {
                COMPRESSED_HEADER => {
                    headers.push(CompressedHeader::from_entry(&entry)?);
                }
                COMPRESSED_BODY => {
                    bodies.push(CompressedBody::from_entry(&entry)?);
                }
                COMPRESSED_SLIM_RECEIPTS => {
                    receipts.push(CompressedSlimReceipts::from_entry(&entry)?);
                }
                TOTAL_DIFFICULTY => {
                    difficulties.push(TotalDifficulty::from_entry(&entry)?);
                }
                ACCUMULATOR => {
                    if accumulator.is_some() {
                        return Err(E2sError::Ssz("Multiple accumulator entries found".to_string()));
                    }
                    accumulator = Some(Accumulator::from_entry(&entry)?);
                }
                BLOCK_INDEX => {
                    if block_index.is_some() {
                        return Err(E2sError::Ssz("Multiple block index entries found".to_string()));
                    }
                    block_index = Some(BlockIndex::from_entry(&entry)?);
                }
                _ => {
                    other_entries.push(entry);
                }
            }
        }

        // Headers and bodies are required and must match
        if headers.len() != bodies.len() {
            return Err(E2sError::Ssz(format!(
                "Mismatched header/body counts: headers={}, bodies={}",
                headers.len(),
                bodies.len()
            )));
        }

        let block_index = block_index
            .ok_or_else(|| E2sError::Ssz("EraE file missing block index entry".to_string()))?;

        // Zip components into BlockTuples
        let blocks: Vec<BlockTuple> = headers
            .into_iter()
            .zip(bodies)
            .enumerate()
            .map(|(i, (header, body))| {
                let receipt = if i < receipts.len() {
                    receipts[i].clone()
                } else {
                    CompressedSlimReceipts::new(vec![])
                };
                let difficulty = if i < difficulties.len() {
                    difficulties[i].clone()
                } else {
                    TotalDifficulty::new(alloy_primitives::U256::ZERO)
                };
                BlockTuple::new(header, body, receipt, difficulty)
            })
            .collect();

        let mut group = EraEGroup::new(blocks, accumulator, block_index.clone());

        // Add other entries
        for entry in other_entries {
            group.add_entry(entry);
        }

        let id = EraEId::new(
            network_name,
            block_index.starting_number(),
            block_index.block_count() as u32,
        );

        Ok(EraEFile::new(group, id))
    }
}

impl FileReader for EraEReader<File> {}

/// Writer for `EraE` files that builds on top of [`E2StoreWriter`]
#[derive(Debug)]
pub struct EraEWriter<W: Write> {
    writer: E2StoreWriter<W>,
    has_written_version: bool,
    has_written_blocks: bool,
    has_written_accumulator: bool,
    has_written_block_index: bool,
}

impl<W: Write> StreamWriter<W> for EraEWriter<W> {
    type File = EraEFile;

    /// Create a new [`EraEWriter`]
    fn new(writer: W) -> Self {
        Self {
            writer: E2StoreWriter::new(writer),
            has_written_version: false,
            has_written_blocks: false,
            has_written_accumulator: false,
            has_written_block_index: false,
        }
    }

    /// Write the version entry
    fn write_version(&mut self) -> Result<(), E2sError> {
        if self.has_written_version {
            return Ok(());
        }

        self.writer.write_version()?;
        self.has_written_version = true;
        Ok(())
    }

    /// Write a complete [`EraEFile`] to the underlying writer
    fn write_file(&mut self, erae_file: &EraEFile) -> Result<(), E2sError> {
        // Write version
        self.write_version()?;

        // Ensure blocks are written before other entries
        if erae_file.group.blocks.len() > MAX_BLOCKS_PER_ERAE {
            return Err(E2sError::Ssz("EraE file cannot contain more than 8192 blocks".to_string()));
        }

        // Write all blocks
        for block in &erae_file.group.blocks {
            self.write_block(block)?;
        }

        // Write other entries
        for entry in &erae_file.group.other_entries {
            self.writer.write_entry(entry)?;
        }

        // Write accumulator (optional, pre-merge only)
        if let Some(accumulator) = &erae_file.group.accumulator {
            self.write_accumulator(accumulator)?;
        }

        // Write block index
        self.write_block_index(&erae_file.group.block_index)?;

        // Flush the writer
        self.writer.flush()?;

        Ok(())
    }

    /// Flush any buffered data to the underlying writer
    fn flush(&mut self) -> Result<(), E2sError> {
        self.writer.flush()
    }
}

impl<W: Write> EraEWriter<W> {
    /// Write a single block tuple
    pub fn write_block(&mut self, block_tuple: &BlockTuple) -> Result<(), E2sError> {
        if !self.has_written_version {
            self.write_version()?;
        }

        if self.has_written_accumulator || self.has_written_block_index {
            return Err(E2sError::Ssz(
                "Cannot write blocks after accumulator or block index".to_string(),
            ));
        }

        // Write header
        let header_entry = block_tuple.header.to_entry();
        self.writer.write_entry(&header_entry)?;

        // Write body
        let body_entry = block_tuple.body.to_entry();
        self.writer.write_entry(&body_entry)?;

        // Write receipts
        let receipts_entry = block_tuple.receipts.to_entry();
        self.writer.write_entry(&receipts_entry)?;

        // Write difficulty
        let difficulty_entry = block_tuple.total_difficulty.to_entry();
        self.writer.write_entry(&difficulty_entry)?;

        self.has_written_blocks = true;

        Ok(())
    }

    /// Write the block index
    pub fn write_block_index(&mut self, block_index: &BlockIndex) -> Result<(), E2sError> {
        if !self.has_written_version {
            self.write_version()?;
        }

        if self.has_written_block_index {
            return Err(E2sError::Ssz("Block index already written".to_string()));
        }

        let block_index_entry = block_index.to_entry();
        self.writer.write_entry(&block_index_entry)?;
        self.has_written_block_index = true;

        Ok(())
    }

    /// Write the accumulator
    pub fn write_accumulator(&mut self, accumulator: &Accumulator) -> Result<(), E2sError> {
        if !self.has_written_version {
            self.write_version()?;
        }

        if self.has_written_accumulator {
            return Err(E2sError::Ssz("Accumulator already written".to_string()));
        }

        if self.has_written_block_index {
            return Err(E2sError::Ssz("Cannot write accumulator after block index".to_string()));
        }

        let accumulator_entry = accumulator.to_entry();
        self.writer.write_entry(&accumulator_entry)?;
        self.has_written_accumulator = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::file_ops::FileWriter;
    use alloy_primitives::{B256, U256};
    use std::io::Cursor;
    use tempfile::tempdir;

    // Helper to create a sample block tuple for testing
    fn create_test_block(number: BlockNumber, data_size: usize) -> BlockTuple {
        let header_data = vec![(number % 256) as u8; data_size];
        let header = CompressedHeader::new(header_data);

        let body_data = vec![((number + 1) % 256) as u8; data_size * 2];
        let body = CompressedBody::new(body_data);

        let receipts_data = vec![((number + 2) % 256) as u8; data_size];
        let receipts = CompressedSlimReceipts::new(receipts_data);

        let difficulty = TotalDifficulty::new(U256::from(number * 1000));

        BlockTuple::new(header, body, receipts, difficulty)
    }

    // Helper to create a sample EraEFile for testing
    fn create_test_erae_file(
        start_block: BlockNumber,
        block_count: usize,
        network: &str,
    ) -> EraEFile {
        // Create blocks
        let mut blocks = Vec::with_capacity(block_count);
        for i in 0..block_count {
            let block_num = start_block + i as u64;
            blocks.push(create_test_block(block_num, 32));
        }

        let accumulator = Some(Accumulator::new(B256::from([0xAA; 32])));

        let component_count = 4u64; // header + body + receipts + td
        let mut offsets = Vec::with_capacity(block_count * component_count as usize);
        for i in 0..block_count {
            for c in 0..component_count as usize {
                offsets.push((i * component_count as usize + c) as i64 * 100);
            }
        }
        let block_index = BlockIndex::new(start_block, component_count, offsets);
        let group = EraEGroup::new(blocks, accumulator, block_index);
        let id = EraEId::new(network, start_block, block_count as u32);

        EraEFile::new(group, id)
    }

    #[test]
    fn test_erae_roundtrip_memory() -> Result<(), E2sError> {
        // Create a test EraEFile
        let start_block = 1000;
        let erae_file = create_test_erae_file(1000, 5, "testnet");

        // Write to memory buffer
        let mut buffer = Vec::new();
        {
            let mut writer = EraEWriter::new(&mut buffer);
            writer.write_file(&erae_file)?;
        }

        // Read back from memory buffer
        let reader = EraEReader::new(Cursor::new(&buffer));
        let read_erae = reader.read("testnet".to_string())?;

        // Verify core properties
        assert_eq!(read_erae.id.network_name, "testnet");
        assert_eq!(read_erae.id.start_block, 1000);
        assert_eq!(read_erae.id.block_count, 5);
        assert_eq!(read_erae.group.blocks.len(), 5);

        // Verify block properties
        assert_eq!(read_erae.group.blocks[0].total_difficulty.value, U256::from(1000 * 1000));
        assert_eq!(read_erae.group.blocks[1].total_difficulty.value, U256::from(1001 * 1000));

        // Verify block data
        assert_eq!(read_erae.group.blocks[0].header.data, vec![(start_block % 256) as u8; 32]);
        assert_eq!(read_erae.group.blocks[0].body.data, vec![((start_block + 1) % 256) as u8; 64]);
        assert_eq!(
            read_erae.group.blocks[0].receipts.data,
            vec![((start_block + 2) % 256) as u8; 32]
        );

        // Verify block access methods
        assert!(read_erae.contains_block(1000));
        assert!(read_erae.contains_block(1004));
        assert!(!read_erae.contains_block(999));
        assert!(!read_erae.contains_block(1005));

        let block_1002 = read_erae.get_block_by_number(1002);
        assert!(block_1002.is_some());
        assert_eq!(block_1002.unwrap().header.data, vec![((start_block + 2) % 256) as u8; 32]);

        Ok(())
    }

    #[test]
    fn test_erae_roundtrip_file() -> Result<(), E2sError> {
        // Create a temporary directory
        let temp_dir = tempdir().expect("Failed to create temp directory");
        let file_path = temp_dir.path().join("test_roundtrip.erae");

        // Create and write `EraEFile` to disk
        let erae_file = create_test_erae_file(2000, 3, "mainnet");
        EraEWriter::create(&file_path, &erae_file)?;

        // Read it back
        let read_erae = EraEReader::open(&file_path, "mainnet")?;

        // Verify core properties
        assert_eq!(read_erae.id.network_name, "mainnet");
        assert_eq!(read_erae.id.start_block, 2000);
        assert_eq!(read_erae.id.block_count, 3);
        assert_eq!(read_erae.group.blocks.len(), 3);

        // Verify blocks
        for i in 0..3 {
            let block_num = 2000 + i as u64;
            let block = read_erae.get_block_by_number(block_num);
            assert!(block.is_some());
            assert_eq!(block.unwrap().header.data, vec![block_num as u8; 32]);
        }

        Ok(())
    }
}
