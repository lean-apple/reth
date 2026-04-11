//! Core `EraE` (Ere) primitives and file handling.
//!
//! `EraE` files store execution layer block history for both pre-merge and post-merge data.
//! Entries are grouped by type (all headers, then all bodies, etc.) for efficient range queries.
//!
//! See also <https://github.com/eth-clients/e2store-format-specs/blob/main/formats/ere.md>

pub mod file;
pub mod types;
