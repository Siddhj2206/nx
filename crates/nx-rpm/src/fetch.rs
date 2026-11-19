//! Downloads a repo's metadata into a local cache and keeps it fresh.
//!
//! The cache holds, per repo, `repomd.xml` and the files it names, each
//! verified against its checksum. Within the repo's `metadata_expire` nothing
//! touches the network, which is what keeps a warm `nx add --dry-run` under
//! a second.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::repo::Repo;
use crate::repomd::{Checksum, DataFile, Metalink, Repomd, parse_metalink, parse_repomd};
use crate::{Error, Result};

/// Something that can GET a URL. [`Ureq`] is the real one; tests use a map.
pub trait Http {
    fn get(&self, url: &str) -> Result<Vec<u8>>;
}

/// HTTP over ureq.
pub struct Ureq(ureq::Agent);

impl Ureq {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("nx/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_global(Some(std::time::Duration::from_secs(300)))
            // The system trust store, so a CA the image or the user adds
            // applies to nx too.
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        Self(config.into())
    }
}

impl Default for Ureq {
    fn default() -> Self {
        Self::new()
    }
}

impl Http for Ureq {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        let fail = |e: ureq::Error| Error::Fetch {
            url: url.to_owned(),
            reason: e.to_string(),
        };
        self.0
            .get(url)
            .call()
            .map_err(fail)?
            .body_mut()
            .with_config()
            .limit(1 << 30)
            .read_to_vec()
            .map_err(fail)
    }
}

/// A repo's metadata on disk, verified.
#[derive(Debug, Clone)]
pub struct Synced {
    pub repomd: Repomd,
    /// The repo's `primary` metadata (compressed, as downloaded).
    pub primary: PathBuf,
    /// Base URL the metadata came from, for downloading packages later.
    pub base: String,
    /// Whether this call went to the network.
    pub fetched: bool,
}

impl Synced {
    /// `repomd.xml`'s `primary` checksum: what a `.solv` cache is keyed by.
    pub fn primary_checksum(&self) -> &str {
        self.repomd.get("primary").map_or("", |p| &p.checksum.hex)
    }
}

/// When to go to the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Only when the cache is older than the repo's `metadata_expire`.
    IfStale,
    /// Always check for new metadata.
    Always,
    /// Never; fail if nothing is cached.
    Never,
}

/// Makes sure `repo`'s metadata is in `cache/<id>/` and returns it.
/// `on_fetch` runs once, just before the first request, so a caller can say
/// it is downloading.
pub fn sync(
    repo: &Repo,
    http: &dyn Http,
    cache: &Path,
    refresh: Refresh,
    on_fetch: &mut dyn FnMut(),
) -> Result<Synced> {
    let dir = cache.join(&repo.id);
    let cached = read_cached(&dir);
    if let Some(synced) = &cached {
        let fresh = match refresh {
            Refresh::Never => true,
            Refresh::Always => false,
            Refresh::IfStale => repo
                .metadata_expire
                .is_none_or(|expire| age(&dir.join("repomd.xml")).is_some_and(|age| age < expire)),
        };
        if fresh {
            return Ok(synced.clone());
        }
    } else if refresh == Refresh::Never {
        return Err(Error::Invalid(format!(
            "no cached metadata for {}",
            repo.id
        )));
    }

    on_fetch();
    let (bases, accepted) = mirrors(repo, http)?;
    let mut last_err = None;
    for base in &bases {
        match fetch_from(base, &accepted, http, &dir, cached.as_ref()) {
            Ok(synced) => return Ok(synced),
            Err(e) => last_err = Some(e),
        }
    }
    // Stale metadata beats none: nx can still resolve, and says so.
    if let Some(synced) = cached {
        return Ok(synced);
    }
    Err(last_err.unwrap_or_else(|| Error::Invalid(format!("{} has no mirrors", repo.id))))
}

