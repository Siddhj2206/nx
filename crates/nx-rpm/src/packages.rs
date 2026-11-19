//! Package files: downloading them from their repo (or Koji, for an image
//! relative the repos no longer carry) and the keys they must verify
//! against.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use nx_librpm::Keyring;
use nx_solv::Package;

use crate::fetch::{Http, mirrors, sha256_hex, write_atomic};
use crate::repo::Repo;
use crate::universe::Loaded;
use crate::{Error, Result};

/// The keys a repo's packages must be signed with.
pub struct RepoKeys {
    pub keyring: Keyring,
    /// Sorted, without duplicates.
    pub fingerprints: Vec<String>,
}

/// The keys `repo`'s packages must be signed with. `file://` keys are read
/// from the image under `root`; any other URL takes `pinned`, the key
/// `nx repo add` fetched and pinned (one per repo, whatever URLs it
/// names). Never the system keyring.
pub fn keyring(repo: &Repo, root: &Path, pinned: Option<&str>) -> Result<RepoKeys> {
    if repo.gpgkey.is_empty() {
        return Err(Error::Invalid(format!("{} names no gpgkey", repo.id)));
    }
    let mut keyring = Keyring::new()?;
    let mut fingerprints = Vec::new();
    let mut pinned_added = false;
    for url in &repo.gpgkey {
        match url.strip_prefix("file://") {
            Some(abs) => {
                let path = root.join(abs.trim_start_matches('/'));
                let text = std::fs::read_to_string(&path)
                    .map_err(|source| Error::Read { path, source })?;
                fingerprints.extend(keyring.add(&text)?);
            }
            None if pinned_added => {}
            None => {
                let text = pinned.ok_or_else(|| {
                    Error::Invalid(format!(
                        "{} has no pinned key (nx repo add pins it)",
                        repo.id
                    ))
                })?;
                fingerprints.extend(keyring.add(text)?);
                pinned_added = true;
            }
        }
    }
    fingerprints.sort();
    fingerprints.dedup();
    Ok(RepoKeys {
        keyring,
        fingerprints,
    })
}

/// The file name a repo's key was pinned under before nx kept keys by
/// content (`keys/<sha256>.asc`); read when a lockfile names no key.
pub fn legacy_pinned_key(repo_id: &str) -> String {
    format!("{}.asc", repo_id.replace(['/', ':'], "_"))
}

/// Downloads `p` from `from` into `dir`, checked against the SHA-256 in the
/// repo metadata. Tries the mirror the metadata came from, then the others.
pub fn download(http: &dyn Http, from: &Loaded, p: &Package, dir: &Path) -> Result<PathBuf> {
    if p.sha256.is_empty() {
        return Err(Error::Invalid(format!(
            "{}: the repo metadata has no SHA-256 for it",
            p.nevra()
        )));
    }
    let file = dir.join(format!("{}.rpm", p.nevra()));
    if file.exists() && sha256_file(&file)? == p.sha256 {
        return Ok(file);
    }
    let mut bases = vec![from.synced.base.clone()];
    let mut last = None;
    let mut tried_mirrors = false;
    let mut i = 0;
    while i < bases.len() {
        let url = format!("{}{}", bases[i], p.location);
        match http
            .get(&url)
            .and_then(|body| check(&url, &body, &p.sha256).map(|()| body))
        {
            Ok(body) => {
                write_atomic(&file, &body)?;
                return Ok(file);
            }
            Err(e) => last = Some(e),
        }
        i += 1;
        if i == bases.len() && !tried_mirrors {
            tried_mirrors = true;
            if let Ok((more, _)) = mirrors(&from.repo, http) {
                bases.extend(more.into_iter().filter(|b| *b != from.synced.base));
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Invalid(format!("{} has no mirrors", from.repo.id))))
}

fn check(url: &str, body: &[u8], sha256: &str) -> Result<()> {
    let got = sha256_hex(body);
    if got == sha256 {
        Ok(())
    } else {
        Err(Error::Fetch {
            url: url.to_owned(),
            reason: "checksum does not match the repo metadata".into(),
        })
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let body = std::fs::read(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })?;
    Ok(sha256_hex(&body))
}

/// One download for [`download_all`].
pub struct Job<'a> {
    pub from: &'a Loaded,
    pub package: &'a Package,
}

