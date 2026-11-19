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
//! Everything is written beside its final name, flushed to disk, and only
//! then renamed into place, so a crash leaves either the old state or the
//! new one, never a name over half-written data.

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

/// How the host tier reaches the running system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// One directory extension merged over `/usr` by `systemd-sysext`.
    Sysext,
    /// The generation's `usr` as a prefix, reached through `nx shell`.
    Prefix,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sysext => "sysext",
            Self::Prefix => "prefix",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sysext" => Some(Self::Sysext),
            "prefix" => Some(Self::Prefix),
            _ => None,
        }
    }
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
        // The first user to change anything creates the file; another
        // administrator may not be able to open it for writing, and flock
        // doesn't need that. Someone who can't write the directory either
        // isn't one, and gets the error.
        let file = match File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    && rustix::fs::access(&self.root, rustix::fs::Access::WRITE_OK).is_ok() =>
            {
                File::open(&path).map_err(|source| Error::Write {
                    path: path.clone(),
                    source,
                })?
            }
            Err(source) => {
                return Err(Error::Write {
                    path: path.clone(),
                    source,
                });
            }
        };
        file.lock()
            .map_err(|source| Error::Write { path, source })?;
        Ok(Lock { _file: file })
    }

    /// The cache every administrator shares (repo metadata, `.solv` files,
    /// downloads), `<state>/cache`, when this user may write it or can make
    /// it. Anyone else, and any machine without a writable state
    /// directory, has no shared cache and keeps their own.
    ///
    /// Anything in it is checked again before it is trusted: metadata
    /// against the checksums its repo's `repomd.xml` names, packages against
    /// their signatures. An administrator who poisons it can fail another's
    /// command, nothing more.
    pub fn shared_cache(&self) -> Option<PathBuf> {
        use rustix::fs::{Access, access};
        let dir = self.root.join("cache");
        if dir.is_dir() {
            return access(&dir, Access::WRITE_OK | Access::EXEC_OK)
                .is_ok()
                .then_some(dir);
        }
        // Created under the state directory's default ACL, so the other
        // administrators can write it too.
        (access(&self.root, Access::WRITE_OK).is_ok() && std::fs::create_dir(&dir).is_ok())
            .then_some(dir)
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

    /// Points `current` at generation `number` with one rename. The caller
    /// serializes: a command holds the state [`Lock`]; nxd handles one
    /// request at a time.
    pub fn set_current(&self, number: u32) -> Result<()> {
        if !self.generations_dir().join(number.to_string()).is_dir() {
            return Err(Error::Invalid(format!("there is no generation {number}")));
        }
        let link = self.current_link();
        let tmp = link.with_file_name(".current.part");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(format!("generations/{number}"), &tmp).map_err(|source| {
            Error::Write {
                path: tmp.clone(),
                source,
            }
        })?;
        rename(&tmp, &link)?;
        sync_dir(&self.root)
    }

    /// The activation backend nxd last used; `None` before the first
    /// activation.
    pub fn backend(&self) -> Result<Option<Backend>> {
        let path = self.backend_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => Backend::parse(text.trim()).map(Some).ok_or_else(|| {
                Error::Invalid(format!("{} says {:?}", path.display(), text.trim()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Read { path, source }),
        }
    }

    /// Records the backend in use, for `nx shell`. Written beside its name
    /// with `create_new` (which never follows a symlink: nxd runs this as
    /// root in a directory others can write) and flushed before the rename.
    pub fn set_backend(&self, backend: Backend) -> Result<()> {
        use std::io::Write;
        let path = self.backend_file();
        let tmp = path.with_file_name(".backend.part");
        let write = |source| Error::Write {
            path: tmp.clone(),
            source,
        };
        let _ = std::fs::remove_file(&tmp);
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(write)?;
        file.write_all(format!("{}\n", backend.name()).as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(write)?;
        rename(&tmp, &path)?;
        sync_dir(&self.root)
    }

    fn backend_file(&self) -> PathBuf {
        self.root.join("backend")
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

    /// What the package's signed header said about its scriptlets and
    /// files outside `/usr`, recorded when it was unpacked. `None` for an
    /// entry unpacked before nx read them.
    pub fn meta(&self, sha256: &str) -> Result<Option<nx_scriptlet::PackageMeta>> {
        let path = self.path(sha256).join(META_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| Error::Toml {
                    path,
                    message: e.to_string(),
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Read { path, source }),
        }
    }

    /// Unpacks a package beside the store: `fill` writes its tree into an
    /// empty directory, which [`Store::commit`] later moves in. `None` when
    /// the store has it already. Needs the state lock.
    pub fn stage<E>(
        &self,
        _lock: &Lock,
        sha256: &str,
        fill: impl FnOnce(&Path) -> Result<(), E>,
    ) -> Result<Option<Staged>, E>
    where
        E: From<Error>,
    {
        let valid = sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(Error::Invalid(format!("not a SHA-256: {sha256:?}")).into());
        }
        let dest = self.path(sha256);
        if dest.is_dir() {
            return Ok(None);
        }
        let tmp = self.dir.join(format!(".{sha256}.part"));
        remove_tree(&tmp)?;
        create_dir(&tmp)?;
        if let Err(e) = fill(&tmp) {
            let _ = remove_tree(&tmp);
            return Err(e);
        }
        Ok(Some(Staged { tmp, dest }))
    }

    /// Moves staged packages into the store, once their data is on disk:
    /// one filesystem sync for all of them, then a rename each.
    pub fn commit(&self, _lock: &Lock, staged: Vec<Staged>) -> Result<()> {
        if staged.is_empty() {
            return Ok(());
        }
        sync_fs(&self.dir)?;
        for s in staged {
            rename(&s.tmp, &s.dest)?;
        }
        sync_dir(&self.dir)
    }

    /// [`Store::stage`] and [`Store::commit`] for one package.
    pub fn insert<E>(
        &self,
        lock: &Lock,
        sha256: &str,
        fill: impl FnOnce(&Path) -> Result<(), E>,
    ) -> Result<PathBuf, E>
    where
        E: From<Error>,
    {
        let staged = self.stage(lock, sha256, fill)?;
        self.commit(lock, staged.into_iter().collect())?;
        Ok(self.path(sha256))
    }
}

/// Where a store entry keeps its package record, beside the payload's top
/// level. A package that ships a file of this name fails to unpack, since
/// the record is created exclusively.
pub const META_FILE: &str = ".nx-meta.json";

/// Records `meta` in the store entry (or staging directory) `dir`.
pub fn write_meta(dir: &Path, meta: &nx_scriptlet::PackageMeta) -> Result<()> {
    use std::io::Write;
    let path = dir.join(META_FILE);
    let text = serde_json::to_string(meta).map_err(|e| Error::Serialize {
        what: "package record",
        message: e.to_string(),
    })?;
    File::options()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|source| Error::Write { path, source })
}

/// A package unpacked beside the store, waiting for [`Store::commit`].
#[derive(Debug)]
pub struct Staged {
    tmp: PathBuf,
    dest: PathBuf,
}

/// Bytes free to an unprivileged writer on the filesystem holding `path`
/// (or its nearest existing parent).
pub fn free_space(path: &Path) -> Result<u64> {
    let mut at = path;
    while !at.exists() {
        match at.parent() {
            Some(p) => at = p,
            None => break,
        }
    }
    let st = rustix::fs::statvfs(at).map_err(|e| Error::Read {
        path: at.to_owned(),
        source: e.into(),
    })?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Whether two existing paths are on the same filesystem.
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

/// Flushes everything written to the filesystem holding `dir`. One
/// `syncfs` costs far less than an `fsync` per unpacked file.
pub(crate) fn sync_fs(dir: &Path) -> Result<()> {
    let f = File::open(dir).map_err(|source| Error::Read {
        path: dir.to_owned(),
        source,
    })?;
    rustix::fs::syncfs(&f).map_err(|e| Error::Write {
        path: dir.to_owned(),
        source: e.into(),
    })
}

/// Makes renames inside `dir` durable.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|source| Error::Write {
            path: dir.to_owned(),
            source,
        })
}

