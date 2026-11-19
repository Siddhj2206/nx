//! RPM repository definitions, read from the image's `.repo` files.
//!
//! dnf5 ships the Fedora repos in `/usr/share/dnf5/repos.d` and reads
//! `/etc/yum.repos.d` after it; a file in `/etc` with the same name replaces
//! the one in `/usr`. nx reads both, read-only: these are the repos the image
//! provides. Repos nx itself adds (`nx repo add`) live in the manifest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{Error, Result};

/// Where dnf5 looks for `.repo` files, lowest precedence first.
pub const REPO_DIRS: [&str; 2] = ["usr/share/dnf5/repos.d", "etc/yum.repos.d"];

/// dnf's default priority; lower wins.
pub const DEFAULT_PRIORITY: i32 = 99;

/// One `[section]` of a `.repo` file, with `$releasever` and `$basearch`
/// already substituted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub id: String,
    pub name: String,
    pub baseurl: Vec<String>,
    pub metalink: Option<String>,
    pub mirrorlist: Option<String>,
    pub enabled: bool,
    pub gpgcheck: bool,
    pub gpgkey: Vec<String>,
    pub priority: i32,
    pub cost: i32,
    /// How long downloaded metadata stays fresh; `None` is never stale.
    pub metadata_expire: Option<Duration>,
    /// The file it came from.
    pub file: PathBuf,
}

/// The values `$releasever` and `$basearch` expand to.
#[derive(Debug, Clone)]
pub struct Vars {
    pub releasever: String,
    pub basearch: String,
}

impl Vars {
    fn expand(&self, s: &str) -> String {
        s.replace("${releasever}", &self.releasever)
            .replace("$releasever", &self.releasever)
            .replace("${basearch}", &self.basearch)
            .replace("$basearch", &self.basearch)
    }
}

/// Every repo defined under `root`, sorted by id. Disabled repos are
/// included; callers filter on [`Repo::enabled`].
pub fn load(root: &Path, vars: &Vars) -> Result<Vec<Repo>> {
    // File name → path; later directories replace earlier ones.
    let mut files = BTreeMap::new();
    for dir in REPO_DIRS {
        let dir = root.join(dir);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::Read { path: dir, source }),
        };
        for entry in entries {
            let path = entry
                .map_err(|source| Error::Read {
                    path: dir.clone(),
                    source,
                })?
                .path();
            if path.extension().is_some_and(|e| e == "repo")
                && let Some(name) = path.file_name()
            {
                files.insert(name.to_owned(), path);
            }
        }
    }
    let mut repos = BTreeMap::new();
    for path in files.into_values() {
        let text = std::fs::read_to_string(&path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        for repo in parse(&text, &path, vars) {
            repos.insert(repo.id.clone(), repo);
        }
    }
    Ok(repos.into_values().collect())
}

