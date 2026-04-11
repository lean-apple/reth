//! Root
//! Includes common helpers for integration tests
//!
//! These tests use the `reth-era-downloader` client to download `.erae` files temporarily
//! and verify that we can correctly read and decompress their data.
//!
//! Files are downloaded from [`MAINNET_URL`] and [`SEPOLIA_URL`].

use reqwest::{Client, Url};
use reth_era::{
    common::file_ops::{EraFileType, FileReader},
    e2s::error::E2sError,
    era::file::{EraFile, EraReader},
    erae::file::{EraEFile, EraEReader},
};
use reth_era_downloader::EraClient;
use std::{
    collections::HashMap,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
};

use eyre::{eyre, Result};
use tempfile::TempDir;

mod era;
mod erae;

const fn main() {}

/// Mainnet network name
const MAINNET: &str = "mainnet";
/// Default mainnet url
/// for downloading mainnet `.erae` files
const ERAE_MAINNET_URL: &str = "https://data.ethpandaops.io/erae/mainnet/";

/// Succinct list of mainnet files we want to download
/// from <https://data.ethpandaops.io/erae/mainnet/>
/// for testing purposes
const ERAE_MAINNET_FILES_NAMES: [&str; 8] = [
    "mainnet-00000-a6860fef.erae",
    "mainnet-00003-14c8639d.erae",
    "mainnet-00151-126e861b.erae",
    "mainnet-00293-6c3fd372.erae",
    "mainnet-00443-c791ac46.erae",
    "mainnet-01367-fa634aec.erae",
    "mainnet-01610-2a0f5993.erae",
    "mainnet-01895-4373f22f.erae",
];

/// Sepolia network name
const SEPOLIA: &str = "sepolia";

/// Default sepolia url
/// for downloading sepolia `.erae` files
const ERAE_SEPOLIA_URL: &str = "https://data.ethpandaops.io/erae/sepolia/";

/// Succinct list of sepolia files we want to download
/// from <https://data.ethpandaops.io/erae/sepolia/>
/// for testing purposes
const ERAE_SEPOLIA_FILES_NAMES: [&str; 4] = [
    "sepolia-00000-8e3e7dc9.erae",
    "sepolia-00074-3d475575.erae",
    "sepolia-00173-b0373953.erae",
    "sepolia-00182-959852e5.erae",
];

const HOODI: &str = "hoodi";

/// Default hoodi url
/// for downloading hoodi `.era` files
/// TODO: to replace with internal era files hosting url
const ERA_HOODI_URL: &str = "https://hoodi.era.nimbus.team/";

/// Succinct list of hoodi files we want to download
/// from <https://hoodi.era.nimbus.team/> //TODO: to replace with internal era files hosting url
/// for testing purposes
const ERA_HOODI_FILES_NAMES: [&str; 4] = [
    "hoodi-00000-212f13fc.era",
    "hoodi-00021-857e418b.era",
    "hoodi-00175-202aaa6d.era",
    "hoodi-00201-0d521fc8.era",
];

/// Default mainnet url
/// for downloading mainnet `.era` files
//TODO: to replace with internal era files hosting url
const ERA_MAINNET_URL: &str = "https://mainnet.era.nimbus.team/";

/// Succinct list of mainnet files we want to download
/// from <https://mainnet.era.nimbus.team/> //TODO: to replace with internal era files hosting url
/// for testing purposes
const ERA_MAINNET_FILES_NAMES: [&str; 8] = [
    "mainnet-00000-4b363db9.era",
    "mainnet-00178-0d0a5290.era",
    "mainnet-00518-4e267a3a.era",
    "mainnet-00780-bb546fec.era",
    "mainnet-01070-7616e3e2.era",
    "mainnet-01267-e3ddc749.era",
    "mainnet-01581-82073d28.era",
    "mainnet-01592-d4dc8b98.era",
];