/// Base URLs to try, and the `repomd.xml` checksums a metalink vouches for
/// (empty for plain `baseurl` repos).
fn mirrors(repo: &Repo, http: &dyn Http) -> Result<(Vec<String>, Vec<Checksum>)> {
    if let Some(url) = &repo.metalink {
        let body = http.get(url)?;
        let metalink: Metalink = parse_metalink(&String::from_utf8_lossy(&body))?;
        let bases = metalink.bases().map(str::to_owned).collect();
        return Ok((bases, metalink.repomd_checksums));
    }
    let bases = repo
        .baseurl
        .iter()
        .map(|b| {
            if b.ends_with('/') {
                b.clone()
            } else {
                format!("{b}/")
            }
        })
        .collect::<Vec<_>>();
    if bases.is_empty() {
        return Err(Error::Invalid(format!(
            "{} has neither metalink nor baseurl",
            repo.id
        )));
    }
    Ok((bases, Vec::new()))
}

fn fetch_from(
    base: &str,
    accepted: &[Checksum],
    http: &dyn Http,
    dir: &Path,
    cached: Option<&Synced>,
) -> Result<Synced> {
    let repomd_bytes = http.get(&format!("{base}repodata/repomd.xml"))?;
    if !accepted.is_empty() {
        let hex = sha256_hex(&repomd_bytes);
        if !accepted.iter().any(|c| c.kind == "sha256" && c.hex == hex) {
            return Err(Error::Invalid(format!(
                "{base}repodata/repomd.xml does not match the metalink (mirror out of date)"
            )));
        }
    }
    let repomd = parse_repomd(&String::from_utf8_lossy(&repomd_bytes))?;
    let primary = repomd
        .get("primary")
        .ok_or_else(|| Error::Invalid(format!("{base}repodata/repomd.xml has no primary")))?;

    create_dir(dir)?;
    let primary_path = dir.join(file_name(primary));
    let unchanged = cached.is_some_and(|c| c.primary == primary_path) && primary_path.exists();
    if !unchanged {
        let body = http.get(&format!("{base}{}", primary.href))?;
        verify(primary, &body)?;
        write_atomic(&primary_path, &body)?;
    }
    write_atomic(&dir.join("repomd.xml"), &repomd_bytes)?;
    write_atomic(&dir.join("base"), base.as_bytes())?;
    remove_others(dir, &primary_path);
    Ok(Synced {
        repomd,
        primary: primary_path,
        base: base.to_owned(),
        fetched: true,
    })
}

/// What's in `dir`, if it's complete.
fn read_cached(dir: &Path) -> Option<Synced> {
    let repomd = std::fs::read_to_string(dir.join("repomd.xml")).ok()?;
    let repomd = parse_repomd(&repomd).ok()?;
    let primary = dir.join(file_name(repomd.get("primary")?));
    if !primary.exists() {
        return None;
    }
    let base = std::fs::read_to_string(dir.join("base")).ok()?;
    Some(Synced {
        repomd,
        primary,
        base,
        fetched: false,
    })
}

fn file_name(d: &DataFile) -> String {
    d.href.rsplit('/').next().unwrap_or(&d.href).to_owned()
}

fn verify(d: &DataFile, body: &[u8]) -> Result<()> {
    if d.checksum.kind != "sha256" {
        return Err(Error::Invalid(format!(
            "{}: unsupported checksum {}",
            d.href, d.checksum.kind
        )));
    }
    let hex = sha256_hex(body);
    if hex != d.checksum.hex {
        return Err(Error::Invalid(format!(
            "{}: checksum mismatch (got {hex})",
            d.href
        )));
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn age(path: &Path) -> Option<std::time::Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| Error::Write {
        path: dir.to_owned(),
        source,
    })
}

/// Writes through a temporary file and a rename, so a reader never sees a
/// half-written file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })
}

