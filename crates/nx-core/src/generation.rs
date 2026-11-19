//! Generations: one complete, immutable host tier each, built for one
//! image digest. Building assembles the extension tree from the store; it
//! never activates anything (that is nxd's job).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::lockfile::Lock;
use crate::manifest::Manifest;
use crate::state::{self, State};
use crate::{Error, Result};

/// A generation on disk.
#[derive(Debug, Clone)]
pub struct Generation {
    pub number: u32,
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub lock: Lock,
}

impl Generation {
    pub fn read(state: &State, number: u32) -> Result<Self> {
        let dir = state.generations_dir().join(number.to_string());
        let manifest_path = dir.join("manifest.toml");
        let lock_path = dir.join("lock.toml");
        Ok(Self {
            number,
            manifest: Manifest::parse(&read(&manifest_path)?, &manifest_path)?,
            lock: Lock::parse(&read(&lock_path)?, &lock_path)?,
            dir,
        })
    }

    /// The generation `current` points at, if any.
    pub fn current(state: &State) -> Result<Option<Self>> {
        state
            .current_number()?
            .map(|n| Self::read(state, n))
            .transpose()
    }

    /// The extension tree: what would appear under `/`.
    pub fn sysext(&self) -> PathBuf {
        self.dir.join("sysext")
    }
}

/// What to put in a new generation.
pub struct Spec<'a> {
    pub manifest: &'a Manifest,
    pub lock: &'a Lock,
    /// The image's root filesystem (the booted deployment's), checked for
    /// files the extension would shadow.
    pub image_root: &'a Path,
    /// The image's os-release `ID`, for the extension-release file.
    pub os_id: &'a str,
}

/// A built generation, and what was left out of it.
#[derive(Debug)]
pub struct Built {
    pub generation: Generation,
    /// Packages with files outside `/usr`, which an extension cannot carry;
    /// they wait for the scriptlet translator (tmpfiles, sysusers).
    pub outside_usr: Vec<String>,
}

/// Where the extension declares which OS it extends.
pub const EXTENSION_RELEASE: &str = "usr/lib/extension-release.d/extension-release.nx";

/// Builds generation N+1 from packages already in the store and points
/// `current` at it. Refuses, leaving `current` alone, when two packages
/// disagree about a path or a package would shadow a file of the image
/// (design invariant 1).
pub fn build(state: &State, _lock: &state::Lock, spec: &Spec) -> Result<Built> {
    let number = state.generations()?.last().map_or(1, |n| n + 1);
    let dir = state.generations_dir();
    let tmp = dir.join(format!(".{number}.part"));
    state::remove_tree(&tmp)?;
    let sysext = tmp.join("sysext");
    state::create_dir(&sysext)?;

    let store = state.store();
    let mut tree = Tree::default();
    let mut outside_usr = Vec::new();
    for p in &spec.lock.host {
        let root = store.path(&p.sha256);
        if !root.is_dir() {
            return Err(Error::Invalid(format!("{} is not in the store", p.nevra)));
        }
        if top_level(&root)?.iter().any(|name| name != "usr") {
            outside_usr.push(p.nevra.clone());
        }
        tree.add(&root, Path::new("usr"), &p.nevra)?;
    }
    tree.check_image(spec.image_root)?;
    tree.write(&sysext)?;

    let release = sysext.join(EXTENSION_RELEASE);
    write(
        &release,
        &format!("ID={}\nVERSION_ID={}\n", spec.os_id, spec.lock.release),
    )?;
    write(&tmp.join("manifest.toml"), &spec.manifest.to_toml())?;
    write(&tmp.join("lock.toml"), &spec.lock.to_toml())?;

    let final_dir = dir.join(number.to_string());
    rename(&tmp, &final_dir)?;
    swap_current(state, number)?;
    Ok(Built {
        generation: Generation::read(state, number)?,
        outside_usr,
    })
}

/// Points `current` at generation `number` with one rename.
fn swap_current(state: &State, number: u32) -> Result<()> {
    let link = state.current_link();
    let tmp = link.with_file_name(".current.part");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(format!("generations/{number}"), &tmp).map_err(|source| {
        Error::Write {
            path: tmp.clone(),
            source,
        }
    })?;
    rename(&tmp, &link)
}

/// One path in the extension and where it comes from.
#[derive(Debug)]
enum Node {
    Dir { mode: u32 },
    File { source: PathBuf, owner: String },
    Link { target: PathBuf, owner: String },
}

/// The merged tree of every package, keyed by path under the extension
/// root (`usr/bin/rg`), so parents sort before children.
#[derive(Debug, Default)]
struct Tree(BTreeMap<PathBuf, Node>);

