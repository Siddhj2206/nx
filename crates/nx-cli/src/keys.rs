//! Repo keys for the host tier. The keys an added repo's packages verify
//! against are pinned by content: `keys/<sha256>.asc` in the state
//! directory, named in each generation's `lock.toml` (`key`). A key is
//! never replaced or removed while a generation names it, so a rollback
//! verifies with the key it was built with; `nx gc` removes the ones no
//! kept generation names. Design, *Repositories and COPR*: a changed key
//! blocks the repo until the user confirms it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nx_core::{Generation, LockedRepo, Manifest, State};
use nx_rpm::{Repo, RepoKeys};

use crate::host::{self, Machine};

/// Where pinned keys are.
pub fn dir(state: &State) -> PathBuf {
    host::keys_dir(state)
}

/// Pins `text` (an armored key) in `dir`; returns its SHA-256, the name a
/// lockfile records. Written beside its name and renamed, never over an
/// existing pin: the same text has the same name.
pub fn pin(dir: &Path, text: &str) -> Result<String> {
    let sha = nx_rpm::fetch::sha256_hex(text.as_bytes());
    let path = dir.join(format!("{sha}.asc"));
    if path.is_file() {
        return Ok(sha);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{sha}.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .with_context(|| format!("pinning {}", path.display()))?;
    Ok(sha)
}

/// The pinned key named `sha`, checked: the directory is shared, and a
/// file that no longer hashes to its name is refused.
fn read(dir: &Path, sha: &str) -> nx_rpm::Result<String> {
    let path = dir.join(format!("{sha}.asc"));
    let text = std::fs::read_to_string(&path).map_err(|source| nx_rpm::Error::Read {
        path: path.clone(),
        source,
    })?;
    if nx_rpm::fetch::sha256_hex(text.as_bytes()) != sha {
        return Err(nx_rpm::Error::Invalid(format!(
            "{} no longer holds the key it was pinned as",
            path.display()
        )));
    }
    Ok(text)
}

/// Pinned keys in `dir` that `keep` doesn't name. Only content-named
/// files: anything else is not nx's to remove.
pub fn unused(dir: &Path, keep: &HashSet<String>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let sha = name.strip_suffix(".asc")?;
            let ours = sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit());
            (ours && !keep.contains(sha)).then(|| e.path())
        })
        .collect()
}

/// Repo keys, read as packages need them.
pub struct Keys {
    root: PathBuf,
    dir: PathBuf,
    loaded: HashMap<String, RepoKeys>,
    /// Added repo ids → names.
    added: HashMap<String, String>,
    /// Added repo id → what the current generation recorded for it.
    pinned: HashMap<String, LockedRepo>,
    /// Keys this command fetched and pinned (`nx repo add`, `nx apply`),
    /// by repo id: the user was shown their fingerprints.
    chosen: HashMap<String, String>,
    /// Repos whose new key the user confirmed (`nx repo add --new-key`).
    trusted: Vec<String>,
    /// The pin each added repo's packages were checked with, for the lock.
    used: HashMap<String, String>,
    generation: u32,
}

