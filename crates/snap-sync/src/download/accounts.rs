//! Account range and single-account requests.

use super::{StateDownloader, MAX_HASH};
use crate::{
    error::SnapSyncError, proof::verify_range_proof, MAX_REQUEST_ATTEMPTS,
    SNAP_RESPONSE_BYTES_LIMIT,
};
use alloy_primitives::{Bytes, B256};
use reth_db_api::transaction::DbTxMut;
use reth_eth_wire_types::snap::{AccountData, GetAccountRangeMessage};
use reth_network_p2p::snap::client::{SnapClient, SnapResponse};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::{TrieAccount, EMPTY_ROOT_HASH};

impl<C, F> StateDownloader<'_, C, F>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    /// Requests one account range, retrying with another peer when a response cannot be trusted.
    ///
    /// A peer that answers with the wrong message type, an unusable ordering or a proof that does
    /// not reconstruct the root is reported and the request reissued. Giving up on the first bad
    /// answer would let a single peer end the sync, so only exhausting the attempts is fatal.
    pub(super) async fn fetch_account_range(
        &mut self,
        cursor: B256,
    ) -> Result<AccountRange, SnapSyncError> {
        let mut last_error = None;
        let mut unavailable = false;

        for _ in 0..MAX_REQUEST_ATTEMPTS {
            let request_id = self.next_request_id();
            let response = match self
                .client
                .get_account_range(GetAccountRangeMessage {
                    request_id,
                    root_hash: self.root_hash,
                    starting_hash: cursor,
                    limit_hash: MAX_HASH,
                    response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
                })
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    // The request itself failed, so there is no peer response to hold against
                    // anyone; the network layer already accounts for the failure.
                    last_error = Some(SnapSyncError::Network(format!(
                        "snap account range request failed: {err}"
                    )));
                    continue
                }
            };

            let (peer, data) = response.split();
            let SnapResponse::AccountRange(msg) = data else {
                last_error = Some(self.penalize(
                    peer,
                    SnapSyncError::Network("expected an account range response".into()),
                ));
                continue
            };

            if msg.accounts.is_empty() {
                // An empty trie holds no accounts and has no proof to give, so a bare reply is
                // the only answer a correct server can send.
                if self.root_hash == EMPTY_ROOT_HASH {
                    return Ok(AccountRange::PastTheEnd)
                }
                // Otherwise this peer cannot serve the root; another still might, so spend an
                // attempt rather than ending the request here.
                if msg.proof.is_empty() {
                    unavailable = true;
                    continue
                }
                match self.verify_account_range(cursor, &[], &msg.proof) {
                    Ok(()) => return Ok(AccountRange::PastTheEnd),
                    Err(err) => {
                        last_error = Some(self.penalize(peer, err));
                        continue
                    }
                }
            }

            let accounts = match Self::decode_account_range(&msg.accounts, cursor) {
                Ok(accounts) => accounts,
                Err(err) => {
                    last_error = Some(self.penalize(peer, err));
                    continue
                }
            };
            if let Err(err) = self.verify_account_range(cursor, &accounts, &msg.proof) {
                last_error = Some(self.penalize(peer, err));
                continue
            }

            return Ok(AccountRange::Verified { accounts, exhausted: msg.proof.is_empty() })
        }

        if unavailable {
            return Ok(AccountRange::Unavailable)
        }
        Err(last_error.expect("at least one attempt was made"))
    }
}

// Checks that need neither a client nor a database.
impl<C, F> StateDownloader<'_, C, F> {
    /// Checks a served account range against the pivot root.
    fn verify_account_range(
        &self,
        origin: B256,
        accounts: &[(B256, TrieAccount)],
        proof: &[Bytes],
    ) -> Result<(), SnapSyncError> {
        let leaves = accounts.iter().map(|(hash, account)| (*hash, alloy_rlp::encode(account)));

        verify_range_proof(self.root_hash, origin, leaves, proof).map_err(|err| {
            SnapSyncError::Network(format!("invalid snap account range proof: {err}"))
        })
    }

    /// Decodes a served account range, rejecting orderings that would let a peer hide accounts.
    fn decode_account_range(
        accounts: &[AccountData],
        origin: B256,
    ) -> Result<Vec<(B256, TrieAccount)>, SnapSyncError> {
        let mut decoded = Vec::with_capacity(accounts.len());
        let mut previous = None;

        for account in accounts {
            if account.hash < origin {
                return Err(SnapSyncError::Network(
                    "snap account range returned an account before the requested origin".into(),
                ))
            }
            if previous.is_some_and(|previous| account.hash <= previous) {
                return Err(SnapSyncError::Network(
                    "snap account range returned non-monotonic account hashes".into(),
                ))
            }
            previous = Some(account.hash);

            let account_body = account.trie_account().map_err(|err| {
                SnapSyncError::RlpDecode(format!("snap slim account body: {err}"))
            })?;
            decoded.push((account.hash, account_body));
        }

        Ok(decoded)
    }
}

/// A verified account range, or the reason there is nothing to take from it.
pub(super) enum AccountRange {
    /// The peer could not serve the requested root.
    Unavailable,
    /// The requested origin is past the last account, proven by an absence proof.
    PastTheEnd,
    /// Accounts verified against the root; `exhausted` when no boundary proof was attached,
    /// meaning the range reached the end of the trie.
    Verified {
        /// Accounts in the order the peer served them.
        accounts: Vec<(B256, TrieAccount)>,
        /// Whether this range ran to the end of the trie.
        exhausted: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{KECCAK256_EMPTY, U256};
    use reth_trie::EMPTY_ROOT_HASH;

    type Downloader<'a> = StateDownloader<'a, (), ()>;

    fn b256(value: u64) -> B256 {
        B256::left_padding_from(&value.to_be_bytes())
    }

    fn account_data(hash: B256, nonce: u64) -> AccountData {
        AccountData::from_trie_account(
            hash,
            &TrieAccount {
                nonce,
                balance: U256::from(1),
                storage_root: EMPTY_ROOT_HASH,
                code_hash: KECCAK256_EMPTY,
            },
        )
    }

    #[test]
    fn account_range_round_trips_through_the_slim_encoding() {
        let decoded =
            Downloader::decode_account_range(&[account_data(b256(1), 7)], B256::ZERO).unwrap();

        assert_eq!(decoded[0].0, b256(1));
        assert_eq!(decoded[0].1.nonce, 7);
        assert_eq!(decoded[0].1.storage_root, EMPTY_ROOT_HASH);
        assert_eq!(decoded[0].1.code_hash, KECCAK256_EMPTY);
    }

    #[test]
    fn account_range_rejects_out_of_order_accounts() {
        let accounts = [account_data(b256(2), 0), account_data(b256(1), 0)];

        assert!(Downloader::decode_account_range(&accounts, B256::ZERO).is_err());
    }

    #[test]
    fn account_range_rejects_accounts_before_origin() {
        let accounts = [account_data(b256(1), 0)];

        assert!(Downloader::decode_account_range(&accounts, b256(2)).is_err());
    }
}
