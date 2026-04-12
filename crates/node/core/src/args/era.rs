use clap::Args;
use reth_chainspec::{ChainKind, NamedChain};
use std::path::Path;
use url::Url;

/// Erae url for mainnet.
const MAINNET_ERAE_URL: &str = "https://data.ethpandaops.io/erae/mainnet/index.html";

/// Erae url for sepolia.
const SEPOLIA_ERAE_URL: &str = "https://data.ethpandaops.io/erae/sepolia/index.html";

/// Syncs `EraE` encoded blocks from a local or remote source.
#[derive(Clone, Debug, Default, Args)]
pub struct EraArgs {
    /// Enable import from `EraE` files.
    #[arg(
        id = "era.enable",
        long = "era.enable",
        value_name = "ERA_ENABLE",
        default_value_t = false
    )]
    pub enabled: bool,

    /// Describes where to get the ERA files to import from.
    #[clap(flatten)]
    pub source: EraSourceArgs,
}

/// Arguments for the block history import based on `EraE` encoded files.
#[derive(Clone, Debug, Default, Args)]
#[group(required = false, multiple = false)]
pub struct EraSourceArgs {
    /// The path to a directory for import.
    ///
    /// The `EraE` files are read from the local directory parsing headers and bodies.
    #[arg(long = "era.path", value_name = "ERA_PATH", verbatim_doc_comment)]
    pub path: Option<Box<Path>>,

    /// The URL to a remote host where the `EraE` files are hosted.
    ///
    /// The `EraE` files are read from the remote host using HTTP GET requests parsing headers
    /// and bodies.
    #[arg(long = "era.url", value_name = "ERA_URL", verbatim_doc_comment)]
    pub url: Option<Url>,
}

/// The `ExtractEraHost` trait allows to derive a default URL host for ERA files.
pub trait DefaultEraHost {
    /// Converts `self` into [`Url`] index page of the ERA host.
    ///
    /// Returns `Err` if the conversion is not possible.
    fn default_era_host(&self) -> Option<Url>;
}

impl DefaultEraHost for ChainKind {
    fn default_era_host(&self) -> Option<Url> {
        Some(match self {
            Self::Named(NamedChain::Mainnet) => {
                Url::parse(MAINNET_ERAE_URL).expect("URL should be valid")
            }
            Self::Named(NamedChain::Sepolia) => {
                Url::parse(SEPOLIA_ERAE_URL).expect("URL should be valid")
            }
            _ => return None,
        })
    }
}