impl Tree {
    /// Adds `root/rel` and everything under it, owned by `owner`.
    fn add(&mut self, root: &Path, rel: &Path, owner: &str) -> Result<()> {
        let path = root.join(rel);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(Error::Read { path, source }),
        };
        let node = if meta.is_dir() {
            use std::os::unix::fs::PermissionsExt;
            Node::Dir {
                mode: meta.permissions().mode() & 0o7777,
            }
        } else if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path).map_err(|source| Error::Read {
                path: path.clone(),
                source,
            })?;
            Node::Link {
                target,
                owner: owner.to_owned(),
            }
        } else {
            Node::File {
                source: path.clone(),
                owner: owner.to_owned(),
            }
        };
        self.insert(rel, node)?;
        if meta.is_dir() {
            let entries = std::fs::read_dir(&path).map_err(|source| Error::Read {
                path: path.clone(),
                source,
            })?;
            let mut names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
            names.sort();
            for name in names {
                self.add(root, &rel.join(name), owner)?;
            }
        }
        Ok(())
    }

    fn insert(&mut self, rel: &Path, node: Node) -> Result<()> {
        let Some(have) = self.0.get(rel) else {
            self.0.insert(rel.to_owned(), node);
            return Ok(());
        };
        let same = match (have, &node) {
            (Node::Dir { .. }, Node::Dir { .. }) => true,
            (Node::Link { target: a, .. }, Node::Link { target: b, .. }) => a == b,
            // Multilib and -common packages ship identical copies.
            (Node::File { source: a, .. }, Node::File { source: b, .. }) => same_content(a, b)?,
            _ => false,
        };
        if same {
            return Ok(());
        }
        Err(Error::Conflict(format!(
            "/{} is in both {} and {}",
            rel.display(),
            owner(have),
            owner(&node)
        )))
    }

    /// Design invariant 1: no file of the extension may hide one of the
    /// image's. Directories merge; anything else is refused.
    fn check_image(&self, image_root: &Path) -> Result<()> {
        let mut clashes = Vec::new();
        for (rel, node) in &self.0 {
            let Ok(meta) = std::fs::symlink_metadata(image_root.join(rel)) else {
                continue;
            };
            let clash = match node {
                Node::Dir { .. } => !meta.is_dir(),
                _ => true,
            };
            if clash {
                clashes.push(format!("/{} ({})", rel.display(), owner(node)));
            }
        }
        if clashes.is_empty() {
            return Ok(());
        }
        let more = clashes.len().saturating_sub(5);
        clashes.truncate(5);
        let mut msg = format!("the image already has {}", clashes.join(", "));
        if more > 0 {
            msg.push_str(&format!(" and {more} more"));
        }
        Err(Error::Conflict(msg))
    }

    /// Hardlinks files from the store (copies across filesystems).
    fn write(&self, dest: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut dirs = Vec::new();
        for (rel, node) in &self.0 {
            let to = dest.join(rel);
            match node {
                Node::Dir { mode } => {
                    state::create_dir(&to)?;
                    dirs.push((to, *mode));
                }
                Node::File { source, .. } => {
                    if std::fs::hard_link(source, &to).is_err() {
                        std::fs::copy(source, &to).map_err(|source| Error::Write {
                            path: to.clone(),
                            source,
                        })?;
                    }
                }
                Node::Link { target, .. } => {
                    std::os::unix::fs::symlink(target, &to).map_err(|source| Error::Write {
                        path: to.clone(),
                        source,
                    })?;
                }
            }
        }
        for (dir, mode) in dirs.into_iter().rev() {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).map_err(
                |source| Error::Write {
                    path: dir.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }
}

fn owner(node: &Node) -> &str {
    match node {
        Node::Dir { .. } => "a directory",
        Node::File { owner, .. } | Node::Link { owner, .. } => owner,
    }
}

fn same_content(a: &Path, b: &Path) -> Result<bool> {
    Ok(read_bytes(a)? == read_bytes(b)?)
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })
}

