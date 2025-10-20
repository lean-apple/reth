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

pub(crate) const ERA1_NIMBUS: &[u8] = include_bytes!("../res/era1-nimbus.html");
pub(crate) const ERA1_ETH_PORTAL: &[u8] = include_bytes!("../res/era1-ethportal.html");
pub(crate) const ERA1_ITHACA: &[u8] = include_bytes!("../res/era1-ithaca.html");
pub(crate) const ERA1_CHECKSUMS: &[u8] = include_bytes!("../res/era1-checksums.txt");

pub(crate) const ERA1_MAINNET_0: &[u8] = include_bytes!("../res/mainnet-00000-5ec1ffb8.era1");
pub(crate) const ERA1_MAINNET_1: &[u8] = include_bytes!("../res/mainnet-00001-a5364e9a.era1");

pub(crate) const ERA_NIMBUS: &[u8] = include_bytes!("../res/era-nimbus.html");


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

        Ok(futures::stream::iter(vec![Ok(match url.to_string().as_str() {
            // Era1 urls match
            "https://mainnet.era1.nimbus.team/" => Bytes::from_static(ERA1_NIMBUS),
            "https://era1.ethportal.net/" => Bytes::from_static(ERA1_ETH_PORTAL),
            "https://era.ithaca.xyz/era1/index.html" => Bytes::from_static(ERA1_ITHACA),
            "https://mainnet.era1.nimbus.team/checksums.txt" |
            "https://era1.ethportal.net/checksums.txt" |
            "https://era.ithaca.xyz/era1/checksums.txt" => Bytes::from_static(ERA1_CHECKSUMS),
            "https://era1.ethportal.net/mainnet-00000-5ec1ffb8.era1" |
            "https://mainnet.era1.nimbus.team/mainnet-00000-5ec1ffb8.era1" |
            "https://era.ithaca.xyz/era1/mainnet-00000-5ec1ffb8.era1" => {
                Bytes::from_static(ERA1_MAINNET_0)
            }
            "https://era1.ethportal.net/mainnet-00001-a5364e9a.era1" |
            "https://mainnet.era1.nimbus.team/mainnet-00001-a5364e9a.era1" |
            "https://era.ithaca.xyz/era1/mainnet-00001-a5364e9a.era1" => {
                Bytes::from_static(ERA1_MAINNET_1)
            }
            // Era urls match
            "https://mainnet.era.nimbus.team/" => Bytes::from_static(ERA_NIMBUS),
            v => unimplemented!("Unexpected URL \"{v}\""),
        })]))
    }
}
