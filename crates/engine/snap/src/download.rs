//! Streaming download of accounts, storage and bytecodes at a fixed state root.
//!
//! [`download_state`] walks the account trie in hashed order. Each account batch is verified
//! against the pivot root, written, and immediately followed by that batch's storage and
//! bytecodes before the next range is requested, so peak memory stays at one batch regardless of
//! how large the state is.

use crate::{
    proof::verify_range_proof,
    storage::{increment_b256, SnapStateWriter},
    SnapSyncError, SNAP_RESPONSE_BYTES_LIMIT,
};
use alloy_primitives::{keccak256, Bytes, B256, KECCAK256_EMPTY, U256};
use alloy_rlp::{Decodable, RlpDecodable};
use reth_db_api::transaction::DbTxMut;
use reth_eth_wire_types::snap::{
    AccountData, GetAccountRangeMessage, GetByteCodesMessage, GetStorageRangesMessage, StorageData,
};
use reth_network_p2p::snap::client::{SnapClient, SnapResponse};
use reth_primitives_traits::Account;
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};
use reth_trie::{TrieAccount, EMPTY_ROOT_HASH};
use std::collections::{HashMap, HashSet};
use tracing::debug;

/// Maximum number of account hashes per storage range request.
const STORAGE_BATCH_SIZE: usize = 20;

/// Maximum number of code hashes per bytecode request.
const BYTECODE_BATCH_SIZE: usize = 50;

/// Upper bound of the hashed key space.
const MAX_HASH: B256 = B256::new([0xff; 32]);

type DecodedStorageSlots = Vec<(B256, U256)>;

/// Result of a [`download_state`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum DownloadStateOutcome {
    /// The whole account range was iterated and written; the state at the root is complete.
    Done,
    /// No peer could serve the requested root any more.
    ///
    /// Carries the account hash to resume from once the caller has a fresher root. State written
    /// before this point stays valid, because every batch was verified against the root it was
    /// served at.
    Stale {
        /// Account hash to resume the download from.
        resume_from: B256,
    },
}

/// Downloads accounts, storage and bytecodes at `root_hash`, starting from `starting_hash`.
pub async fn download_state<C, F>(
    client: &C,
    factory: &F,
    root_hash: B256,
    starting_hash: B256,
) -> Result<DownloadStateOutcome, SnapSyncError>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory + Clone + Send + Sync + 'static,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let writer = SnapStateWriter::new(factory);
    let mut request_id: u64 = 0;
    let mut cursor = starting_hash;

    loop {
        // Retrying a stale root restarts at the batch boundary, not mid-batch, so an account's
        // storage and code are never left half-written against a root we stopped trusting.
        let batch_start = cursor;

        request_id += 1;
        let response = client
            .get_account_range(GetAccountRangeMessage {
                request_id,
                root_hash,
                starting_hash: cursor,
                limit_hash: MAX_HASH,
                response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
            })
            .await
            .map_err(|err| {
                SnapSyncError::Network(format!("snap account range request failed: {err}"))
            })?;

        let SnapResponse::AccountRange(msg) = response.into_data() else {
            return Err(SnapSyncError::Network("expected an account range response".into()))
        };

        if msg.accounts.is_empty() {
            // A server that cannot serve the root replies fully empty; an absence proof instead
            // means the range really is past the last account.
            if msg.proof.is_empty() {
                return Ok(DownloadStateOutcome::Stale { resume_from: cursor })
            }
            verify_account_range_proof(root_hash, cursor, &[], &msg.proof)?;
            return Ok(DownloadStateOutcome::Done)
        }

        let decoded = decode_account_range(&msg.accounts, cursor)?;
        verify_account_range_proof(root_hash, cursor, &decoded, &msg.proof)?;

        let accounts: Vec<(B256, Account)> =
            decoded.iter().map(|(hash, account)| (*hash, Account::from(*account))).collect();
        let account_hashes: Vec<B256> = decoded.iter().map(|(hash, _)| *hash).collect();
        let storage_roots: HashMap<B256, B256> =
            decoded.iter().map(|(hash, account)| (*hash, account.storage_root)).collect();
        let code_hashes: HashSet<B256> = accounts
            .iter()
            .filter_map(|(_, account)| account.bytecode_hash)
            .filter(|hash| *hash != KECCAK256_EMPTY)
            .collect();

        debug!(
            target: "engine::snap",
            accounts = accounts.len(),
            %root_hash,
            "Downloaded account range"
        );
        writer.write_accounts(&accounts)?;

        if fetch_storage_for_accounts(
            client,
            writer,
            root_hash,
            &account_hashes,
            &storage_roots,
            &mut request_id,
        )
        .await?
        {
            return Ok(DownloadStateOutcome::Stale { resume_from: batch_start })
        }

        fetch_bytecodes(client, writer, &code_hashes, &mut request_id).await?;

        // No boundary proof means the server exhausted the trie from a zero origin, which
        // `verify_account_range_proof` already checked against the root.
        let last_hash = account_hashes.last().copied().expect("checked non-empty above");
        if msg.proof.is_empty() || last_hash == MAX_HASH {
            return Ok(DownloadStateOutcome::Done)
        }
        cursor = increment_b256(last_hash);
    }
}

