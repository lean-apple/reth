//! Root module for test modules, so that the tests are built into a single binary.

mod checksums;
mod download;
mod fs;
mod list;
mod stream;

const fn main() {}

use bytes::Bytes;
use futures::Stream;
use reqwest::IntoUrl;
use reth_era_downloader::HttpClient;

pub(crate) const ERAE_ETHPANDAOPS: &[u8] = include_bytes!("../res/erae-files/erae-ethpandaops.html");
pub(crate) const ERAE_CHECKSUMS: &[u8] = include_bytes!("../res/erae-files/erae-checksums.txt");
pub(crate) const ERAE_MAINNET_0: &[u8] =
    include_bytes!("../res/erae-files/mainnet-00000-a6860fef.erae");
pub(crate) const ERAE_MAINNET_1: &[u8] =
    include_bytes!("../res/erae-files/mainnet-00001-05c64fc4.erae");

pub(crate) const ERA_NIMBUS: &[u8] = include_bytes!("../res/era-files/era-nimbus.html");
pub(crate) const ERA_MAINNET_0: &[u8] =
    include_bytes!("../res/era-files/mainnet-00000-4b363db9.era");
pub(crate) const ERA_MAINNET_1: &[u8] =
    include_bytes!("../res/era-files/mainnet-00001-40cf2f3c.era");

/// An HTTP client pre-programmed with canned answers to received calls.
/// Panics if it receives an unknown call.
#[derive(Debug, Clone)]
struct StubClient;

impl HttpClient for StubClient {
    async fn get<U: IntoUrl + Send + Sync>(
        &self,
        url: U,
    ) -> eyre::Result<impl Stream<Item = eyre::Result<Bytes>> + Send + Sync + Unpin> {
        let url = url.into_url().unwrap();

        Ok(futures::stream::iter(vec![Ok(match url.as_str() {
            // EraE urls
            "https://data.ethpandaops.io/erae/mainnet/" |
            "https://data.ethpandaops.io/erae/mainnet/index.html" => {
                Bytes::from_static(ERAE_ETHPANDAOPS)
            }
            "https://data.ethpandaops.io/erae/mainnet/checksums.txt" => {
                Bytes::from_static(ERAE_CHECKSUMS)
            }
            "https://data.ethpandaops.io/erae/mainnet/mainnet-00000-a6860fef.erae" => {
                Bytes::from_static(ERAE_MAINNET_0)
            }
            "https://data.ethpandaops.io/erae/mainnet/mainnet-00001-05c64fc4.erae" => {
                Bytes::from_static(ERAE_MAINNET_1)
            }
            // Era urls
            "https://mainnet.era.nimbus.team/" => Bytes::from_static(ERA_NIMBUS),
            "https://mainnet.era.nimbus.team/mainnet-00000-4b363db9.era" => {
                Bytes::from_static(ERA_MAINNET_0)
            }
            "https://mainnet.era.nimbus.team/mainnet-00001-40cf2f3c.era" => {
                Bytes::from_static(ERA_MAINNET_1)
            }

            v => unimplemented!("Unexpected URL \"{v}\""),
        })]))
    }
}
