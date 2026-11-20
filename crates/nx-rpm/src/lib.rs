//! RPM repositories for the host tier: which repos the image defines, and
//! the metadata files that describe them.

pub mod fetch;
pub mod packages;
pub mod plugs;
pub mod repo;
pub mod repomd;
pub mod universe;

/// Where a package's commands go: the directories on `PATH`, relative to
/// a root. Fedora 42 and later link `/usr/sbin` to `bin` and `/bin` and
/// `/sbin` into `/usr`, but a package's file list still names them.
pub const COMMAND_DIRS: [&str; 4] = ["usr/bin", "usr/sbin", "bin", "sbin"];

pub use fetch::{Freshness, Http, Refresh, Synced, Ureq, sync};
pub use nx_solv::Package;
pub use packages::{Relative, RepoKeys, download, download_all, keyring, koji};
pub use repo::{Repo, Vars};
pub use repomd::{Checksum, DataFile, Metalink, Repomd};
pub use universe::{Options, Plan, Planned, Universe};

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
    #[error(transparent)]
    Rpm(#[from] nx_librpm::Error),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<nx_http::FetchError> for Error {
    fn from(e: nx_http::FetchError) -> Self {
        Self::Fetch {
            url: e.url,
            reason: e.reason,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {

    /// Serves canned bodies and records requests.
    pub type FakeHttp = nx_http::Canned;

    /// A fresh directory under the system temp dir, removed on drop.
    pub struct TempDir(pub std::path::PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nx-rpm-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A file under the workspace's `testdata/rpm`.
    pub fn testdata(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/rpm")
            .join(name)
    }
}