/// Fetches and writes storage for one account batch.
///
/// Returns `Ok(true)` when the serving peer no longer has the root, `Ok(false)` when the whole
/// batch was written.
async fn fetch_storage_for_accounts<C, F>(
    client: &C,
    writer: SnapStateWriter<'_, F>,
    root_hash: B256,
    account_hashes: &[B256],
    storage_roots: &HashMap<B256, B256>,
    request_id: &mut u64,
) -> Result<bool, SnapSyncError>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory + Clone + Send + Sync + 'static,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let mut idx = 0;

    while idx < account_hashes.len() {
        let end = (idx + STORAGE_BATCH_SIZE).min(account_hashes.len());
        let chunk = &account_hashes[idx..end];

        *request_id += 1;
        let response = client
            .get_storage_ranges(GetStorageRangesMessage {
                request_id: *request_id,
                root_hash,
                account_hashes: chunk.to_vec(),
                starting_hash: B256::ZERO.into(),
                limit_hash: MAX_HASH.into(),
                response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
            })
            .await
            .map_err(|err| {
                SnapSyncError::Network(format!("snap storage range request failed: {err}"))
            })?;

        let SnapResponse::StorageRanges(msg) = response.into_data() else {
            return Err(SnapSyncError::Network("expected a storage ranges response".into()))
        };

        if msg.slots.len() > chunk.len() {
            return Err(SnapSyncError::Network(
                "snap storage range returned more slot lists than requested".into(),
            ))
        }

        // Servers answer with nothing at all when an account is missing at this root, rather than
        // skipping it and shifting the rest, so an empty response means the root went stale.
        let returned = msg.slots.len();
        if returned == 0 {
            return Ok(true)
        }

        // A proof is only attached to the last returned account, and only when its range is
        // partial; everything before it is a complete zero-origin range.
        let truncated_index = (!msg.proof.is_empty()).then_some(returned - 1);
        let mut entries = Vec::new();

        for (i, slots) in msg.slots.iter().enumerate() {
            let account_hash = chunk[i];
            validate_storage_slots(account_hash, B256::ZERO, slots)?;

            let account_slots = if Some(i) == truncated_index {
                let decoded = verify_storage_range_proof(
                    account_hash,
                    storage_roots,
                    B256::ZERO,
                    slots,
                    &msg.proof,
                )?;

                match slots.last() {
                    Some(last) => {
                        match fetch_storage_continuation(
                            client,
                            root_hash,
                            account_hash,
                            storage_roots,
                            increment_b256(last.hash),
                            request_id,
                            decoded,
                        )
                        .await?
                        {
                            StorageContinuationOutcome::Complete(slots) => slots,
                            StorageContinuationOutcome::Stale => return Ok(true),
                        }
                    }
                    // An empty slot list with a proof is an absence proof for the whole storage
                    // trie, already checked above.
                    None => decoded,
                }
            } else {
                verify_full_storage_range(account_hash, storage_roots, slots)?
            };

            entries.extend(
                account_slots
                    .into_iter()
                    .map(|(slot_hash, value)| (account_hash, slot_hash, value)),
            );
        }

        if !entries.is_empty() {
            writer.write_storages(&entries)?;
        }

        idx += returned;
    }

    Ok(false)
}

/// Outcome of continuing a single account's truncated storage range.
enum StorageContinuationOutcome {
    /// The account's storage is complete and matches its storage root.
    Complete(DecodedStorageSlots),
    /// The serving peer no longer has the requested root.
    Stale,
}