pub(crate) fn rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(|source| Error::Write {
        path: to.to_owned(),
        source,
    })
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
    fn current_and_backend_are_recorded() -> Result<()> {
        let tmp = TempDir::new("current");
        let state = State::new(&tmp.0);
        let _lock = state.lock()?;
        assert!(state.set_current(1).is_err());
        create_dir(&state.generations_dir().join("1"))?;
        create_dir(&state.generations_dir().join("2"))?;
        state.set_current(1)?;
        state.set_current(2)?;
        assert_eq!(state.current_number()?, Some(2));
        assert_eq!(state.backend()?, None);
        state.set_backend(Backend::Prefix)?;
        assert_eq!(state.backend()?, Some(Backend::Prefix));
        Ok(())
    }

    #[test]
    fn staged_packages_appear_only_on_commit() -> Result<()> {
        let tmp = TempDir::new("stage");
        let state = State::new(&tmp.0);
        let lock = state.lock()?;
        let store = state.store();
        let staged = store.stage(&lock, SHA, |_| Ok::<(), Error>(()))?;
        assert!(staged.is_some() && !store.contains(SHA));
        store.commit(&lock, staged.into_iter().collect())?;
        assert!(store.contains(SHA));
        assert!(store.stage(&lock, SHA, |_| Ok::<(), Error>(()))?.is_none());
        assert!(free_space(&tmp.0.join("not/yet"))? > 0);
        Ok(())
    }

    #[test]
    fn the_shared_cache_is_made_where_the_state_is_writable() -> Result<()> {
        let tmp = TempDir::new("shared-cache");
        let state = State::new(tmp.0.join("state"));
        // No state directory yet: nothing to put a cache in.
        assert_eq!(state.shared_cache(), None);
        create_dir(state.root())?;
        let cache = state.shared_cache();
        assert_eq!(cache, Some(state.root().join("cache")));
        assert!(state.root().join("cache").is_dir());
        assert_eq!(state.shared_cache(), cache);
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
        assert!(!store.dir.join(format!(".{SHA}.part")).exists());
        Ok(())
    }
}
