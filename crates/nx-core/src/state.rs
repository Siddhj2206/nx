//! nx's state directory (`/var/lib/nx`): the package store, generations,
//! `current`, and the lock that serializes changes.
//!
//! ```text
//! store/rpm/<sha256>/      one unpacked package file, named by its SHA-256
//! generations/<N>/         sysext/, manifest.toml, lock.toml
//! current -> generations/<N>
//! lock                     held by any command that changes state
//! ```
//!
//! Everything is written beside its final name and renamed into place, so a
//! crash leaves either the old state or the new one.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The default state directory.
pub const DEFAULT_ROOT: &str = "/var/lib/nx";

/// A state directory.
#[derive(Debug, Clone)]
pub struct State {
    root: PathBuf,
}

/// Held while a command changes state; a second one waits.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

impl State {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Takes the state lock, creating the directory layout on first use.
    /// Read-only commands don't take it.
    pub fn lock(&self) -> Result<Lock> {
        for dir in [self.store_dir(), self.generations_dir()] {
            create_dir(&dir)?;
        }
        let path = self.root.join("lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| Error::Write {
                path: path.clone(),
                source,
            })?;
        file.lock()
            .map_err(|source| Error::Write { path, source })?;
        Ok(Lock { _file: file })
    }

    pub fn store(&self) -> Store {
        Store {
            dir: self.store_dir(),
        }
    }

    fn store_dir(&self) -> PathBuf {
        self.root.join("store/rpm")
    }

    pub(crate) fn generations_dir(&self) -> PathBuf {
        self.root.join("generations")
    }

    pub(crate) fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }

    /// Generation numbers on disk, oldest first.
    pub fn generations(&self) -> Result<Vec<u32>> {
        let dir = self.generations_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(Error::Read { path: dir, source }),
        };
        let mut numbers: Vec<u32> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        numbers.sort_unstable();
        Ok(numbers)
    }

    /// The generation `current` points at, if any.
    pub fn current_number(&self) -> Result<Option<u32>> {
        let link = self.current_link();
        match std::fs::read_link(&link) {
            Ok(target) => target
                .file_name()
                .and_then(|n| n.to_str()?.parse().ok())
                .map(Some)
                .ok_or_else(|| {
                    Error::Invalid(format!("{} points at {}", link.display(), target.display()))
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Read { path: link, source }),
        }
    }
}

/// Unpacked package files, one directory each, named by the file's SHA-256.
/// A directory is complete once it has its final name.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn path(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256)
    }

    pub fn contains(&self, sha256: &str) -> bool {
        self.path(sha256).is_dir()
    }

    /// Adds a package: `fill` writes its tree into an empty directory,
    /// which is then renamed into place. Needs the state lock.
    pub fn insert<E>(
        &self,
        _lock: &Lock,
        sha256: &str,
        fill: impl FnOnce(&Path) -> Result<(), E>,
    ) -> Result<PathBuf, E>
    where
        E: From<Error>,
    {
        let valid = sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(Error::Invalid(format!("not a SHA-256: {sha256:?}")).into());
        }
        let dest = self.path(sha256);
        if dest.is_dir() {
            return Ok(dest);
        }
        let tmp = self.dir.join(format!(".{sha256}.part"));
        remove_tree(&tmp)?;
        create_dir(&tmp)?;
        fill(&tmp)?;
        std::fs::rename(&tmp, &dest).map_err(|source| Error::Write {
            path: dest.clone(),
            source,
        })?;
        Ok(dest)
    }
}

pub(crate) fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| Error::Write {
        path: dir.to_owned(),
        source,
    })
}

/// Removes a tree nx left half-built, read-only directories included.
pub(crate) fn remove_tree(dir: &Path) -> Result<()> {
    if std::fs::symlink_metadata(dir).is_err() {
        return Ok(());
    }
    make_writable(dir);
    std::fs::remove_dir_all(dir).map_err(|source| Error::Write {
        path: dir.to_owned(),
        source,
    })
}

fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode() | 0o700;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode));
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            make_writable(&e.path());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nx-core-{tag}-{}", std::process::id()));
            let _ = remove_tree(&dir);
            let _ = std::fs::create_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = remove_tree(&self.0);
        }
    }

    pub const SHA: &str = "834489e0eb7f4cb5a0addf9557f36ff592d295b130ba9121ec2868413abb9584";

    #[test]
    fn store_inserts_once() -> Result<()> {
        let tmp = TempDir::new("store");
        let state = State::new(&tmp.0);
        let lock = state.lock()?;
        let store = state.store();
        assert!(!store.contains(SHA));
        let path = store.insert(&lock, SHA, |dir| {
            std::fs::write(dir.join("f"), "x").map_err(|source| Error::Write {
                path: dir.to_owned(),
                source,
            })
        })?;
        assert!(store.contains(SHA) && path.join("f").is_file());
        // Present: fill is not called again.
        store.insert(&lock, SHA, |_| Err(Error::Invalid("called twice".into())))?;
        assert!(
            store
                .insert(&lock, "../x", |_| Ok::<(), Error>(()))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn a_failed_fill_leaves_nothing() -> Result<()> {
        let tmp = TempDir::new("fail");
        let state = State::new(&tmp.0);
        let lock = state.lock()?;
        let store = state.store();
        let r = store.insert(&lock, SHA, |_| Err(Error::Invalid("bad package".into())));
        assert!(r.is_err() && !store.contains(SHA));
        Ok(())
    }
}