fn top_level(root: &Path) -> Result<Vec<String>> {
    let entries = std::fs::read_dir(root).map_err(|source| Error::Read {
        path: root.to_owned(),
        source,
    })?;
    Ok(entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect())
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        state::create_dir(parent)?;
    }
    std::fs::write(path, text).map_err(|source| Error::Write {
        path: path.to_owned(),
        source,
    })
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(|source| Error::Write {
        path: to.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::LockedPackage;
    use crate::state::tests::TempDir;

    fn sha(n: u8) -> String {
        format!("{n:064x}")
    }

    /// A state with packages in the store: (sha, [(path, content)]).
    /// Content starting with `->` makes a symlink.
    fn setup(tmp: &TempDir, pkgs: &[(&str, &[(&str, &str)])]) -> Result<(State, Lock)> {
        let state = State::new(tmp.0.join("state"));
        let guard = state.lock()?;
        let mut lock = Lock::new(Some("sha256:ab".into()), "45", "x86_64");
        for (i, (name, files)) in pkgs.iter().enumerate() {
            let sha = sha(i as u8 + 1);
            state.store().insert(&guard, &sha, |dir| {
                for (path, content) in *files {
                    let to = dir.join(path);
                    state::create_dir(to.parent().unwrap_or(dir))?;
                    match content.strip_prefix("->") {
                        Some(target) => std::os::unix::fs::symlink(target, &to)
                            .map_err(|source| Error::Write { path: to, source })?,
                        None => write(&to, content)?,
                    }
                }
                Ok::<(), Error>(())
            })?;
            lock.host.push(LockedPackage {
                name: (*name).into(),
                nevra: format!("{name}-1-1.fc45.x86_64"),
                repo: "fedora".into(),
                sha256: sha,
                requested: vec![(*name).into()],
            });
        }
        Ok((state, lock))
    }

    fn image(tmp: &TempDir, files: &[&str]) -> Result<PathBuf> {
        let root = tmp.0.join("image");
        state::create_dir(&root.join("usr/include"))?;
        for f in files {
            write(&root.join(f), "image")?;
        }
        Ok(root)
    }

    fn spec<'a>(manifest: &'a Manifest, lock: &'a Lock, image_root: &'a Path) -> Spec<'a> {
        Spec {
            manifest,
            lock,
            image_root,
            os_id: "fedora",
        }
    }

    #[test]
    fn assembles_and_points_current_at_it() -> Result<()> {
        let tmp = TempDir::new("gen");
        let (state, lock) = setup(
            &tmp,
            &[
                (
                    "libxkbcommon-devel",
                    &[
                        ("usr/include/xkbcommon/xkbcommon.h", "h"),
                        ("usr/lib64/libxkbcommon.so", "->libxkbcommon.so.0"),
                    ],
                ),
                (
                    "filesystem-extra",
                    &[("etc/thing.conf", "c"), ("usr/share/x", "x")],
                ),
            ],
        )?;
        let root = image(&tmp, &["usr/lib64/libxkbcommon.so.0"])?;
        let manifest = Manifest::default();
        let guard = state.lock()?;
        let built = build(&state, &guard, &spec(&manifest, &lock, &root))?;
        let g = &built.generation;
        assert_eq!(g.number, 1);
        assert_eq!(built.outside_usr, ["filesystem-extra-1-1.fc45.x86_64"]);
        let sysext = g.sysext();
        assert_eq!(
            std::fs::read_to_string(sysext.join("usr/include/xkbcommon/xkbcommon.h")).ok(),
            Some("h".into())
        );
        assert!(!sysext.join("etc").exists());
        assert_eq!(
            std::fs::read_link(sysext.join("usr/lib64/libxkbcommon.so")).ok(),
            Some(PathBuf::from("libxkbcommon.so.0"))
        );
        assert_eq!(
            std::fs::read_to_string(sysext.join(EXTENSION_RELEASE)).ok(),
            Some("ID=fedora\nVERSION_ID=45\n".into())
        );
        assert_eq!(g.lock, lock);
        assert_eq!(Generation::current(&state)?.map(|g| g.number), Some(1));

        let again = build(&state, &guard, &spec(&manifest, &lock, &root))?;
        assert_eq!(again.generation.number, 2);
        assert_eq!(state.current_number()?, Some(2));
        assert_eq!(state.generations()?, [1, 2]);
        Ok(())
    }

    #[test]
    fn never_shadows_the_image() -> Result<()> {
        let tmp = TempDir::new("shadow");
        let (state, lock) = setup(&tmp, &[("pkgconf", &[("usr/bin/pkgconf", "new")])])?;
        let root = image(&tmp, &["usr/bin/pkgconf"])?;
        let manifest = Manifest::default();
        let guard = state.lock()?;
        let err = build(&state, &guard, &spec(&manifest, &lock, &root));
        let Err(Error::Conflict(msg)) = err else {
            panic!("expected a conflict, got {err:?}");
        };
        assert_eq!(
            msg,
            "the image already has /usr/bin/pkgconf (pkgconf-1-1.fc45.x86_64)"
        );
        assert_eq!(state.current_number()?, None);
        assert_eq!(state.generations()?, Vec::<u32>::new());
        Ok(())
    }

    #[test]
    fn packages_may_share_identical_files_only() -> Result<()> {
        let tmp = TempDir::new("share");
        let root = image(&tmp, &[])?;
        let manifest = Manifest::default();
        let (state, lock) = setup(
            &tmp,
            &[
                ("a", &[("usr/share/doc/x", "same")]),
                ("b", &[("usr/share/doc/x", "same")]),
            ],
        )?;
        let guard = state.lock()?;
        build(&state, &guard, &spec(&manifest, &lock, &root))?;
        drop(guard);

        let tmp = TempDir::new("clash");
        let root = image(&tmp, &[])?;
        let (state, lock) = setup(
            &tmp,
            &[
                ("a", &[("usr/bin/x", "one")]),
                ("b", &[("usr/bin/x", "two")]),
            ],
        )?;
        let guard = state.lock()?;
        let err = build(&state, &guard, &spec(&manifest, &lock, &root));
        assert!(
            matches!(&err, Err(Error::Conflict(m)) if m == "/usr/bin/x is in both a-1-1.fc45.x86_64 and b-1-1.fc45.x86_64"),
            "{err:?}"
        );
        Ok(())
    }
}
