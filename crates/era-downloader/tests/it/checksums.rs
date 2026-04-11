use bytes::Bytes;
use futures::Stream;
use futures_util::StreamExt;
use reqwest::{IntoUrl, Url};
use reth_era_downloader::{EraClient, EraStream, EraStreamConfig, HttpClient};
use std::str::FromStr;
use tempfile::tempdir;
use test_case::test_case;

#[test_case("https://data.ethpandaops.io/erae/mainnet/"; "ethpandaops")]
#[tokio::test]
async fn test_invalid_checksum_returns_error(url: &str) {
    let base_url = Url::from_str(url).unwrap();
    let folder = tempdir().unwrap();
    let folder = folder.path();
    let client = EraClient::new(FailingClient, base_url, folder);

    let mut stream = EraStream::new(
        client,
        EraStreamConfig::default().with_max_files(2).with_max_concurrent_downloads(1),
    );

    let actual_err = stream.next().await.unwrap().unwrap_err().to_string();
    let expected_err = format!(
        "Checksum mismatch, \
got: 87428fc522803d31065e7bce3cf03fe475096631e5e07bbd7a0fde60c4cf25c7, \
expected: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
for mainnet-00000-a6860fef.erae at {}/mainnet-00000-a6860fef.erae",
        folder.display()
    );

    assert_eq!(actual_err, expected_err);

    let actual_err = stream.next().await.unwrap().unwrap_err().to_string();
    let expected_err = format!(
        "Checksum mismatch, \
got: 0263829989b6fd954f72baaf2fc64bc2e2f01d692d4de72986ea808f6e99813f, \
expected: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb \
for mainnet-00001-05c64fc4.erae at {}/mainnet-00001-05c64fc4.erae",
        folder.display()
    );

    assert_eq!(actual_err, expected_err);
}

const CHECKSUMS: &[u8] = b"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// An HTTP client pre-programmed with canned answers to received calls.
/// Panics if it receives an unknown call.
#[derive(Debug, Clone)]
struct FailingClient;

impl HttpClient for FailingClient {
    async fn get<U: IntoUrl + Send + Sync>(
        &self,
        url: U,
    ) -> eyre::Result<impl Stream<Item = eyre::Result<Bytes>> + Send + Sync + Unpin> {
        let url = url.into_url().unwrap();

        Ok(futures::stream::iter(vec![Ok(match url.as_str() {
            "https://data.ethpandaops.io/erae/mainnet/" |
            "https://data.ethpandaops.io/erae/mainnet/index.html" => {
                Bytes::from_static(crate::ERAE_ETHPANDAOPS)
            }
            "https://data.ethpandaops.io/erae/mainnet/checksums.txt" => {
                Bytes::from_static(CHECKSUMS)
            }
            "https://data.ethpandaops.io/erae/mainnet/mainnet-00000-a6860fef.erae" => {
                Bytes::from_static(crate::ERAE_MAINNET_0)
            }
            "https://data.ethpandaops.io/erae/mainnet/mainnet-00001-05c64fc4.erae" => {
                Bytes::from_static(crate::ERAE_MAINNET_1)
            }
            v => unimplemented!("Unexpected URL \"{v}\""),
        })]))
    }
}