/// Requests the remainder of one account's storage until it verifies against its storage root.
async fn fetch_storage_continuation<C>(
    client: &C,
    root_hash: B256,
    account_hash: B256,
    storage_roots: &HashMap<B256, B256>,
    mut starting_hash: B256,
    request_id: &mut u64,
    mut collected: DecodedStorageSlots,
) -> Result<StorageContinuationOutcome, SnapSyncError>
where
    C: SnapClient + 'static,
{
    loop {
        *request_id += 1;
        let response = client
            .get_storage_ranges(GetStorageRangesMessage {
                request_id: *request_id,
                root_hash,
                account_hashes: vec![account_hash],
                starting_hash: starting_hash.into(),
                limit_hash: MAX_HASH.into(),
                response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
            })
            .await
            .map_err(|err| {
                SnapSyncError::Network(format!("snap storage continuation failed: {err}"))
            })?;

        let SnapResponse::StorageRanges(msg) = response.into_data() else {
            return Err(SnapSyncError::Network("expected a storage ranges response".into()))
        };

        if msg.slots.len() > 1 {
            return Err(SnapSyncError::Network(
                "snap storage continuation returned multiple slot lists".into(),
            ))
        }

        let Some(slots) = msg.slots.first() else { return Ok(StorageContinuationOutcome::Stale) };

        validate_storage_slots(account_hash, starting_hash, slots)?;
        collected.extend(verify_storage_range_proof(
            account_hash,
            storage_roots,
            starting_hash,
            slots,
            &msg.proof,
        )?);

        // Without a boundary proof the peer reached the end of this account's storage.
        let Some(last) = slots.last().filter(|_| !msg.proof.is_empty()) else {
            verify_storage_root(account_hash, storage_roots, &collected)?;
            return Ok(StorageContinuationOutcome::Complete(collected))
        };

        starting_hash = increment_b256(last.hash);
    }
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
        decoded.push((account.hash, decode_slim_account(&account.body)?));
    }

    Ok(decoded)
}

/// Expands snap/2's slim account encoding back into the trie's account representation.
///
/// The slim form omits the storage root and code hash when they are the empty defaults; the trie
/// leaf that the range proof commits to always carries them in full.
fn decode_slim_account(body: &Bytes) -> Result<TrieAccount, SnapSyncError> {
    let slim = SlimAccountBody::decode(&mut body.as_ref())
        .map_err(|err| SnapSyncError::RlpDecode(format!("slim account body: {err}")))?;

    let storage_root = match slim.storage_root.len() {
        0 => EMPTY_ROOT_HASH,
        32 => B256::from_slice(&slim.storage_root),
        _ => return Err(SnapSyncError::RlpDecode("slim account storage root length".into())),
    };
    let code_hash = match slim.code_hash.len() {
        0 => KECCAK256_EMPTY,
        32 => B256::from_slice(&slim.code_hash),
        _ => return Err(SnapSyncError::RlpDecode("slim account code hash length".into())),
    };

    Ok(TrieAccount { nonce: slim.nonce, balance: slim.balance, storage_root, code_hash })
}

/// Owned decode counterpart of the server's slim account encoding.
#[derive(Debug, RlpDecodable)]
struct SlimAccountBody {
    nonce: u64,
    balance: U256,
    /// Empty when the account has no storage.
    storage_root: Bytes,
    /// Empty when the account has no code.
    code_hash: Bytes,
}

fn validate_storage_slots(
    account_hash: B256,
    starting_hash: B256,
    slots: &[StorageData],
) -> Result<(), SnapSyncError> {
    let mut previous = None;
    for slot in slots {
        if slot.hash < starting_hash {
            return Err(SnapSyncError::Network(format!(
                "snap storage range for account {account_hash} returned a slot before the origin"
            )))
        }
        if previous.is_some_and(|previous| slot.hash <= previous) {
            return Err(SnapSyncError::Network(format!(
                "snap storage range for account {account_hash} returned non-monotonic slots"
            )))
        }
        previous = Some(slot.hash);
    }
    Ok(())
}

fn verify_account_range_proof(
    root_hash: B256,
    origin: B256,
    accounts: &[(B256, TrieAccount)],
    proof: &[Bytes],
) -> Result<(), SnapSyncError> {
    let leaves = accounts.iter().map(|(hash, account)| (*hash, alloy_rlp::encode(account)));

    verify_range_proof(root_hash, origin, leaves, proof)
        .map_err(|err| SnapSyncError::Network(format!("invalid snap account range proof: {err}")))
}

/// Verifies a partial storage range against its boundary proof and returns the decoded slots.
fn verify_storage_range_proof(
    account_hash: B256,
    storage_roots: &HashMap<B256, B256>,
    origin: B256,
    slots: &[StorageData],
    proof: &[Bytes],
) -> Result<DecodedStorageSlots, SnapSyncError> {
    let storage_root = storage_root_of(account_hash, storage_roots)?;
    let decoded = decode_storage_slots(slots)?;
    // The trie leaf is the RLP-encoded slot value, which is exactly what the server sent.
    let leaves = slots.iter().map(|slot| (slot.hash, slot.data.clone()));

    verify_range_proof(storage_root, origin, leaves, proof).map_err(|err| {
        SnapSyncError::Network(format!("invalid snap storage range proof: {err}"))
    })?;

    Ok(decoded)
}

