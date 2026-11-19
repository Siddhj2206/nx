//! RPM repositories for the host tier: which repos the image defines, and
//! the metadata files that describe them.

pub mod fetch;
pub mod repo;
pub mod repomd;
pub mod universe;

pub use fetch::{Http, Refresh, Synced, Ureq, sync};
pub use nx_solv::Package;
pub use repo::{Repo, Vars};
pub use repomd::{Checksum, DataFile, Metalink, Repomd};
pub use universe::{Options, Plan, Universe};

/// Errors from reading repo definitions and metadata.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("downloading {url}: {reason}")]
    Fetch { url: String, reason: String },
    #[error(transparent)]
    Solv(#[from] nx_solv::Error),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