/// Drops metadata files from earlier syncs.
fn remove_others(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path != keep && name.contains("-primary.xml") {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{Vars, parse};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Serves canned bodies and counts requests.
    #[derive(Default)]
    struct FakeHttp {
        bodies: HashMap<String, Vec<u8>>,
        hits: RefCell<Vec<String>>,
    }

    impl Http for FakeHttp {
        fn get(&self, url: &str) -> Result<Vec<u8>> {
            self.hits.borrow_mut().push(url.to_owned());
            self.bodies.get(url).cloned().ok_or_else(|| Error::Fetch {
                url: url.to_owned(),
                reason: "404".into(),
            })
        }
    }

    const PRIMARY: &[u8] = b"<metadata packages=\"0\"/>";

    fn fake_repo(mirror_ok: bool) -> (Repo, FakeHttp) {
        let primary_sum = sha256_hex(PRIMARY);
        let repomd = format!(
            r#"<repomd><revision>1</revision><data type="primary"><checksum type="sha256">{primary_sum}</checksum><location href="repodata/{primary_sum}-primary.xml"/></data></repomd>"#
        );
        let repomd_sum = sha256_hex(repomd.as_bytes());
        let metalink = format!(
            r#"<metalink><files><file name="repomd.xml"><verification><hash type="sha256">{repomd_sum}</hash></verification><resources><url protocol="https">https://bad/repodata/repomd.xml</url><url protocol="https">https://good/repodata/repomd.xml</url></resources></file></files></metalink>"#
        );
        let mut http = FakeHttp::default();
        http.bodies
            .insert("https://meta".into(), metalink.into_bytes());
        // The first mirror is out of date: its repomd doesn't match.
        http.bodies.insert(
            "https://bad/repodata/repomd.xml".into(),
            b"<repomd/>".to_vec(),
        );
        if mirror_ok {
            http.bodies.insert(
                "https://good/repodata/repomd.xml".into(),
                repomd.into_bytes(),
            );
            http.bodies.insert(
                format!("https://good/repodata/{primary_sum}-primary.xml"),
                PRIMARY.to_vec(),
            );
        }
        let vars = Vars {
            releasever: "45".into(),
            basearch: "x86_64".into(),
        };
        let repo = parse(
            "[t]\nmetalink=https://meta\nmetadata_expire=1h\n",
            Path::new("t.repo"),
            &vars,
        )
        .remove(0);
        (repo, http)
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nx-fetch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn skips_a_stale_mirror_then_serves_from_cache() -> Result<()> {
        let cache = temp("mirror");
        let (repo, http) = fake_repo(true);
        let first = sync(&repo, &http, &cache, Refresh::IfStale, &mut || {})?;
        assert!(first.fetched);
        assert_eq!(first.base, "https://good/");
        assert_eq!(std::fs::read(&first.primary).ok().as_deref(), Some(PRIMARY));

        http.hits.borrow_mut().clear();
        let second = sync(&repo, &http, &cache, Refresh::IfStale, &mut || {})?;
        assert!(!second.fetched);
        assert!(http.hits.borrow().is_empty(), "fresh cache must not fetch");
        let _ = std::fs::remove_dir_all(&cache);
        Ok(())
    }

    #[test]
    fn keeps_stale_cache_when_every_mirror_fails() -> Result<()> {
        let cache = temp("offline");
        let (repo, http) = fake_repo(true);
        sync(&repo, &http, &cache, Refresh::IfStale, &mut || {})?;
        let (_, broken) = fake_repo(false);
        let again = sync(&repo, &broken, &cache, Refresh::Always, &mut || {})?;
        assert!(!again.fetched);
        let _ = std::fs::remove_dir_all(&cache);
        Ok(())
    }

    #[test]
    fn never_refresh_without_cache_fails() {
        let (repo, http) = fake_repo(true);
        assert!(sync(&repo, &http, &temp("never"), Refresh::Never, &mut || {}).is_err());
        assert!(http.hits.borrow().is_empty());
    }
}