/// Verifies that `slots` is the complete storage trie for `account_hash`.
fn verify_full_storage_range(
    account_hash: B256,
    storage_roots: &HashMap<B256, B256>,
    slots: &[StorageData],
) -> Result<DecodedStorageSlots, SnapSyncError> {
    let decoded = decode_storage_slots(slots)?;
    verify_storage_root(account_hash, storage_roots, &decoded)?;
    Ok(decoded)
}

/// Rebuilds the storage trie from `slots` and checks it against the account's storage root.
fn verify_storage_root(
    account_hash: B256,
    storage_roots: &HashMap<B256, B256>,
    slots: &DecodedStorageSlots,
) -> Result<(), SnapSyncError> {
    let storage_root = storage_root_of(account_hash, storage_roots)?;
    let leaves = slots
        .iter()
        .map(|(hash, value)| (*hash, alloy_rlp::encode_fixed_size(value).as_ref().to_vec()));

    verify_range_proof(storage_root, B256::ZERO, leaves, &[]).map_err(|err| {
        SnapSyncError::Network(format!(
            "snap storage for account {account_hash} does not match its storage root: {err}"
        ))
    })
}

fn storage_root_of(
    account_hash: B256,
    storage_roots: &HashMap<B256, B256>,
) -> Result<B256, SnapSyncError> {
    storage_roots.get(&account_hash).copied().ok_or_else(|| {
        SnapSyncError::Network(format!(
            "snap storage response for unrequested account {account_hash}"
        ))
    })
}

fn decode_storage_slots(slots: &[StorageData]) -> Result<DecodedStorageSlots, SnapSyncError> {
    slots
        .iter()
        .map(|slot| {
            let value = U256::decode(&mut slot.data.as_ref())
                .map_err(|err| SnapSyncError::RlpDecode(format!("snap storage slot: {err}")))?;
            Ok((slot.hash, value))
        })
        .collect()
}

/// Fetches and writes bytecodes for a set of code hashes.
async fn fetch_bytecodes<C, F>(
    client: &C,
    writer: SnapStateWriter<'_, F>,
    code_hashes: &HashSet<B256>,
    request_id: &mut u64,
) -> Result<(), SnapSyncError>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory + Clone + Send + Sync + 'static,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let hashes: Vec<B256> = code_hashes.iter().copied().collect();

    for chunk in hashes.chunks(BYTECODE_BATCH_SIZE) {
        *request_id += 1;
        let response = client
            .get_byte_codes(GetByteCodesMessage {
                request_id: *request_id,
                hashes: chunk.to_vec(),
                response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
            })
            .await
            .map_err(|err| {
                SnapSyncError::Network(format!("snap bytecode request failed: {err}"))
            })?;

        let SnapResponse::ByteCodes(msg) = response.into_data() else {
            return Err(SnapSyncError::Network("expected a byte codes response".into()))
        };

        let codes = match_bytecodes_to_hashes(chunk, &msg.codes)?;
        if !codes.is_empty() {
            writer.write_bytecodes(&codes)?;
        }
    }

    Ok(())
}