/// Parses one `.repo` file. Unknown keys are ignored; a line indented under
/// a key continues its value (multi-line `gpgkey`).
pub fn parse(text: &str, file: &Path, vars: &Vars) -> Vec<Repo> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some(id) = trimmed.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((id.trim().to_owned(), Vec::new()));
            continue;
        }
        let Some((_, keys)) = sections.last_mut() else {
            continue;
        };
        if line.starts_with(char::is_whitespace) {
            if let Some((_, value)) = keys.last_mut() {
                value.push(' ');
                value.push_str(trimmed);
            }
        } else if let Some((key, value)) = trimmed.split_once('=') {
            keys.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }

    sections
        .into_iter()
        .map(|(id, keys)| {
            let get = |k: &str| {
                keys.iter()
                    .rev()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| vars.expand(v))
            };
            let list = |k: &str| {
                get(k)
                    .map(|v| {
                        v.split([',', ' ', '\t'])
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let int =
                |k: &str, default: i32| get(k).and_then(|v| v.parse().ok()).unwrap_or(default);
            Repo {
                name: get("name").unwrap_or_else(|| id.clone()),
                baseurl: list("baseurl"),
                metalink: get("metalink"),
                mirrorlist: get("mirrorlist"),
                enabled: get("enabled").is_none_or(|v| truthy(&v)),
                gpgcheck: get("gpgcheck").is_some_and(|v| truthy(&v)),
                gpgkey: list("gpgkey"),
                priority: int("priority", DEFAULT_PRIORITY),
                cost: int("cost", 1000),
                metadata_expire: get("metadata_expire")
                    .map_or(Some(DEFAULT_EXPIRE), |v| parse_expire(&v)),
                file: file.to_owned(),
                id,
            }
        })
        .collect()
}

/// dnf's default `metadata_expire`: 48 hours.
pub const DEFAULT_EXPIRE: Duration = Duration::from_secs(48 * 3600);

/// `7d`, `6h`, `90m`, `1800` (seconds); `never` or `-1` for never.
fn parse_expire(v: &str) -> Option<Duration> {
    let v = v.trim();
    if v == "never" || v == "-1" {
        return None;
    }
    let (num, unit) = match v.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&v[..i], c),
        _ => (v, 's'),
    };
    let secs = match unit {
        'd' => 86_400,
        'h' => 3_600,
        'm' => 60,
        _ => 1,
    };
    Some(num.parse::<u64>().map_or(DEFAULT_EXPIRE, |n| {
        Duration::from_secs(n.saturating_mul(secs))
    }))
}

fn truthy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "yes" | "true" | "on")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars {
            releasever: "45".into(),
            basearch: "x86_64".into(),
        }
    }

    #[test]
    fn parses_fedora_repo() {
        let text = include_str!("../tests/fixtures/fedora.repo");
        let repos = parse(text, Path::new("fedora.repo"), &vars());
        let fedora = repos.iter().find(|r| r.id == "fedora");
        assert_eq!(
            fedora.and_then(|r| r.metalink.as_deref()),
            Some("https://mirrors.fedoraproject.org/metalink?repo=fedora-45&arch=x86_64")
        );
        assert!(fedora.is_some_and(|r| r.enabled && r.gpgcheck && r.baseurl.is_empty()));
        assert_eq!(
            fedora.map(|r| r.gpgkey.clone()),
            Some(vec![
                "file:///usr/share/pki/rpm-gpg/RPM-GPG-KEY-fedora-45-x86_64".to_owned()
            ])
        );
        assert!(
            repos
                .iter()
                .any(|r| r.id == "fedora-debuginfo" && !r.enabled)
        );
    }

    #[test]
    fn metadata_expire_units() {
        assert_eq!(parse_expire("7d"), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_expire("6h"), Some(Duration::from_secs(6 * 3_600)));
        assert_eq!(parse_expire("1800"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_expire("never"), None);
    }

    #[test]
    fn continuation_lines_extend_a_value() {
        let text = "[x]\nbaseurl=https://a/\ngpgkey=https://k1\n  https://k2\npriority=50\n";
        let repos = parse(text, Path::new("x.repo"), &vars());
        assert_eq!(repos[0].gpgkey, vec!["https://k1", "https://k2"]);
        assert_eq!(repos[0].priority, 50);
        assert!(repos[0].enabled, "enabled defaults to true");
    }

    #[test]
    fn etc_replaces_usr_by_file_name() -> Result<()> {
        let root = std::env::temp_dir().join(format!("nx-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, enabled) in [(REPO_DIRS[0], 1), (REPO_DIRS[1], 0)] {
            let dir = root.join(dir);
            std::fs::create_dir_all(&dir).map_err(|source| Error::Read {
                path: dir.clone(),
                source,
            })?;
            let path = dir.join("fedora.repo");
            std::fs::write(&path, format!("[fedora]\nenabled={enabled}\n"))
                .map_err(|source| Error::Read { path, source })?;
        }
        let repos = load(&root, &vars());
        let _ = std::fs::remove_dir_all(&root);
        let repos = repos?;
        assert_eq!(repos.len(), 1);
        assert!(!repos[0].enabled);
        Ok(())
    }
}
