//! Generations: one complete, immutable host tier each, built for one
//! image digest. Building assembles the extension tree from the store; it
//! never activates anything (that is nxd's job).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use nx_scriptlet::{Context, Intents, Leftover};
use serde::{Deserialize, Serialize};

use crate::lockfile::Lock;
use crate::manifest::Manifest;
use crate::state::{self, State};
use crate::{Error, FORMAT, Result};

/// A generation on disk.
#[derive(Debug, Clone)]
pub struct Generation {
    pub number: u32,
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub lock: Lock,
    /// What its packages' scriptlets mean for units (empty for a
    /// generation built before nx read scriptlets).
    pub activation: Activation,
}

/// `activation.toml`: what making a generation live asks of systemd, as
/// translated from its packages' scriptlets. nxd diffs the outgoing and
/// incoming generation's records: a package that appears gets its presets
/// applied, one that disappears has its units stopped and disabled, one
/// whose version changes has its units restarted if running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activation {
    #[serde(default = "format")]
    pub version: u32,
    #[serde(default, rename = "package")]
    pub packages: Vec<ActivationPackage>,
}

fn format() -> u32 {
    FORMAT
}

impl Default for Activation {
    fn default() -> Self {
        Self {
            version: FORMAT,
            packages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationPackage {
    pub name: String,
    pub nevra: String,
    #[serde(flatten)]
    pub intents: Intents,
    /// Names of the `sysusers.d` files the package ships or nx wrote for
    /// it, which nxd applies when the package appears or changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sysusers_files: Vec<String>,
    /// The same for `tmpfiles.d`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmpfiles_files: Vec<String>,
}

impl Activation {
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let a: Self = toml::from_str(text).map_err(|e| Error::Toml {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        if a.version > FORMAT {
            return Err(Error::Newer {
                path: path.to_owned(),
                version: a.version,
            });
        }
        Ok(a)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).map_err(|e| Error::Serialize {
            what: "activation.toml",
            message: e.to_string(),
        })
    }

    pub fn package(&self, name: &str) -> Option<&ActivationPackage> {
        self.packages.iter().find(|p| p.name == name)
    }
}

/// A package whose scriptlets nx won't translate, and the text it left.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub name: String,
    pub nevra: String,
    /// What an allowlist entry for it must say.
    pub digest: String,
    pub leftovers: Vec<Leftover>,
}

impl Generation {
    pub fn read(state: &State, number: u32) -> Result<Self> {
        let dir = state.generations_dir().join(number.to_string());
        let manifest_path = dir.join("manifest.toml");
        let lock_path = dir.join("lock.toml");
        let activation_path = dir.join("activation.toml");
        let activation = match std::fs::read_to_string(&activation_path) {
            Ok(text) => Activation::parse(&text, &activation_path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Activation::default(),
            Err(source) => {
                return Err(Error::Read {
                    path: activation_path,
                    source,
                });
            }
        };
        Ok(Self {
            number,
            manifest: Manifest::parse(&read(&manifest_path)?, &manifest_path)?,
            lock: Lock::parse(&read(&lock_path)?, &lock_path)?,
            activation,
            dir,
        })
    }

    /// The generation `current` points at, if any. Fails when a newer nx
    /// wrote it: a change built on top would lose what that nx recorded.
    pub fn current(state: &State) -> Result<Option<Self>> {
        state
            .current_number()?
            .map(|n| Self::read(state, n))
            .transpose()
    }

    /// What a read-only command shows (design invariant 9): the current
    /// generation or, when a newer nx wrote it, the newest one this nx can
    /// read.
    pub fn readable(state: &State) -> Result<Readable> {
        let Some(current) = state.current_number()? else {
            return Ok(Readable::default());
        };
        match Self::read(state, current) {
            Ok(g) => Ok(Readable {
                generation: Some(g),
                newer: None,
            }),
            Err(Error::Newer { .. }) => {
                let generation = state
                    .generations()?
                    .into_iter()
                    .rev()
                    .find_map(|n| Self::read(state, n).ok());
                Ok(Readable {
                    generation,
                    newer: Some(current),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// The extension tree: what would appear under `/`.
    pub fn sysext(&self) -> PathBuf {
        self.dir.join("sysext")
    }
}

/// What [`Generation::readable`] found.
#[derive(Debug, Default)]
pub struct Readable {
    pub generation: Option<Generation>,
    /// Set when `current` is this generation, written by a newer nx.
    pub newer: Option<u32>,
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
    /// Whether the image has a package of this name: a trigger watching it
    /// can fire.
    pub image_has: &'a dyn Fn(&str) -> bool,
}

/// A built generation, and what was left out of it.
#[derive(Debug)]
pub struct Built {
    pub generation: Generation,
    /// Packages with files an extension can't carry and tmpfiles can't
    /// place either (under `/opt`, `/srv`, `/boot`…); they are left out.
    pub outside_usr: Vec<String>,
    /// Packages whose leftover scriptlets the manifest allows.
    pub allowed: Vec<String>,
}

/// Where the extension declares which OS it extends.
pub const EXTENSION_RELEASE: &str = "usr/lib/extension-release.d/extension-release.nx";

/// Builds generation N+1 from packages already in the store and checks
/// what it wrote. It does not activate it: `current` stays where it was
/// until [`State::set_current`] (nxd, or the CLI when there is no nxd).
/// Refuses when two packages disagree about a path or a package would
/// shadow a file of the image (design invariant 1).
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
    let mut allowed = Vec::new();
    let mut refusals = Vec::new();
    let mut activation = Activation::default();
    // Files the extension gains that no package ships: sysusers.d and
    // tmpfiles.d translations.
    let mut generated: Vec<(String, String)> = Vec::new();
    let names: HashSet<&str> = spec.lock.host.iter().map(|p| p.name.as_str()).collect();
    let ctx = Context {
        present: &|n| names.contains(n) || (spec.image_has)(n),
        removable: &|n| names.contains(n),
    };
    for p in &spec.lock.host {
        let root = store.path(&p.sha256);
        if !root.is_dir() {
            return Err(Error::Invalid(format!("{} is not in the store", p.nevra)));
        }
        let meta = store
            .meta(&p.sha256)?
            .ok_or_else(|| Error::Invalid(format!("{} has no recorded scriptlets", p.nevra)))?;
        let translation = nx_scriptlet::translate(&meta.scripts, &ctx);
        if !translation.leftovers.is_empty() {
            let digest = translation.digest();
            let ok = spec
                .manifest
                .scriptlets
                .iter()
                .any(|a| a.package == p.name && a.matches(&digest));
            if ok {
                allowed.push(p.name.clone());
            } else {
                refusals.push(Refusal {
                    name: p.name.clone(),
                    nevra: p.nevra.clone(),
                    digest,
                    leftovers: translation.leftovers,
                });
                continue;
            }
        }
        let tmpfiles = nx_scriptlet::tmpfiles(&meta.files);
        let mut left_out = !tmpfiles.unplaced.is_empty();
        for name in top_level(&root)? {
            let into = match name.as_str() {
                "usr" => "usr".to_owned(),
                // The usrmerge links of the image.
                "bin" | "sbin" | "lib" | "lib64" => format!("usr/{name}"),
                // Config and state defaults: copied into place by tmpfiles.
                "etc" | "var" => format!("usr/share/factory/{name}"),
                // /run holds directories tmpfiles creates; the record says
                // whether it holds anything else.
                "run" => continue,
                n if n.starts_with(".nx-") => continue,
                _ => {
                    left_out = true;
                    continue;
                }
            };
            tree.add(&root, Path::new(&name), Path::new(&into), &p.nevra)?;
        }
        if left_out {
            outside_usr.push(p.nevra.clone());
        }
        let file_stem = format!("nx-{}", p.name);
        let mut sysusers_files = conf_files(&root, "sysusers.d");
        let mut tmpfiles_files = conf_files(&root, "tmpfiles.d");
        if !translation.intents.sysusers.is_empty() {
            generated.push((
                format!("usr/lib/sysusers.d/{file_stem}.conf"),
                generated_file(&p.nevra, &translation.intents.sysusers),
            ));
            sysusers_files.push(format!("{file_stem}.conf"));
        }
        if !tmpfiles.lines.is_empty() {
            generated.push((
                format!("usr/lib/tmpfiles.d/{file_stem}.conf"),
                generated_file(&p.nevra, &tmpfiles.lines),
            ));
            tmpfiles_files.push(format!("{file_stem}.conf"));
        }
        activation.packages.push(ActivationPackage {
            name: p.name.clone(),
            nevra: p.nevra.clone(),
            intents: translation.intents,
            sysusers_files,
            tmpfiles_files,
        });
    }
    if !refusals.is_empty() {
        return Err(Error::Scriptlets(refusals));
    }
    let tree = tree.follow_image_links(spec.image_root)?;
    tree.check_image(spec.image_root)?;
    tree.write(&sysext)?;
    tree.verify(&sysext)?;
    for (rel, text) in &generated {
        write(&sysext.join(rel), text)?;
    }

    let release = sysext.join(EXTENSION_RELEASE);
    write(
        &release,
        &format!("ID={}\nVERSION_ID={}\n", spec.os_id, spec.lock.release),
    )?;
    let manifest_path = tmp.join("manifest.toml");
    let lock_path = tmp.join("lock.toml");
    write(&manifest_path, &spec.manifest.to_toml()?)?;
    write(&lock_path, &spec.lock.to_toml()?)?;
    let activation_path = tmp.join("activation.toml");
    write(&activation_path, &activation.to_toml()?)?;
    // What a later command will read back must be what was asked for.
    let readback = Manifest::parse(&read(&manifest_path)?, &manifest_path)? == *spec.manifest
        && Lock::parse(&read(&lock_path)?, &lock_path)? == *spec.lock;
    if !readback {
        return Err(Error::Invalid(format!(
            "{} does not read back as written",
            tmp.display()
        )));
    }

    state::sync_fs(&dir)?;
    let final_dir = dir.join(number.to_string());
    state::rename(&tmp, &final_dir)?;
    state::sync_dir(&dir)?;
    Ok(Built {
        generation: Generation::read(state, number)?,
        outside_usr,
        allowed,
    })
}

/// The `.conf` files a package ships in `usr/lib/<dir>` (or `lib/<dir>`).
fn conf_files(root: &Path, dir: &str) -> Vec<String> {
    let mut names = Vec::new();
    for base in ["usr/lib", "lib"] {
        if let Ok(entries) = std::fs::read_dir(root.join(base).join(dir)) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".conf") && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names.sort();
    names
}

fn generated_file(nevra: &str, lines: &[String]) -> String {
    let mut text = format!("# Written by nx from {nevra}; nx never runs its scriptlets.\n");
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    text
}

/// Design invariant 1 for an extension tree already on disk (nxd checks
/// it again at activation: the image may have moved since the build, as
/// with a rollback): no file of `extension` may hide one of the image's.
/// Directories merge; anything else is refused.
pub fn check_extension(extension: &Path, image_root: &Path) -> Result<()> {
    let mut clashes = Vec::new();
    collect_clashes(extension, Path::new(""), image_root, &mut clashes)?;
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

fn collect_clashes(
    extension: &Path,
    rel: &Path,
    image_root: &Path,
    clashes: &mut Vec<String>,
) -> Result<()> {
    let dir = extension.join(rel);
    let entries = std::fs::read_dir(&dir).map_err(|source| Error::Read {
        path: dir.clone(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Read {
            path: dir.clone(),
            source,
        })?;
        let rel = rel.join(entry.file_name());
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if let Ok(meta) = std::fs::symlink_metadata(image_root.join(&rel))
            && (!is_dir || !meta.is_dir())
        {
            clashes.push(format!("/{}", rel.display()));
        }
        if is_dir {
            collect_clashes(extension, &rel, image_root, clashes)?;
        }
    }
    Ok(())
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
    /// Adds `root/from` and everything under it, as `to` in the extension,
    /// owned by `owner`.
    fn add(&mut self, root: &Path, from: &Path, to: &Path, owner: &str) -> Result<()> {
        let rel = to;
        let path = root.join(from);
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
                self.add(root, &from.join(&name), &rel.join(&name), owner)?;
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

    /// Moves paths under a directory the image has as a symlink to where
    /// that symlink leads, as rpm would when installing onto the image:
    /// with `/usr/sbin -> bin`, a package's `/usr/sbin/foo` becomes
    /// `/usr/bin/foo`. A real `usr/sbin` in the extension would hide the
    /// image's symlink.
    fn follow_image_links(self, image_root: &Path) -> Result<Tree> {
        let mut out = Tree::default();
        // A package directory → the image directory it lands in.
        let mut moved: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        // Parents sort before their children, so a directory's move is
        // known before anything under it.
        for (rel, node) in self.0 {
            let rel = moved_path(&moved, &rel);
            if matches!(node, Node::Dir { .. })
                && let Some(to) = image_dir_link(image_root, &rel)
            {
                moved.insert(rel, to);
                continue;
            }
            out.insert(&rel, node)?;
        }
        Ok(out)
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

    /// Checks that `dest` holds exactly what [`Tree::write`] was to put
    /// there.
    fn verify(&self, dest: &Path) -> Result<()> {
        for (rel, node) in &self.0 {
            let path = dest.join(rel);
            let meta = std::fs::symlink_metadata(&path).map_err(|source| Error::Read {
                path: path.clone(),
                source,
            })?;
            let ok = match node {
                Node::Dir { .. } => meta.is_dir(),
                Node::File { source, .. } => {
                    meta.is_file() && std::fs::metadata(source).is_ok_and(|s| s.len() == meta.len())
                }
                Node::Link { target, .. } => std::fs::read_link(&path).is_ok_and(|t| t == *target),
            };
            if !ok {
                return Err(Error::Invalid(format!(
                    "{} was not written as planned",
                    path.display()
                )));
            }
        }
        Ok(())
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

/// `rel` under the directory moves made so far.
fn moved_path(moved: &BTreeMap<PathBuf, PathBuf>, rel: &Path) -> PathBuf {
    for dir in rel.ancestors() {
        if let Some(to) = moved.get(dir) {
            let rest = rel.strip_prefix(dir).unwrap_or(Path::new(""));
            return to.join(rest);
        }
    }
    rel.to_owned()
}

/// Where the image's symlink at `rel` leads, when it is a symlink to a
/// directory inside `/usr`, followed through further symlinks.
fn image_dir_link(image_root: &Path, rel: &Path) -> Option<PathBuf> {
    let mut at = rel.to_owned();
    let mut followed = false;
    for _ in 0..8 {
        let full = image_root.join(&at);
        let meta = std::fs::symlink_metadata(&full).ok()?;
        if meta.is_dir() {
            return followed.then_some(at);
        }
        if !meta.file_type().is_symlink() {
            return None;
        }
        let target = std::fs::read_link(&full).ok()?;
        let joined = if target.is_absolute() {
            target
        } else {
            Path::new("/").join(at.parent()?).join(target)
        };
        at = normalize(&joined)?;
        if !at.starts_with("usr") {
            return None;
        }
        followed = true;
    }
    None
}

/// `/usr/lib/../bin` → `usr/bin`; `None` if it climbs above the root.
fn normalize(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Some(out)
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
        setup_meta(tmp, pkgs, &|_| nx_scriptlet::PackageMeta::default())
    }

    /// [`setup`], with each package's recorded header data from `meta`.
    fn setup_meta(
        tmp: &TempDir,
        pkgs: &[(&str, &[(&str, &str)])],
        meta: &dyn Fn(&str) -> nx_scriptlet::PackageMeta,
    ) -> Result<(State, Lock)> {
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
                state::write_meta(dir, &meta(name))?;
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
            image_has: &|_| false,
        }
    }

    fn script(body: &str) -> nx_scriptlet::Script {
        nx_scriptlet::Script {
            interpreter: String::new(),
            body: body.into(),
        }
    }

    fn pkg_file(path: &str, mode: u32, flags: u32) -> nx_scriptlet::PackageFile {
        nx_scriptlet::PackageFile {
            path: path.into(),
            mode,
            user: "svc".into(),
            group: "svc".into(),
            flags,
            link: String::new(),
        }
    }

    #[test]
    fn assembles_without_activating() -> Result<()> {
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
        assert_eq!(built.outside_usr, Vec::<String>::new());
        let sysext = g.sysext();
        assert_eq!(
            std::fs::read_to_string(sysext.join("usr/include/xkbcommon/xkbcommon.h")).ok(),
            Some("h".into())
        );
        // /etc content waits in the factory for tmpfiles to copy.
        assert!(!sysext.join("etc").exists());
        assert_eq!(
            std::fs::read_to_string(sysext.join("usr/share/factory/etc/thing.conf")).ok(),
            Some("c".into())
        );
        assert_eq!(
            std::fs::read_link(sysext.join("usr/lib64/libxkbcommon.so")).ok(),
            Some(PathBuf::from("libxkbcommon.so.0"))
        );
        assert_eq!(
            std::fs::read_to_string(sysext.join(EXTENSION_RELEASE)).ok(),
            Some("ID=fedora\nVERSION_ID=45\n".into())
        );
        assert_eq!(g.lock, lock);
        assert_eq!(state.current_number()?, None);
        state.set_current(1)?;
        assert_eq!(Generation::current(&state)?.map(|g| g.number), Some(1));

        let again = build(&state, &guard, &spec(&manifest, &lock, &root))?;
        assert_eq!(again.generation.number, 2);
        assert_eq!(state.current_number()?, Some(1));
        assert_eq!(state.generations()?, [1, 2]);
        Ok(())
    }

    #[test]
    fn an_extension_on_disk_is_checked_against_a_newer_image() -> Result<()> {
        let tmp = TempDir::new("recheck");
        let (state, lock) = setup(&tmp, &[("pkgconf", &[("usr/bin/pkgconf", "new")])])?;
        let root = image(&tmp, &[])?;
        let manifest = Manifest::default();
        let guard = state.lock()?;
        let built = build(&state, &guard, &spec(&manifest, &lock, &root))?;
        let sysext = built.generation.sysext();
        check_extension(&sysext, &root)?;
        // The image grows the file the extension carries.
        let newer = image(&tmp, &["usr/bin/pkgconf"])?;
        let err = check_extension(&sysext, &newer)
            .err()
            .map(|e| e.to_string());
        assert_eq!(
            err.as_deref(),
            Some("the image already has /usr/bin/pkgconf")
        );
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
    fn follows_the_images_directory_symlinks() -> Result<()> {
        let tmp = TempDir::new("sbin");
        let (state, lock) = setup(
            &tmp,
            &[("tool", &[("usr/sbin/tool", "t"), ("usr/sbin/sub/x", "x")])],
        )?;
        let root = image(&tmp, &["usr/bin/other"])?;
        std::os::unix::fs::symlink("bin", root.join("usr/sbin")).map_err(|source| {
            Error::Write {
                path: root.clone(),
                source,
            }
        })?;
        let manifest = Manifest::default();
        let guard = state.lock()?;
        let built = build(&state, &guard, &spec(&manifest, &lock, &root))?;
        let sysext = built.generation.sysext();
        assert!(sysext.join("usr/bin/tool").is_file());
        assert!(sysext.join("usr/bin/sub/x").is_file());
        assert!(!sysext.join("usr/sbin").exists());
        Ok(())
    }

    #[test]
    fn places_etc_var_and_usrmerge_paths() -> Result<()> {
        let tmp = TempDir::new("place");
        let (state, lock) = setup_meta(
            &tmp,
            &[(
                "svc",
                &[
                    ("lib/systemd/system/svc.service", "u"),
                    ("etc/svc.conf", "c"),
                    ("opt/svc/bin", "o"),
                    ("usr/bin/svc", "b"),
                ],
            )],
            &|_| nx_scriptlet::PackageMeta {
                files: vec![
                    pkg_file("/etc/svc.conf", 0o100_644, 17),
                    pkg_file("/var/lib/svc", 0o040_750, 0),
                    pkg_file("/var/lib/svc/state", 0o100_644, 64),
                    pkg_file("/opt/svc/bin", 0o100_755, 0),
                ],
                ..Default::default()
            },
        )?;
        let root = image(&tmp, &[])?;
        let manifest = Manifest::default();
        let guard = state.lock()?;
        let built = build(&state, &guard, &spec(&manifest, &lock, &root))?;
        let sysext = built.generation.sysext();
        assert!(sysext.join("usr/lib/systemd/system/svc.service").is_file());
        assert!(!sysext.join("lib").exists() && !sysext.join("opt").exists());
        assert_eq!(built.outside_usr, ["svc-1-1.fc45.x86_64"]);
        let tmpfiles = read(&sysext.join("usr/lib/tmpfiles.d/nx-svc.conf"))?;
        assert_eq!(
            tmpfiles,
            "# Written by nx from svc-1-1.fc45.x86_64; nx never runs its scriptlets.\n\
             C /etc/svc.conf 0644 svc svc -\n\
             d /var/lib/svc 0750 svc svc -\n"
        );
        Ok(())
    }

    #[test]
    fn refuses_scriptlets_it_cannot_translate_until_allowed() -> Result<()> {
        let tmp = TempDir::new("refuse");
        let meta = |_: &str| nx_scriptlet::PackageMeta {
            scripts: nx_scriptlet::Scripts {
                phases: vec![
                    (
                        nx_scriptlet::Phase::Post,
                        script("sed -i s/a/b/ /etc/svc.conf\n"),
                    ),
                    (
                        nx_scriptlet::Phase::Postun,
                        script("getent group svc >/dev/null || groupadd -r svc\n"),
                    ),
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let (state, lock) = setup_meta(&tmp, &[("svc", &[("usr/bin/svc", "b")])], &meta)?;
        let root = image(&tmp, &[])?;
        let mut manifest = Manifest::default();
        let guard = state.lock()?;
        let Err(Error::Scriptlets(refusals)) =
            build(&state, &guard, &spec(&manifest, &lock, &root))
        else {
            panic!("expected a refusal");
        };
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].leftovers[0].source, "%post");
        assert_eq!(state.generations()?, Vec::<u32>::new());

        // An entry for other text doesn't apply; the reviewed text does.
        manifest.scriptlets.push(crate::ScriptletAllow {
            package: "svc".into(),
            digest: "0".repeat(64),
        });
        assert!(build(&state, &guard, &spec(&manifest, &lock, &root)).is_err());
        manifest.scriptlets[0].digest = refusals[0].digest.clone();
        let built = build(&state, &guard, &spec(&manifest, &lock, &root))?;
        assert_eq!(built.allowed, ["svc"]);
        // What was understood is still carried out.
        let sysusers = read(
            &built
                .generation
                .sysext()
                .join("usr/lib/sysusers.d/nx-svc.conf"),
        )?;
        assert!(sysusers.ends_with("g svc -\n"), "{sysusers}");
        Ok(())
    }

    #[test]
    fn records_what_each_package_asks_of_systemd() -> Result<()> {
        let tmp = TempDir::new("activation");
        let meta = |_: &str| nx_scriptlet::PackageMeta {
            scripts: nx_scriptlet::Scripts {
                phases: vec![(
                    nx_scriptlet::Phase::Post,
                    script(
                        "if [ $1 -eq 1 ] && [ -x \"/usr/lib/systemd/systemd-update-helper\" ]; then\n\
                         /usr/lib/systemd/systemd-update-helper install-system-units svc.service || :\nfi\n",
                    ),
                )],
                ..Default::default()
            },
            ..Default::default()
        };
        let (state, lock) = setup_meta(&tmp, &[("svc", &[("usr/bin/svc", "b")])], &meta)?;
        let root = image(&tmp, &[])?;
        let manifest = Manifest::default();
        let guard = state.lock()?;
        let built = build(&state, &guard, &spec(&manifest, &lock, &root))?;
        let a = &built.generation.activation;
        let svc = a.package("svc");
        assert_eq!(
            svc.map(|p| p.intents.preset.clone()),
            Some(vec![nx_scriptlet::UnitRef {
                scope: nx_scriptlet::Scope::System,
                name: "svc.service".into()
            }])
        );
        // Read back from disk, as nxd will.
        assert_eq!(Generation::read(&state, 1)?.activation, *a);
        Ok(())
    }

    #[test]
    fn reads_the_newest_generation_it_understands() -> Result<()> {
        let tmp = TempDir::new("newer");
        let (state, lock) = setup(&tmp, &[])?;
        let root = image(&tmp, &[])?;
        let manifest = Manifest::default();
        let guard = state.lock()?;
        build(&state, &guard, &spec(&manifest, &lock, &root))?;
        build(&state, &guard, &spec(&manifest, &lock, &root))?;
        state.set_current(2)?;
        // A newer nx wrote generation 2.
        let path = state.generations_dir().join("2/lock.toml");
        let text = read(&path)?.replace("version = 1", "version = 99");
        write(&path, &text)?;
        assert!(matches!(
            Generation::current(&state),
            Err(Error::Newer { version: 99, .. })
        ));
        let snap = Generation::readable(&state)?;
        assert_eq!(snap.newer, Some(2));
        assert_eq!(snap.generation.map(|g| g.number), Some(1));
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
