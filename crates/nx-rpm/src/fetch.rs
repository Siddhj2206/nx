//! Downloads a repo's metadata into a local cache and keeps it fresh.
//!
//! The cache holds, per repo, `repomd.xml` and the files it names, each
//! verified against its checksum. Commands read the cache and go to the
//! network only for what is missing or when asked (`--refresh`); metadata
//! past its `metadata_expire` is used and reported as [`Freshness::Expired`].

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::repo::Repo;
use crate::repomd::{Checksum, DataFile, Metalink, Repomd, parse_metalink, parse_repomd};
use crate::{Error, Result};

pub(crate) use nx_http::Hashing;
pub use nx_http::{Http, Ureq, sha256_hex};

/// A repo's metadata on disk, verified.
#[derive(Debug, Clone)]
pub struct Synced {
    pub repomd: Repomd,
    /// The repo's `primary` metadata (compressed, as downloaded).
    pub primary: PathBuf,
    /// Base URL the metadata came from, for downloading packages later.
    pub base: String,
    /// Where it came from, and whether it is still current.
    pub freshness: Freshness,
}

/// How current a repo's metadata is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Downloaded by this call.
    Fetched,
    /// Downloaded by this call from a later mirror: the earlier ones
    /// failed, and the error says why the last of them did.
    OtherMirror(String),
    /// From the cache, within the repo's `metadata_expire`.
    Cached,
    /// From the cache, past the repo's `metadata_expire`.
    Expired,
    /// From the cache because every mirror failed; the error says why.
    Unreachable(String),
    /// From the cache, which an earlier refresh failed to update (the
    /// refresh timer's, say); the error says why. Recorded beside the cache
    /// in `last-error` until a refresh works.
    Failed(String),
}

/// Beside a repo's cached metadata: why the last refresh of it failed.
const LAST_ERROR: &str = "last-error";

impl Synced {
    /// How long ago this metadata was last checked against the repo: the
    /// age of its cached `repomd.xml`, rewritten on every fetch.
    pub fn age(&self) -> Option<std::time::Duration> {
        age(&self.primary.with_file_name("repomd.xml"))
    }

    /// `repomd.xml`'s `primary` checksum: what a `.solv` cache is keyed by.
    pub fn primary_checksum(&self) -> &str {
        self.repomd.get("primary").map_or("", |p| &p.checksum.hex)
    }
}

/// When to go to the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Never: what is cached, however old, or an error. For work that must
    /// not wait on the network (checking a generation at boot).
    Never,
    /// Only for repos with nothing cached. What commands do: they never
    /// wait on a download the cache could have answered.
    IfMissing,
    /// When the cache is older than the repo's `metadata_expire` (the
    /// background refresh).
    IfStale,
    /// Always check for new metadata (`--refresh`).
    Always,
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
    let mut cached = read_cached(&dir);
    if let Some(synced) = &mut cached {
        let expired = repo
            .metadata_expire
            .is_some_and(|expire| age(&dir.join("repomd.xml")).is_none_or(|age| age >= expire));
        if expired && synced.freshness == Freshness::Cached {
            synced.freshness = Freshness::Expired;
        }
        let use_cache = match refresh {
            Refresh::Never | Refresh::IfMissing => true,
            Refresh::IfStale => !expired,
            Refresh::Always => false,
        };
        if use_cache {
            return Ok(synced.clone());
        }
    }

    // Only reachable with nothing cached: `Never` always answers from the
    // cache when there is one, above.
    if refresh == Refresh::Never {
        return Err(Error::Invalid(format!(
            "no cached metadata for {}, and not going to the network",
            repo.id
        )));
    }
    on_fetch();
    let err = match fetch(repo, http, &dir, cached.as_ref()) {
        Ok(synced) => {
            let _ = std::fs::remove_file(dir.join(LAST_ERROR));
            return Ok(synced);
        }
        Err(e) => e,
    };
    // Old metadata beats none: nx can still resolve, and the caller tells
    // the user why it is old. The failure is kept for later reads, which
    // don't go to the network themselves. Best effort: a user who can't
    // write the shared cache only loses the note.
    match cached {
        Some(mut synced) => {
            let _ = write_atomic(&dir.join(LAST_ERROR), err.to_string().as_bytes());
            synced.freshness = Freshness::Unreachable(err.to_string());
            Ok(synced)
        }
        None => Err(err),
    }
}

