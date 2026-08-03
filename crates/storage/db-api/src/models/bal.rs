//! Block access list table models.

use crate::{
    table::{Compress, Decode, Decompress, Encode},
    DatabaseError,
};
use alloy_eip7928::bal::RawBal;
use alloy_eips::NumHash;
use alloy_primitives::{keccak256, BlockNumber, Bytes, B256};
use bytes::BufMut;
use core::cmp::Ordering;
use reth_codecs::DecompressError;
use serde::{Deserialize, Serialize};

const BLOCK_ACCESS_LIST_KEY_BYTES: usize = 8 + 32;
const STORED_BLOCK_ACCESS_LIST_HASH_BYTES: usize = 32;

/// Block number/hash key ordered by number for efficient pruning.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(transparent)]
pub struct StoredBlockAccessListKey(NumHash);

impl StoredBlockAccessListKey {
    /// Creates a key from a block number/hash pair.
    pub const fn new(num_hash: NumHash) -> Self {
        Self(num_hash)
    }

    /// Returns the smallest key for the given block number.
    pub const fn first_at_number(block_number: BlockNumber) -> Self {
        Self::new(NumHash::new(block_number, B256::ZERO))
    }

    /// Returns the block number.
    pub const fn number(&self) -> BlockNumber {
        self.0.number
    }

    /// Returns the block number/hash pair.
    pub const fn num_hash(&self) -> NumHash {
        self.0
    }
}

impl Ord for StoredBlockAccessListKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .number
            .cmp(&other.0.number)
            .then_with(|| self.0.hash.as_slice().cmp(other.0.hash.as_slice()))
    }
}

impl PartialOrd for StoredBlockAccessListKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Encode for StoredBlockAccessListKey {
    type Encoded = [u8; BLOCK_ACCESS_LIST_KEY_BYTES];

    fn encode(self) -> Self::Encoded {
        let mut buf = [0u8; BLOCK_ACCESS_LIST_KEY_BYTES];
        buf[..8].copy_from_slice(&self.0.number.to_be_bytes());
        buf[8..].copy_from_slice(self.0.hash.as_slice());
        buf
    }
}

impl Decode for StoredBlockAccessListKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != BLOCK_ACCESS_LIST_KEY_BYTES {
            return Err(DatabaseError::Decode)
        }

        let number = u64::from_be_bytes(value[..8].try_into().map_err(|_| DatabaseError::Decode)?);
        let hash = B256::decode(&value[8..])?;
        Ok(Self::new(NumHash::new(number, hash)))
    }
}

/// Stored BAL bytes with an integrity hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredBlockAccessList {
    hash: B256,
    raw: RawBal,
}

impl StoredBlockAccessList {
    /// Creates a stored BAL from raw bytes.
    pub fn new(raw: RawBal) -> Self {
        let hash = keccak256(raw.as_raw());
        Self { hash, raw }
    }

    /// Returns the raw BAL after checking its integrity hash.
    pub fn into_verified_raw(self) -> Result<RawBal, StoredBlockAccessListHashError> {
        if keccak256(self.raw.as_raw()) == self.hash {
            Ok(self.raw)
        } else {
            Err(StoredBlockAccessListHashError)
        }
    }
}

/// Error returned when persisted BAL bytes fail their integrity check.
#[derive(Debug, derive_more::Display, derive_more::Error)]
#[display("stored block access list hash mismatch")]
pub struct StoredBlockAccessListHashError;

impl Compress for StoredBlockAccessList {
    type Compressed = Vec<u8>;

    fn compress(self) -> Self::Compressed {
        let mut out =
            Vec::with_capacity(STORED_BLOCK_ACCESS_LIST_HASH_BYTES + self.raw.as_raw().len());
        out.extend_from_slice(self.hash.as_slice());
        out.extend_from_slice(self.raw.as_raw());
        out
    }

    fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        buf.put_slice(self.hash.as_slice());
        buf.put_slice(self.raw.as_raw());
    }
}

impl Decompress for StoredBlockAccessList {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        if value.len() < STORED_BLOCK_ACCESS_LIST_HASH_BYTES {
            return Err(DecompressError::new(StoredBlockAccessListDecodeError))
        }

        let hash = B256::from_slice(&value[..STORED_BLOCK_ACCESS_LIST_HASH_BYTES]);
        let raw =
            RawBal::new(Bytes::copy_from_slice(&value[STORED_BLOCK_ACCESS_LIST_HASH_BYTES..]));
        Ok(Self { hash, raw })
    }
}

#[derive(Debug, derive_more::Display, derive_more::Error)]
#[display("stored block access list value is missing its hash prefix")]
struct StoredBlockAccessListDecodeError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip_preserves_order() {
        let low = StoredBlockAccessListKey::new(NumHash::new(1, B256::with_last_byte(0xff)));
        let high = StoredBlockAccessListKey::new(NumHash::new(2, B256::ZERO));

        assert!(low.encode() < high.encode());
        assert_eq!(StoredBlockAccessListKey::decode(&low.encode()).unwrap(), low);
    }

    #[test]
    fn stored_bal_roundtrip_checks_hash() {
        let raw = RawBal::from(Bytes::from_static(&[0xc0]));
        let encoded = StoredBlockAccessList::new(raw.clone()).compress();
        let decoded = StoredBlockAccessList::decompress(&encoded).unwrap();

        assert_eq!(decoded.into_verified_raw().unwrap(), raw);
    }

    #[test]
    fn stored_bal_rejects_hash_mismatch() {
        let mut encoded = B256::ZERO.to_vec();
        encoded.push(0xc0);

        assert!(StoredBlockAccessList::decompress(&encoded).unwrap().into_verified_raw().is_err());
    }
}
