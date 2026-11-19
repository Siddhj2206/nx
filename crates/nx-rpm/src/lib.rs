//! RPM repositories for the host tier: which repos the image defines, and
//! the metadata files that describe them.

pub mod fetch;
pub mod packages;
pub mod repo;
pub mod repomd;
pub mod universe;

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

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::{Error, Http, Result};

    /// Serves canned bodies and records requests.
    #[derive(Default)]
    pub struct FakeHttp {
        pub bodies: HashMap<String, Vec<u8>>,
        hits: Mutex<Vec<String>>,
    }

    impl FakeHttp {
        pub fn hits(&self) -> Vec<String> {
            self.hits.lock().map(|h| h.clone()).unwrap_or_default()
        }

        pub fn clear(&self) {
            if let Ok(mut h) = self.hits.lock() {
                h.clear();
            }
        }
    }

    impl Http for FakeHttp {
        fn get(&self, url: &str) -> Result<Vec<u8>> {
            if let Ok(mut h) = self.hits.lock() {
                h.push(url.to_owned());
            }
            self.bodies.get(url).cloned().ok_or_else(|| Error::Fetch {
                url: url.to_owned(),
                reason: "404".into(),
            })
        }
    }

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