/// Tries each mirror in turn.
fn fetch(repo: &Repo, http: &dyn Http, dir: &Path, cached: Option<&Synced>) -> Result<Synced> {
    let (bases, accepted) = mirrors(repo, http)?;
    let mut last_err = None;
    for base in &bases {
        match fetch_from(base, &accepted, http, dir, cached) {
            Ok(mut synced) => {
                if let Some(e) = last_err {
                    synced.freshness = Freshness::OtherMirror(format!("{e}"));
                }
                return Ok(synced);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::Invalid(format!("{} has no mirrors", repo.id))))
}

/// Base URLs to try, and the `repomd.xml` checksums a metalink vouches for
/// (empty for plain `baseurl` repos).
pub(crate) fn mirrors(repo: &Repo, http: &dyn Http) -> Result<(Vec<String>, Vec<Checksum>)> {
    if let Some(url) = &repo.metalink {
        let body = http.get(url)?;
        let metalink: Metalink = parse_metalink(&String::from_utf8_lossy(&body))?;
        let bases = metalink.bases().map(str::to_owned).collect();
        return Ok((bases, metalink.repomd_checksums));
    }
    let slash = |b: &str| {
        if b.ends_with('/') {
            b.to_owned()
        } else {
            format!("{b}/")
        }
    };
    let mut bases: Vec<String> = repo.baseurl.iter().map(|b| slash(b)).collect();
    if bases.is_empty()
        && let Some(url) = &repo.mirrorlist
    {
        // One base URL per line, as dnf reads it; some servers answer a
        // mirrorlist URL with a metalink instead.
        let text = String::from_utf8_lossy(&http.get(url)?).into_owned();
        if text.contains("<metalink") {
            let metalink = parse_metalink(&text)?;
            let bases = metalink.bases().map(str::to_owned).collect();
            return Ok((bases, metalink.repomd_checksums));
        }
        bases = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(slash)
            .collect();
    }
    if bases.is_empty() {
        return Err(Error::Invalid(format!(
            "{} has no metalink, mirrorlist or baseurl",
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
        freshness: Freshness::Fetched,
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
    let freshness = match std::fs::read_to_string(dir.join(LAST_ERROR)) {
        Ok(error) => Freshness::Failed(error.trim().to_owned()),
        Err(_) => Freshness::Cached,
    };
    Some(Synced {
        repomd,
        primary,
        base,
        freshness,
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
    use crate::testing::FakeHttp;

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
        assert!(
            matches!(&first.freshness, Freshness::OtherMirror(e) if e.contains("https://bad/")),
            "{:?}",
            first.freshness
        );
        assert_eq!(first.base, "https://good/");
        assert_eq!(std::fs::read(&first.primary).ok().as_deref(), Some(PRIMARY));

        http.clear();
        let second = sync(&repo, &http, &cache, Refresh::IfStale, &mut || {})?;
        assert_eq!(second.freshness, Freshness::Cached);
        assert!(http.hits().is_empty(), "fresh cache must not fetch");
        let _ = std::fs::remove_dir_all(&cache);
        Ok(())
    }

    #[test]
    fn a_mirrorlist_names_the_bases() -> Result<()> {
        let cache = temp("mirrorlist");
        let (_, mut http) = fake_repo(true);
        http.bodies.insert(
            "https://list".into(),
            b"# mirrors\nhttps://bad/\n\nhttps://good\n".to_vec(),
        );
        let vars = Vars {
            releasever: "45".into(),
            basearch: "x86_64".into(),
        };
        let repo = parse("[t]\nmirrorlist=https://list\n", Path::new("t.repo"), &vars).remove(0);
        // The first base serves a repomd without a primary; the second is
        // the one that works.
        let synced = sync(&repo, &http, &cache, Refresh::Always, &mut || {})?;
        assert_eq!(synced.base, "https://good/");
        // A mirrorlist that answers with a metalink is read as one.
        let repo = parse("[t]\nmirrorlist=https://meta\n", Path::new("t.repo"), &vars).remove(0);
        let synced = sync(
            &repo,
            &http,
            &temp("mirrorlist-meta"),
            Refresh::Always,
            &mut || {},
        )?;
        assert_eq!(synced.base, "https://good/");
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
        assert!(
            matches!(again.freshness, Freshness::Unreachable(_)),
            "{:?}",
            again.freshness
        );
        let _ = std::fs::remove_dir_all(&cache);
        Ok(())
    }

    #[test]
    fn if_missing_fetches_once_then_reports_expiry() -> Result<()> {
        let cache = temp("missing");
        let (mut repo, http) = fake_repo(true);
        let first = sync(&repo, &http, &cache, Refresh::IfMissing, &mut || {})?;
        // The fixture's first mirror is out of date.
        assert!(matches!(first.freshness, Freshness::OtherMirror(_)));

        // Expired, yet IfMissing still answers from the cache.
        repo.metadata_expire = Some(std::time::Duration::ZERO);
        http.clear();
        let second = sync(&repo, &http, &cache, Refresh::IfMissing, &mut || {})?;
        assert_eq!(second.freshness, Freshness::Expired);
        assert!(http.hits().is_empty(), "IfMissing must not fetch");
        let _ = std::fs::remove_dir_all(&cache);
        Ok(())
    }
}