impl Keys {
    pub fn new(
        machine: &Machine,
        state: &State,
        current: Option<&Generation>,
        manifest: &Manifest,
    ) -> Self {
        let added: HashMap<String, String> = manifest
            .repos
            .iter()
            .map(|r| (nx_rpm::repo::id_of(&r.name), r.name.clone()))
            .collect();
        let pinned = current
            .map(|g| {
                g.lock
                    .repos
                    .iter()
                    .filter(|r| added.contains_key(&r.id))
                    .map(|r| (r.id.clone(), r.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            root: machine.root.clone(),
            dir: dir(state),
            loaded: HashMap::new(),
            pinned,
            added,
            chosen: HashMap::new(),
            trusted: Vec::new(),
            used: HashMap::new(),
            generation: current.map_or(0, |g| g.number),
        }
    }

    pub fn is_added(&self, id: &str) -> bool {
        self.added.contains_key(id)
    }

    /// Uses the key pinned as `sha` for repo `id` from now on.
    pub fn choose(&mut self, id: &str, sha: String) {
        self.loaded.remove(id);
        self.chosen.insert(id.to_owned(), sha);
    }

    /// Accepts the keys chosen for `ids`, though they differ from what the
    /// current generation recorded: the user confirmed them.
    pub fn trust(&mut self, ids: &[String]) {
        self.trusted.extend(ids.iter().cloned());
    }

    /// The pin an added repo's packages were checked with.
    pub fn pin_of(&self, id: &str) -> Option<String> {
        self.used.get(id).cloned()
    }

    pub fn get(&mut self, repo: &Repo) -> nx_rpm::Result<&RepoKeys> {
        if !self.loaded.contains_key(&repo.id) {
            let pinned = match self.added.contains_key(&repo.id) {
                true => self.pinned_text(&repo.id)?,
                false => None,
            };
            let keys = nx_rpm::keyring(repo, &self.root, pinned.as_deref())?;
            if let Some((name, was)) = self
                .pinned
                .get(&repo.id)
                .filter(|r| !r.keys.is_empty())
                .map(|r| (&self.added[&repo.id], &r.keys))
                && *was != keys.fingerprints
                && !self.trusted.contains(&repo.id)
            {
                return Err(nx_rpm::Error::Invalid(format!(
                    "the key pinned for {name} changed since generation {} (was {}, now {}); \
                     if you trust the new key: nx repo add {name} --new-key",
                    self.generation,
                    was.join(", "),
                    keys.fingerprints.join(", ")
                )));
            }
            self.loaded.insert(repo.id.clone(), keys);
        }
        self.loaded
            .get(&repo.id)
            .ok_or_else(|| nx_rpm::Error::Invalid(format!("no keys for {}", repo.id)))
    }

    /// An added repo's pinned key: the one this command chose, else the
    /// one the current generation names, else (a lock from before keys
    /// were kept by content) the file nx 1.0's first builds pinned under
    /// the repo's name, which is pinned by content from then on. `None`
    /// when there is none.
    fn pinned_text(&mut self, id: &str) -> nx_rpm::Result<Option<String>> {
        let sha = self
            .chosen
            .get(id)
            .or_else(|| self.pinned.get(id).and_then(|r| r.key.as_ref()))
            .cloned();
        let (sha, text) = match sha {
            Some(sha) => {
                let text = read(&self.dir, &sha)?;
                (sha, text)
            }
            None => {
                let legacy = self.dir.join(nx_rpm::packages::legacy_pinned_key(id));
                let Ok(text) = std::fs::read_to_string(&legacy) else {
                    return Ok(None);
                };
                let sha =
                    pin(&self.dir, &text).map_err(|e| nx_rpm::Error::Invalid(format!("{e:#}")))?;
                (sha, text)
            }
        };
        self.used.insert(id.to_owned(), sha);
        Ok(Some(text))
    }

    /// What to suggest when an added repo's package fails to verify.
    pub fn change_hint(&self, id: &str) -> String {
        self.added.get(id).map_or_else(String::new, |name| {
            format!(
                " (if {name} changed its signing key, check the new one and run nx repo add {name} --new-key)"
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nx_rpm::repo::Added;

    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/rpm")
            .join(name)
    }

    fn copr() -> Result<Repo> {
        let vars = nx_rpm::Vars {
            releasever: "45".into(),
            basearch: "x86_64".into(),
        };
        Ok(Repo::added(
            &Added {
                name: "copr:lilay/topgrade",
                baseurl: None,
                metalink: None,
                gpgkey: None,
                priority: None,
            },
            &vars,
        )?)
    }

    fn keys(dir: &Path, pinned: Option<LockedRepo>, id: &str) -> Keys {
        Keys {
            root: PathBuf::from("/"),
            dir: dir.to_owned(),
            loaded: HashMap::new(),
            pinned: pinned
                .map(|r| HashMap::from([(id.to_owned(), r)]))
                .unwrap_or_default(),
            added: HashMap::from([(id.to_owned(), "copr:lilay/topgrade".to_owned())]),
            chosen: HashMap::new(),
            trusted: Vec::new(),
            used: HashMap::new(),
            generation: 3,
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nx-cli-keys-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_changed_key_blocks_the_repo_until_trusted() -> Result<()> {
        let dir = tmp("changed");
        let repo = copr()?;
        let text = std::fs::read_to_string(testdata("copr-lilay-topgrade.gpg"))?;
        let sha = pin(&dir, &text)?;
        let mut k = keys(
            &dir,
            Some(LockedRepo {
                id: repo.id.clone(),
                keys: vec!["0000000000000000000000000000000000000000".into()],
                key: Some(sha.clone()),
                ..LockedRepo::default()
            }),
            &repo.id,
        );
        let err = k
            .get(&repo)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.starts_with("the key pinned for copr:lilay/topgrade changed since generation 3"),
            "{err}"
        );
        k.trust(std::slice::from_ref(&repo.id));
        assert_eq!(k.get(&repo)?.fingerprints.len(), 1);
        assert_eq!(k.pin_of(&repo.id), Some(sha));
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// The key a generation names stays readable after another is chosen,
    /// a pin that was edited is refused, and an old name-keyed pin moves
    /// to its content name.
    #[test]
    fn pins_are_kept_by_content() -> Result<()> {
        let dir = tmp("content");
        let repo = copr()?;
        let text = std::fs::read_to_string(testdata("copr-lilay-topgrade.gpg"))?;
        let sha = pin(&dir, &text)?;
        assert_eq!(pin(&dir, &text)?, sha, "the same text, the same pin");
        let other = pin(&dir, "not a key")?;
        assert!(dir.join(format!("{sha}.asc")).is_file());
        assert_eq!(
            unused(&dir, &HashSet::from([sha.clone()])),
            [dir.join(format!("{other}.asc"))]
        );

        std::fs::write(dir.join(format!("{sha}.asc")), "edited")?;
        let mut k = keys(
            &dir,
            Some(LockedRepo {
                id: repo.id.clone(),
                key: Some(sha.clone()),
                ..LockedRepo::default()
            }),
            &repo.id,
        );
        let err = k
            .get(&repo)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("no longer holds the key"), "{err}");

        std::fs::remove_file(dir.join(format!("{sha}.asc")))?;
        std::fs::write(
            dir.join(nx_rpm::packages::legacy_pinned_key(&repo.id)),
            &text,
        )?;
        let mut k = keys(&dir, None, &repo.id);
        assert_eq!(k.get(&repo)?.fingerprints.len(), 1);
        assert_eq!(k.pin_of(&repo.id), Some(sha.clone()));
        assert!(dir.join(format!("{sha}.asc")).is_file());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