/// Utility for downloading `.era` and `.erae` files for tests
/// in a temporary directory and caching them in memory
#[derive(Debug)]
struct EraTestDownloader {
    /// Temporary directory for storing downloaded files
    temp_dir: TempDir,
    /// Cache mapping file names to their paths
    file_cache: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl EraTestDownloader {
    /// Create a new downloader instance with a temporary directory
    async fn new() -> Result<Self> {
        let temp_dir =
            TempDir::new().map_err(|e| eyre!("Failed to create temp directory: {}", e))?;

        Ok(Self { temp_dir, file_cache: Arc::new(Mutex::new(HashMap::new())) })
    }

    /// Download a specific .erae file by name
    pub(crate) async fn download_file(&self, filename: &str, network: &str) -> Result<PathBuf> {
        // check cache first
        {
            let cache = self.file_cache.lock().unwrap();
            if let Some(path) = cache.get(filename) {
                return Ok(path.clone());
            }
        }

        // check if the filename is supported
        self.validate_filename(filename, network)?;

        let (url, _): (&str, &[&str]) = self.get_network_config(filename, network)?;
        let final_url = Url::from_str(url).map_err(|e| eyre!("Failed to parse URL: {}", e))?;

        let folder = self.temp_dir.path();

        // set up the client
        let client = EraClient::new(Client::new(), final_url, folder);

        // set up the file list, required before we can download files
        client.fetch_file_list().await.map_err(|e| {
            E2sError::Io(std::io::Error::other(format!("Failed to fetch file list: {e}")))
        })?;

        // create an url for the file
        let file_url = Url::parse(&format!("{url}{filename}"))
            .map_err(|e| eyre!("Failed to parse file URL: {}", e))?;

        // download the file
        let mut client = client;
        let downloaded_path = client
            .download_to_file(file_url)
            .await
            .map_err(|e| eyre!("Failed to download file: {}", e))?;

        // update the cache
        {
            let mut cache = self.file_cache.lock().unwrap();
            cache.insert(filename.to_string(), downloaded_path.to_path_buf());
        }

        Ok(downloaded_path.to_path_buf())
    }

    /// Validate that filename is in the supported list for the network
    fn validate_filename(&self, filename: &str, network: &str) -> Result<()> {
        let (_, supported_files) = self.get_network_config(filename, network)?;

        if !supported_files.contains(&filename) {
            return Err(eyre!(
                "Unknown file: '{}' for network '{}'. Supported files: {:?}",
                filename,
                network,
                supported_files
            ));
        }

        Ok(())
    }

    /// Get network configuration, URL and supported files, based on network and file type
    fn get_network_config(
        &self,
        filename: &str,
        network: &str,
    ) -> Result<(&'static str, &'static [&'static str])> {
        let file_type = EraFileType::from_filename(filename)
            .ok_or_else(|| eyre!("Unknown file extension for: {}", filename))?;

        match (network, file_type) {
            (MAINNET, EraFileType::EraE) => Ok((ERAE_MAINNET_URL, &ERAE_MAINNET_FILES_NAMES[..])),
            (MAINNET, EraFileType::Era) => Ok((ERA_MAINNET_URL, &ERA_MAINNET_FILES_NAMES[..])),
            (SEPOLIA, EraFileType::EraE) => Ok((ERAE_SEPOLIA_URL, &ERAE_SEPOLIA_FILES_NAMES[..])),
            (HOODI, EraFileType::Era) => Ok((ERA_HOODI_URL, &ERA_HOODI_FILES_NAMES[..])),
            _ => Err(eyre!(
                "Unsupported combination: network '{}' with file type '{:?}'",
                network,
                file_type
            )),
        }
    }

    /// Open `.erae` file, downloading it if necessary
    async fn open_erae_file(&self, filename: &str, network: &str) -> Result<EraEFile> {
        let path = self.download_file(filename, network).await?;
        EraEReader::open(&path, network).map_err(|e| eyre!("Failed to open EraE file: {e}"))
    }

    /// Open `.era` file, downloading it if necessary
    async fn open_era_file(&self, filename: &str, network: &str) -> Result<EraFile> {
        let path = self.download_file(filename, network).await?;
        EraReader::open(&path, network).map_err(|e| eyre!("Failed to open EraE file: {e}"))
    }
}
