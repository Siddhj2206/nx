//! What nxd checks before it merges a generation. The unprivileged CLI
//! wrote the tree, so root trusts nothing in it beyond what this walk
//! verifies: owners, modes, file types, and an extension-release that
//! matches the running system. The walk never follows a symlink.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use nx_core::generation::EXTENSION_RELEASE;
use nx_core::{Generation, OsRelease};

use crate::{Error, Layout, Result};

/// A generation that passed.
#[derive(Debug)]
pub struct Validated {
    /// The extension tree's real path (no symlink in it).
    pub sysext: PathBuf,
}

/// Checks generation `number` of `layout`'s state. Files may be owned by
/// root or one of `allowed` (nx's user, the caller); nothing may be
/// writable by others, setuid, setgid, carry file capabilities or be a
/// device, socket or FIFO.
pub fn validate(layout: &Layout, number: u32, allowed: &[u32]) -> Result<Validated> {
    let refuse = |reason: String| Error::Refused { number, reason };
    let root = layout
        .state
        .root()
        .join("generations")
        .join(number.to_string());
    let owned = |uid: u32| uid == 0 || allowed.contains(&uid);

    for dir in [layout.state.root().join("generations"), root.clone()] {
        let m = std::fs::symlink_metadata(&dir)
            .map_err(|e| refuse(format!("can't read {}: {e}", dir.display())))?;
        if !m.is_dir() || !owned(m.uid()) || m.mode() & 0o002 != 0 {
            return Err(refuse(format!(
                "{} must be a directory of root, nx or you, not writable by others",
                dir.display()
            )));
        }
    }
    Generation::read(&layout.state, number).map_err(|e| refuse(format!("can't read it: {e}")))?;
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
    walk(&sysext, &owned).map_err(refuse)?;
    check_release(layout, &sysext).map_err(refuse)?;
    Ok(Validated { sysext })
}

fn walk(dir: &Path, owned: &dyn Fn(u32) -> bool) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .path();
        let m = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let kind = m.file_type();
        if !owned(m.uid()) {
            return Err(format!(
                "{} is owned by uid {}, not root, nx or you",
                path.display(),
                m.uid()
            ));
        }
        if kind.is_symlink() {
            continue;
        }
        let sticky_dir = kind.is_dir() && m.mode() & 0o1000 != 0;
        if m.mode() & 0o002 != 0 && !sticky_dir {
            return Err(format!("{} is writable by everyone", path.display()));
        }
        if kind.is_dir() {
            walk(&path, owned)?;
        } else if kind.is_file() {
            if m.mode() & 0o6000 != 0 {
                return Err(format!("{} is setuid or setgid", path.display()));
            }
            if has_capabilities(&path) {
                return Err(format!("{} has file capabilities", path.display()));
            }
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

/// Whether `path` carries `security.capability`. Not following links.
fn has_capabilities(path: &Path) -> bool {
    let mut buf = [0u8; 64];
    rustix::fs::lgetxattr(path, "security.capability", &mut buf).is_ok()
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
