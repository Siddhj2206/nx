//! What nxd checks before it merges a generation, and the copy it merges.
//! The unprivileged CLI wrote the tree and still owns it, so root merges
//! a copy of its own instead (reflinked where the filesystem can): what
//! appears in `/usr` belongs to root, and a later write to the
//! generation, by its owner or anything running as them, changes nothing
//! merged. The copy is checked as it is made: owners, modes, file types,
//! and an extension-release that matches the running system. It never
//! follows a symlink.

use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nx_core::generation::EXTENSION_RELEASE;
use nx_core::{Generation, OsRelease};

use crate::{Error, Image, Layout, Result};

/// A generation that passed.
#[derive(Debug)]
pub struct Validated {
    /// Root's copy of the extension tree, under [`Layout::merged`]: what
    /// is merged.
    pub sysext: PathBuf,
    /// What its packages ask of systemd.
    pub activation: nx_core::Activation,
}

/// The uids of the members of group `wheel` under `root`: those it lists
/// and those whose primary group it is. They may all change nx's state, so
/// files any of them wrote are acceptable (design, *Privilege model*).
/// Accounts that exist only in NSS (SSSD, LDAP) aren't seen.
pub fn wheel_uids(root: &Path) -> Vec<u32> {
    let read =
        |name: &str| std::fs::read_to_string(root.join("etc").join(name)).unwrap_or_default();
    let group = read("group");
    let Some((gid, members)) = group.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next()? == "wheel").then_some(())?;
        f.next()?;
        Some((f.next()?.to_owned(), f.next().unwrap_or("").to_owned()))
    }) else {
        return Vec::new();
    };
    read("passwd")
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            let (name, uid, primary) = (*f.first()?, f.get(2)?, *f.get(3)?);
            (primary == gid || members.split(',').any(|m| m == name))
                .then(|| uid.parse().ok())
                .flatten()
        })
        .collect()
}

/// Checks generation `number` of `layout`'s state and copies its extension
/// tree to root's own under [`Layout::merged`]. Files may be owned by root
/// or by a uid `allowed` accepts (nx's user, the caller, wheel); nothing
/// may be writable by others, setuid, setgid, carry file capabilities or
/// be a device, socket or FIFO. The copy is root's, writable by no one
/// else; the image and release checks run on it, so what was checked is
/// what is merged.
pub fn validate(layout: &Layout, number: u32, allowed: &dyn Fn(u32) -> bool) -> Result<Validated> {
    let refuse = |reason: String| Error::Refused { number, reason };
    let root = layout.state.generation_dir(number);
    let owned = |uid: u32| uid == 0 || allowed(uid);

    let parent = root.parent().map(Path::to_path_buf).unwrap_or_default();
    for dir in [parent, root.clone()] {
        let m = std::fs::symlink_metadata(&dir)
            .map_err(|e| refuse(format!("can't read {}: {e}", dir.display())))?;
        if !m.is_dir() || !owned(m.uid()) || m.mode() & 0o002 != 0 {
            return Err(refuse(format!(
                "{} must be a directory of root, nx or you, not writable by others",
                dir.display()
            )));
        }
    }
    let generation = Generation::read(&layout.state, number)
        .map_err(|e| refuse(format!("can't read it: {e}")))?;
    let sysext = root.join("sysext");
    let m = std::fs::symlink_metadata(&sysext)
        .map_err(|e| refuse(format!("no extension tree: {e}")))?;
    if !m.is_dir() || !owned(m.uid()) || m.mode() & 0o002 != 0 {
        return Err(refuse(
            "its extension tree must be a directory of root, nx or you, not writable by others"
                .into(),
        ));
    }
    let sysext = std::fs::canonicalize(&sysext)
        .map_err(|e| refuse(format!("can't resolve {}: {e}", sysext.display())))?;
    let copy = new_copy(layout, number)?;
    let checked = copy_tree(&sysext, &copy, &owned).and_then(|()| {
        match &layout.image {
            Image::None => {}
            Image::Root(image) => {
                nx_core::generation::check_extension(&copy, image).map_err(|e| e.to_string())?;
            }
            Image::Unreadable(why) => return Err(format!("can't read the booted image: {why}")),
        }
        check_release(layout, &copy)
    });
    if let Err(why) = checked {
        let _ = nx_core::state::remove_tree(&copy);
        return Err(refuse(why));
    }
    Ok(Validated {
        sysext: copy,
        activation: generation.activation,
    })
}

/// A fresh directory for generation `number`'s copy, in root's own
/// directory: `<number>.<pid>`, beside the one merged now, which stays
/// until the merge has moved off it.
fn new_copy(layout: &Layout, number: u32) -> Result<PathBuf> {
    let io = |path: &Path, source| Error::Io {
        path: path.to_owned(),
        source,
    };
    let dir = &layout.merged;
    std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    let m = std::fs::symlink_metadata(dir).map_err(|e| io(dir, e))?;
    let me = rustix::process::geteuid().as_raw();
    if !m.is_dir() || m.uid() != me || m.mode() & 0o022 != 0 {
        return Err(io(
            dir,
            std::io::Error::other("must be a directory of nxd's own, writable by no one else"),
        ));
    }
    let copy = dir.join(format!("{number}.{}", std::process::id()));
    nx_core::state::remove_tree(&copy)?;
    std::fs::create_dir(&copy).map_err(|e| io(&copy, e))?;
    Ok(copy)
}