/// Downloads every job into `dir`, `threads` at a time, calling `done` as
/// each lands. Returns the files in job order; the first failure wins.
pub fn download_all(
    http: &dyn Http,
    jobs: &[Job],
    dir: &Path,
    threads: usize,
    done: &(dyn Fn(&Package) + Sync),
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir).map_err(|source| Error::Write {
        path: dir.to_owned(),
        source,
    })?;
    let next = Mutex::new(0usize);
    let results: Mutex<Vec<Option<Result<PathBuf>>>> =
        Mutex::new((0..jobs.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, jobs.len().max(1)) {
            s.spawn(|| {
                loop {
                    let i = {
                        let Ok(mut n) = next.lock() else { return };
                        let i = *n;
                        *n += 1;
                        i
                    };
                    let Some(job) = jobs.get(i) else { return };
                    let r = download(http, job.from, job.package, dir);
                    if r.is_ok() {
                        done(job.package);
                    }
                    if let Ok(mut all) = results.lock() {
                        all[i] = Some(r);
                    }
                }
            });
        }
    });
    let all = results
        .into_inner()
        .map_err(|_| Error::Invalid("a download thread panicked".into()))?;
    all.into_iter()
        .map(|r| r.unwrap_or_else(|| Err(Error::Invalid("a download did not run".into()))))
        .collect()
}

/// An image relative whose exact build the repos no longer carry: the
/// binary package `name.arch` from the image's build of `source`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relative {
    pub name: String,
    pub arch: String,
    /// The source package's name, version and release (the image's build).
    pub source: String,
    pub version: String,
    pub release: String,
}

impl Relative {
    pub fn file_name(&self) -> String {
        format!(
            "{}-{}-{}.{}.rpm",
            self.name, self.version, self.release, self.arch
        )
    }

    pub fn nevra(&self) -> String {
        format!(
            "{}-{}-{}.{}",
            self.name, self.version, self.release, self.arch
        )
    }
}

/// Where Fedora's build system keeps every build.
pub const KOJI: &str = "https://kojipkgs.fedoraproject.org/packages";

