//! A package's files outside `/usr`. An extension can only carry `/usr`,
//! so `/etc` and `/var` content travels as a factory copy under
//! `/usr/share/factory` and `tmpfiles.d` lines that put it in place when it
//! is missing, as systemd's own images do (design, *Scriptlet translation*).

use serde::{Deserialize, Serialize};

use crate::Scripts;

/// rpm's `%ghost` flag: the package owns the path but ships no content.
pub const RPMFILE_GHOST: u32 = 1 << 6;

/// One entry of a package's file list, from the signed header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageFile {
    /// Absolute path.
    pub path: String,
    /// File type and permission bits (`st_mode`).
    pub mode: u32,
    pub user: String,
    pub group: String,
    /// rpm's `%config`, `%ghost` … flags.
    pub flags: u32,
    /// The target of a symlink, else empty.
    pub link: String,
}

/// What the signed header says about a package beyond its `/usr` payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageMeta {
    pub scripts: Scripts,
    /// Every entry whose path is not under `/usr`.
    pub files: Vec<PackageFile>,
}

/// The `tmpfiles.d` text for a package's `/etc`, `/var` and `/run` entries,
/// and the entries nx can't express.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tmpfiles {
    pub lines: Vec<String>,
    /// Entries that are not under `/etc`, `/var` or `/run`, or whose names
    /// tmpfiles.d can't carry. They are left out, and reported.
    pub unplaced: Vec<String>,
}

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFREG: u32 = 0o100_000;

/// Where each kind of path lands: `/etc` and `/var` content is copied from
/// `/usr/share/factory` when absent; `/run` only has directories made.
pub fn tmpfiles(files: &[PackageFile]) -> Tmpfiles {
    let mut out = Tmpfiles::default();
    for f in files {
        let top = f.path.trim_start_matches('/').split('/').next();
        let placed = matches!(top, Some("etc" | "var" | "run"));
        let plain = f
            .path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+@:,=-/".contains(c));
        if !placed || !plain || f.path.contains("//") || f.path.split('/').any(|p| p == "..") {
            out.unplaced.push(f.path.clone());
            continue;
        }
        if f.flags & RPMFILE_GHOST != 0 {
            continue;
        }
        let perm = f.mode & 0o7777;
        let owner = format!("{:04o} {} {}", perm, id(&f.user), id(&f.group));
        match f.mode & S_IFMT {
            S_IFDIR => out.lines.push(format!("d {} {owner} -", f.path)),
            // Only directories exist under /run: it is empty at boot.
            _ if top == Some("run") => out.unplaced.push(f.path.clone()),
            S_IFLNK if !f.link.is_empty() && !f.link.contains(char::is_whitespace) => {
                out.lines.push(format!("L {} - - - - {}", f.path, f.link));
            }
            S_IFREG => out.lines.push(format!("C {} {owner} -", f.path)),
            _ => out.unplaced.push(f.path.clone()),
        }
    }
    out
}

fn id(name: &str) -> &str {
    if name.is_empty() { "root" } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, mode: u32, flags: u32, link: &str) -> PackageFile {
        PackageFile {
            path: path.into(),
            mode,
            user: "chrony".into(),
            group: "root".into(),
            flags,
            link: link.into(),
        }
    }

    #[test]
    fn places_etc_and_var_and_skips_ghosts() {
        let t = tmpfiles(&[
            file("/var/lib/chrony", S_IFDIR | 0o750, 0, ""),
            file("/etc/chrony.conf", S_IFREG | 0o644, 17, ""),
            file(
                "/etc/localtime",
                S_IFLNK | 0o777,
                0,
                "../usr/share/zoneinfo/UTC",
            ),
            file("/var/lib/chrony/drift", S_IFREG | 0o644, RPMFILE_GHOST, ""),
            file("/run/chrony", S_IFDIR | 0o755, 0, ""),
        ]);
        assert_eq!(
            t.lines,
            [
                "d /var/lib/chrony 0750 chrony root -",
                "C /etc/chrony.conf 0644 chrony root -",
                "L /etc/localtime - - - - ../usr/share/zoneinfo/UTC",
                "d /run/chrony 0755 chrony root -",
            ]
        );
        assert_eq!(t.unplaced, Vec::<String>::new());
    }

    #[test]
    fn reports_what_it_cannot_place() {
        let t = tmpfiles(&[
            file("/opt/foo/bin", S_IFREG | 0o755, 0, ""),
            file("/etc/with space", S_IFREG | 0o644, 0, ""),
            file("/etc/50%.conf", S_IFREG | 0o644, 0, ""),
            file("/run/x", S_IFREG | 0o644, 0, ""),
        ]);
        assert_eq!(t.lines, Vec::<String>::new());
        assert_eq!(t.unplaced.len(), 4);
    }
}