/// Copies `src` into the empty directory `dst`, refusing what root must
/// not merge. Files are opened without following links and checked on the
/// descriptor; their data is reflinked when the filesystem can, else
/// copied. Modes lose group and other write (and setgid on directories,
/// which `/var/lib/nx` passes down); owners and ACLs are not copied: all
/// of it is root's.
fn copy_tree(src: &Path, dst: &Path, owned: &dyn Fn(u32) -> bool) -> Result<(), String> {
    let entries = std::fs::read_dir(src).map_err(|e| format!("reading {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", src.display()))?;
        let (path, to) = (entry.path(), dst.join(entry.file_name()));
        let m = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let kind = m.file_type();
        check_owner(&path, &m, owned)?;
        let write = |e: std::io::Error| format!("copying {}: {e}", path.display());
        if kind.is_symlink() {
            let target = std::fs::read_link(&path).map_err(write)?;
            std::os::unix::fs::symlink(target, &to).map_err(write)?;
        } else if kind.is_dir() {
            if m.mode() & 0o002 != 0 && m.mode() & 0o1000 == 0 {
                return Err(format!("{} is writable by everyone", path.display()));
            }
            std::fs::create_dir(&to).map_err(write)?;
            copy_tree(&path, &to, owned)?;
            // Last: a read-only directory can't be filled.
            set_mode(&to, m.mode() & 0o755).map_err(write)?;
        } else if kind.is_file() {
            copy_file(&path, &to, owned)?;
        } else if kind.is_block_device()
            || kind.is_char_device()
            || kind.is_fifo()
            || kind.is_socket()
        {
            return Err(format!("{} is a special file", path.display()));
        }
    }
    Ok(())
}

/// One regular file: checked on the opened descriptor (a swap after the
/// directory listing can't slip anything past), then cloned.
fn copy_file(path: &Path, to: &Path, owned: &dyn Fn(u32) -> bool) -> Result<(), String> {
    let write = |e: std::io::Error| format!("copying {}: {e}", path.display());
    let mut from = std::fs::File::options()
        .read(true)
        .custom_flags(libc_flags())
        .open(path)
        .map_err(write)?;
    let m = from.metadata().map_err(write)?;
    if !m.is_file() {
        return Err(format!("{} is a special file", path.display()));
    }
    check_owner(path, &m, owned)?;
    if m.mode() & 0o002 != 0 {
        return Err(format!("{} is writable by everyone", path.display()));
    }
    if m.mode() & 0o6000 != 0 {
        return Err(format!("{} is setuid or setgid", path.display()));
    }
    let mut buf = [0u8; 64];
    if rustix::fs::fgetxattr(&from, "security.capability", &mut buf).is_ok() {
        return Err(format!("{} has file capabilities", path.display()));
    }
    let mut out = std::fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)
        .map_err(write)?;
    if rustix::fs::ioctl_ficlone(&out, &from).is_err() {
        // Not the same filesystem, or one without reflinks.
        std::io::copy(&mut from, &mut out).map_err(write)?;
    }
    out.set_permissions(std::fs::Permissions::from_mode(m.mode() & 0o755))
        .map_err(write)
}

/// `O_NOFOLLOW` (a link is refused, not followed) and `O_NONBLOCK` (a
/// FIFO swapped in doesn't hang the open).
fn libc_flags() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}

fn check_owner(
    path: &Path,
    m: &std::fs::Metadata,
    owned: &dyn Fn(u32) -> bool,
) -> Result<(), String> {
    if owned(m.uid()) {
        Ok(())
    } else {
        Err(format!(
            "{} is owned by uid {}, not root, nx or you",
            path.display(),
            m.uid()
        ))
    }
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Removes root's copies under [`Layout::merged`] except `keep` (the one
/// merged now). Best effort: a copy left over is only space.
pub(crate) fn prune(layout: &Layout, keep: Option<&Path>) {
    let Ok(entries) = std::fs::read_dir(&layout.merged) else {
        return;
    };
    for e in entries.flatten() {
        if Some(e.path().as_path()) != keep {
            let _ = nx_core::state::remove_tree(&e.path());
        }
    }
}

/// The extension must declare the system it extends, or systemd-sysext
/// would refuse the merge half-way through.
fn check_release(layout: &Layout, sysext: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(sysext.join(EXTENSION_RELEASE))
        .map_err(|e| format!("no extension-release.nx: {e}"))?;
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
    };
    let system = OsRelease::read(&layout.root).map_err(|e| format!("reading os-release: {e}"))?;
    if value("ID") != Some(system.id.as_str())
        || value("VERSION_ID") != Some(system.version_id.as_str())
    {
        return Err(format!(
            "it extends {} {}, this system is {} {}",
            value("ID").unwrap_or("?"),
            value("VERSION_ID").unwrap_or("?"),
            system.id,
            system.version_id
        ));
    }
    Ok(())
}
