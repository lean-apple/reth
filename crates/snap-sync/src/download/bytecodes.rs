//! Bytecode requests.

use super::{StateDownloader, BYTECODE_BATCH_SIZE, MAX_REQUEST_ATTEMPTS};
use crate::{error::SnapSyncError, SNAP_RESPONSE_BYTES_LIMIT};
use alloy_primitives::{
    keccak256,
    map::{B256Map, B256Set},
    Bytes, B256,
};
use reth_db_api::transaction::DbTxMut;
use reth_eth_wire_types::snap::GetByteCodesMessage;
use reth_network_p2p::snap::client::{SnapClient, SnapResponse};
use reth_provider::DatabaseProviderFactory;
use reth_storage_api::{DBProvider, StateWriter};

impl<C, F> StateDownloader<'_, C, F>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    /// Fetches and writes bytecodes for a set of code hashes.
    pub(super) async fn download_bytecodes(
        &mut self,
        code_hashes: &B256Set,
    ) -> Result<(), SnapSyncError> {
        let hashes: Vec<B256> = code_hashes.iter().copied().collect();

        for chunk in hashes.chunks(BYTECODE_BATCH_SIZE) {
            let codes = self.fetch_bytecodes(chunk).await?;
            if !codes.is_empty() {
                self.writer.write_bytecodes(&codes)?;
            }
        }

        Ok(())
    }

    /// Requests bytecodes, retrying with another peer on an untrustworthy response.
    async fn fetch_bytecodes(
        &mut self,
        hashes: &[B256],
    ) -> Result<Vec<(B256, Bytes)>, SnapSyncError> {
        let mut last_error = None;

        for _ in 0..MAX_REQUEST_ATTEMPTS {
            let request_id = self.next_request_id();
            let response = match self
                .client
                .get_byte_codes(GetByteCodesMessage {
                    request_id,
                    hashes: hashes.to_vec(),
                    response_bytes: SNAP_RESPONSE_BYTES_LIMIT,
                })
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(SnapSyncError::Network(format!(
                        "snap bytecode request failed: {err}"
                    )));
                    continue
                }
            };

            let (peer, data) = response.split();
            let SnapResponse::ByteCodes(msg) = data else {
                last_error = Some(self.penalize(
                    peer,
                    SnapSyncError::Network("expected a byte codes response".into()),
                ));
                continue
            };

            match Self::match_bytecodes(hashes, &msg.codes) {
                Ok(codes) => return Ok(codes),
                Err(err) => last_error = Some(self.penalize(peer, err)),
            }
        }

        Err(last_error.expect("at least one attempt was made"))
    }
}

// Checks that need neither a client nor a database.
impl<C, F> StateDownloader<'_, C, F> {
    /// Pairs returned bytecodes with the hashes that were requested.
    ///
    /// Servers may drop entries they don't have but must keep request order, so a short reply is a
    /// valid prefix while a reordered or duplicated one is not.
    fn match_bytecodes(
        requested_hashes: &[B256],
        codes: &[Bytes],
    ) -> Result<Vec<(B256, Bytes)>, SnapSyncError> {
        let requested: B256Map<usize> =
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
}

#[cfg(test)]
mod tests {
    use super::*;

    type Downloader<'a> = StateDownloader<'a, (), ()>;

    #[test]
    fn bytecode_matching_accepts_a_short_prefix() {
        let first = Bytes::from_static(&[1, 2, 3]);
        let second = Bytes::from_static(&[4, 5, 6]);
        let requested = [keccak256(first.as_ref()), keccak256(second.as_ref())];

        let matched =
            Downloader::match_bytecodes(&requested, std::slice::from_ref(&first)).unwrap();

        assert_eq!(matched, vec![(keccak256(first.as_ref()), first)]);
    }

    #[test]
    fn bytecode_matching_rejects_unrequested_code() {
        let requested = [keccak256([1, 2, 3])];

        assert!(Downloader::match_bytecodes(&requested, &[Bytes::from_static(&[4, 5, 6])]).is_err());
    }

    #[test]
    fn bytecode_matching_rejects_out_of_order_and_duplicate_codes() {
        let first = Bytes::from_static(&[1, 2, 3]);
        let second = Bytes::from_static(&[4, 5, 6]);
        let requested = [keccak256(first.as_ref()), keccak256(second.as_ref())];

        assert!(Downloader::match_bytecodes(&requested, &[second, first.clone()]).is_err());
        assert!(Downloader::match_bytecodes(&requested, &[first.clone(), first]).is_err());
    }
}