/// Downloads `rel` from Koji's signed copies into `dir` and verifies it
/// against `keys`. Koji files a signed copy under the key's short ID, so
/// each fingerprint is tried in turn.
pub fn koji(http: &dyn Http, rel: &Relative, keys: &RepoKeys, dir: &Path) -> Result<PathBuf> {
    let keyring = &keys.keyring;
    std::fs::create_dir_all(dir).map_err(|source| Error::Write {
        path: dir.to_owned(),
        source,
    })?;
    let file = dir.join(rel.file_name());
    if file.exists() && nx_librpm::verify(&file, keyring).is_ok() {
        return Ok(file);
    }
    let mut last = None;
    for fp in &keys.fingerprints {
        let short = fp.get(fp.len().saturating_sub(8)..).unwrap_or(fp);
        let url = format!(
            "{KOJI}/{}/{}/{}/data/signed/{}/{}/{}",
            rel.source,
            rel.version,
            rel.release,
            short.to_ascii_lowercase(),
            rel.arch,
            rel.file_name()
        );
        match http.get(&url) {
            Ok(body) => {
                write_atomic(&file, &body)?;
                if let Err(e) = nx_librpm::verify(&file, keyring) {
                    let _ = std::fs::remove_file(&file);
                    return Err(e.into());
                }
                return Ok(file);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| Error::Invalid(format!("no key to find {} on Koji", rel.nevra()))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{Freshness, Synced};
    use crate::repo::{Vars, parse};
    use crate::repomd::Repomd;
    use crate::testing::{FakeHttp, TempDir, testdata};

    const RPM: &str = "libxkbcommon-devel-1.13.1-4.fc45.x86_64.rpm";
    const FEDORA_45: &str = "4f50a6114cd5c6976a7f1179655a4b02f577861e";

    fn vars() -> Vars {
        Vars {
            releasever: "45".into(),
            basearch: "x86_64".into(),
        }
    }

    fn rpm_bytes() -> Vec<u8> {
        std::fs::read(testdata(RPM)).unwrap_or_default()
    }

    fn loaded(repo_text: &str, base: &str) -> Loaded {
        Loaded {
            repo: parse(repo_text, Path::new("t.repo"), &vars()).remove(0),
            synced: Synced {
                repomd: Repomd::default(),
                primary: PathBuf::new(),
                base: base.into(),
                freshness: Freshness::Cached,
            },
            id: Default::default(),
        }
    }

    fn package() -> Package {
        Package {
            name: "libxkbcommon-devel".into(),
            evr: "1.13.1-4.fc45".into(),
            arch: "x86_64".into(),
            location: format!("Packages/l/{RPM}"),
            sha256: sha256_hex(&rpm_bytes()),
            ..Package::default()
        }
    }

    #[test]
    fn keys_come_from_the_image_or_the_pin() -> Result<()> {
        let tmp = TempDir::new("keys");
        let root = tmp.0.join("root");
        let key_dir = root.join("usr/share/pki/rpm-gpg");
        std::fs::create_dir_all(&key_dir).map_err(|source| Error::Write {
            path: key_dir.clone(),
            source,
        })?;
        let key = std::fs::read(testdata("RPM-GPG-KEY-fedora-45-primary")).unwrap_or_default();
        let _ = std::fs::write(key_dir.join("RPM-GPG-KEY-fedora-45-x86_64"), &key);
        let fedora = parse(
            "[fedora]\nbaseurl=https://x/\ngpgcheck=1\ngpgkey=file:///usr/share/pki/rpm-gpg/RPM-GPG-KEY-fedora-$releasever-$basearch\n",
            Path::new("f.repo"),
            &vars(),
        )
        .remove(0);
        let keys = keyring(&fedora, &root, None)?;
        assert_eq!(keys.fingerprints, [FEDORA_45]);

        let copr = parse(
            "[copr:a:b]\nbaseurl=https://x/\ngpgkey=https://copr/pubkey.gpg\n",
            Path::new("c.repo"),
            &vars(),
        )
        .remove(0);
        assert!(keyring(&copr, &root, None).is_err(), "not pinned yet");
        let copr_key =
            std::fs::read_to_string(testdata("copr-lilay-topgrade.gpg")).unwrap_or_default();
        let keys = keyring(&copr, &root, Some(&copr_key))?;
        assert_eq!(keys.fingerprints.len(), 1);
        Ok(())
    }

    #[test]
    fn downloads_and_checks_against_the_metadata() -> Result<()> {
        let tmp = TempDir::new("dl");
        let from = loaded("[fedora]\nbaseurl=https://good/\n", "https://stale/");
        let mut http = FakeHttp::default();
        // The mirror the metadata came from serves a different file; the
        // repo's baseurl has the right one.
        http.bodies
            .insert(format!("https://stale/Packages/l/{RPM}"), b"junk".to_vec());
        http.bodies
            .insert(format!("https://good/Packages/l/{RPM}"), rpm_bytes());
        let p = package();
        let landed = Mutex::new(Vec::new());
        let files = download_all(
            &http,
            &[Job {
                from: &from,
                package: &p,
            }],
            &tmp.0,
            4,
            &|p| {
                if let Ok(mut l) = landed.lock() {
                    l.push(p.name.clone());
                }
            },
        )?;
        assert_eq!(sha256_file(&files[0])?, p.sha256);
        assert_eq!(
            landed.into_inner().unwrap_or_default(),
            ["libxkbcommon-devel"]
        );

        // Cached: no request.
        http.clear();
        download(&http, &from, &p, &tmp.0)?;
        assert!(http.hits().is_empty(), "{:?}", http.hits());

        let bad = Package {
            sha256: "00".repeat(32),
            ..p
        };
        assert!(download(&http, &from, &bad, &tmp.0).is_err());
        Ok(())
    }

    #[test]
    fn fetches_signed_builds_from_koji() -> Result<()> {
        let tmp = TempDir::new("koji");
        let rel = Relative {
            name: "libxkbcommon-devel".into(),
            arch: "x86_64".into(),
            source: "libxkbcommon".into(),
            version: "1.13.1".into(),
            release: "4.fc45".into(),
        };
        let mut http = FakeHttp::default();
        http.bodies.insert(
            format!("{KOJI}/libxkbcommon/1.13.1/4.fc45/data/signed/f577861e/x86_64/{RPM}"),
            rpm_bytes(),
        );
        let mut keyring = Keyring::new()?;
        let text =
            std::fs::read_to_string(testdata("RPM-GPG-KEY-fedora-45-primary")).unwrap_or_default();
        let fingerprints = keyring.add(&text)?;
        let keys = RepoKeys {
            keyring,
            fingerprints: fingerprints.clone(),
        };
        let file = koji(&http, &rel, &keys, &tmp.0)?;
        assert_eq!(file.file_name().and_then(|f| f.to_str()), Some(RPM));

        // Signed by a key the repo doesn't use: refused and not kept.
        let _ = std::fs::remove_file(&file);
        let mut other = Keyring::new()?;
        let copr = std::fs::read_to_string(testdata("copr-lilay-topgrade.gpg")).unwrap_or_default();
        other.add(&copr)?;
        let other = RepoKeys {
            keyring: other,
            fingerprints,
        };
        assert!(koji(&http, &rel, &other, &tmp.0).is_err());
        assert!(!file.exists());
        Ok(())
    }
}
