//! Reth compatibility and utils for RPC types
//!
//! This crate various helper functions to convert between reth primitive types and rpc types.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod block;
mod fees;
pub mod receipt;
mod rpc;
pub mod transaction;

pub use block::TryFromBlockResponse;
pub use fees::{CallFees, CallFeesError};
pub use receipt::TryFromReceiptResponse;
pub use rpc::*;
pub use transaction::{
    EthTxEnvError, IntoRpcTx, RpcConvert, RpcConverter, TransactionConversionError,
    TryFromTransactionResponse, TryIntoSimTx, TxInfoMapper,
};

#[cfg(feature = "op")]
pub use transaction::op::*;