/// Pairs returned bytecodes with the hashes that were requested.
///
/// Servers may drop entries they don't have, but must keep request order, so a short reply is a
/// valid prefix while a reordered or duplicated one is not.
fn match_bytecodes_to_hashes(
    requested_hashes: &[B256],
    codes: &[Bytes],
) -> Result<Vec<(B256, Bytes)>, SnapSyncError> {
    let requested: HashMap<_, _> =
        requested_hashes.iter().copied().enumerate().map(|(i, hash)| (hash, i)).collect();
    let mut last_position = None;
    let mut matched = Vec::with_capacity(codes.len());

    for code in codes {
        let hash = keccak256(code.as_ref());
        let Some(position) = requested.get(&hash).copied() else {
            return Err(SnapSyncError::Network(format!(
                "snap bytecode response contained unrequested code hash {hash}"
            )))
        };
        if last_position.is_some_and(|last| position <= last) {
            return Err(SnapSyncError::Network(
                "snap bytecode response was not in request order".into(),
            ))
        }
        last_position = Some(position);
        matched.push((hash, code.clone()));
    }

    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_trie::test_utils::storage_root_prehashed;

    fn b256(value: u64) -> B256 {
        B256::left_padding_from(&value.to_be_bytes())
    }

    fn slot(hash: B256, value: u64) -> StorageData {
        StorageData { hash, data: alloy_rlp::encode(U256::from(value)).into() }
    }

    /// Mirrors the server's slim account encoding.
    #[derive(alloy_rlp::RlpEncodable)]
    struct SlimBody<'a> {
        nonce: u64,
        balance: U256,
        storage_root: &'a [u8],
        code_hash: &'a [u8],
    }

    fn slim_body(nonce: u64, storage_root: &[u8], code_hash: &[u8]) -> Bytes {
        alloy_rlp::encode(SlimBody { nonce, balance: U256::from(1), storage_root, code_hash })
            .into()
    }

    #[test]
    fn slim_account_expands_empty_fields_to_trie_defaults() {
        let account = decode_slim_account(&slim_body(7, &[], &[])).unwrap();

        assert_eq!(account.nonce, 7);
        assert_eq!(account.storage_root, EMPTY_ROOT_HASH);
        assert_eq!(account.code_hash, KECCAK256_EMPTY);
    }

    #[test]
    fn slim_account_keeps_present_fields() {
        let storage_root = b256(0xaa);
        let code_hash = b256(0xbb);
        let account =
            decode_slim_account(&slim_body(1, storage_root.as_slice(), code_hash.as_slice()))
                .unwrap();

        assert_eq!(account.storage_root, storage_root);
        assert_eq!(account.code_hash, code_hash);
    }

    #[test]
    fn account_range_rejects_out_of_order_accounts() {
        let accounts = vec![
            AccountData { hash: b256(2), body: slim_body(0, &[], &[]) },
            AccountData { hash: b256(1), body: slim_body(0, &[], &[]) },
        ];

        assert!(decode_account_range(&accounts, B256::ZERO).is_err());
    }

    #[test]
    fn account_range_rejects_accounts_before_origin() {
        let accounts = vec![AccountData { hash: b256(1), body: slim_body(0, &[], &[]) }];

        assert!(decode_account_range(&accounts, b256(2)).is_err());
    }

    #[test]
    fn full_storage_range_must_rebuild_the_storage_root() {
        let account = b256(1);
        let slots = vec![slot(b256(2), 2), slot(b256(3), 3)];
        let storage_roots = HashMap::from([(
            account,
            storage_root_prehashed([(b256(2), U256::from(2)), (b256(3), U256::from(3))]),
        )]);

        assert!(verify_full_storage_range(account, &storage_roots, &slots).is_ok());
        // Dropping a slot must not still verify, otherwise a peer could withhold storage.
        assert!(verify_full_storage_range(account, &storage_roots, &slots[..1]).is_err());
    }

    #[test]
    fn empty_storage_verifies_against_the_empty_root() {
        let account = b256(1);
        let storage_roots = HashMap::from([(account, EMPTY_ROOT_HASH)]);

        assert!(verify_full_storage_range(account, &storage_roots, &[]).is_ok());
    }

    #[test]
    fn storage_slots_must_be_ordered_from_the_origin() {
        let account = b256(1);
        let first = slot(b256(2), 2);
        let second = slot(b256(3), 3);

        assert!(validate_storage_slots(account, b256(2), &[first.clone(), second.clone()]).is_ok());
        assert!(validate_storage_slots(account, b256(2), &[second.clone(), first]).is_err());
        assert!(validate_storage_slots(account, b256(4), &[second]).is_err());
    }

    #[test]
    fn bytecode_matching_accepts_a_short_prefix() {
        let first = Bytes::from_static(&[1, 2, 3]);
        let second = Bytes::from_static(&[4, 5, 6]);
        let requested = vec![keccak256(first.as_ref()), keccak256(second.as_ref())];

        let matched = match_bytecodes_to_hashes(&requested, std::slice::from_ref(&first)).unwrap();

        assert_eq!(matched, vec![(keccak256(first.as_ref()), first)]);
    }

    #[test]
    fn bytecode_matching_rejects_unrequested_code() {
        let requested = vec![keccak256([1, 2, 3])];

        assert!(match_bytecodes_to_hashes(&requested, &[Bytes::from_static(&[4, 5, 6])]).is_err());
    }

    #[test]
    fn bytecode_matching_rejects_out_of_order_and_duplicate_codes() {
        let first = Bytes::from_static(&[1, 2, 3]);
        let second = Bytes::from_static(&[4, 5, 6]);
        let requested = vec![keccak256(first.as_ref()), keccak256(second.as_ref())];

        assert!(match_bytecodes_to_hashes(&requested, &[second, first.clone()]).is_err());
        assert!(match_bytecodes_to_hashes(&requested, &[first.clone(), first]).is_err());
    }
}
