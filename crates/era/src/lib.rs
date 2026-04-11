//! Era and `EraE` files support for Ethereum history expiry.
//!
//! `EraE` (Ere) files store execution layer block history for both pre-merge and
//! post-merge data, following the format:
//! `Version | CompressedHeader+ | CompressedBody+ | CompressedSlimReceipts+ | Proofs+ |
//! TotalDifficulty* | other-entries* | Accumulator? | BlockIndex`
//!
//! Era files store consensus layer beacon chain data.
//!
//! Both are special instances of `.e2s` files with strict content formats
//! optimized for reading and long-term storage and distribution.
//!
//! See also:
//! - E2store format: <https://github.com/status-im/nimbus-eth2/blob/stable/docs/e2store.md>
//! - Era format: <https://github.com/eth-clients/e2store-format-specs/blob/main/formats/era.md>
//! - `EraE` format: <https://github.com/eth-clients/e2store-format-specs/blob/main/formats/ere.md>

pub mod common;
pub mod e2s;
pub mod era;
pub mod erae;

#[cfg(test)]
pub(crate) mod test_utils;
