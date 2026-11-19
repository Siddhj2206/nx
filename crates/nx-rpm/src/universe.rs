//! Everything the host tier resolves against: the image's installed set and
//! the repos it enables, loaded into one libsolv pool.
//!
//! Loading goes through `.solv` caches: the installed set's is keyed by the
//! image digest (it can't change until the next deployment), each repo's by
//! its primary metadata checksum. A warm load reads only those.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use nx_solv::{Package, Pool, RepoId, Solution, SolvableId};

use crate::fetch::{Freshness, Http, Refresh, Synced, sync};
use crate::repo::{self, Repo, Vars};
use crate::{Error, Result};

/// Where and what to load.
#[derive(Debug, Clone)]
pub struct Options {
    /// The image's root: repo files and the rpmdb are read under it.
    pub root: PathBuf,
    /// nx's metadata cache (`repodata/` and `solv/` go under it).
    pub cache: PathBuf,
    /// The booted image's digest, keying the installed set's cache. `None`
    /// (not a bootc system) reads the rpmdb every time.
    pub image_digest: Option<String>,
    pub vars: Vars,
    pub arch: String,
    pub refresh: Refresh,
}

/// One loaded repo.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub repo: Repo,
    pub synced: Synced,
    pub id: RepoId,
}

/// A repo that could not be loaded; the rest still resolve.
#[derive(Debug)]
pub struct Skipped {
    pub repo: String,
    pub error: Error,
}

/// The installed set and the enabled repos, ready to resolve.
pub struct Universe {
    pool: Pool,
    /// The machine's arch; packages of other arches (`i686`, `src`) are
    /// left out of search and info.
    arch: String,
    pub repos: Vec<Loaded>,
    pub skipped: Vec<Skipped>,
    /// Packages no command offers because they come from the same source
    /// as an image package but are a different build (image relatives must
    /// be the image's exact build).
    hidden: HashSet<SolvableId>,
}

/// What `nx add` would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Install(Vec<Package>),
    Problems(Vec<String>),
}

impl Universe {
    /// `on_fetch` is called with a repo's id just before its metadata is
    /// downloaded.
    pub fn load(opts: &Options, http: &dyn Http, on_fetch: &mut dyn FnMut(&str)) -> Result<Self> {
        let mut pool = Pool::new(&opts.arch)?;
        let solv_dir = opts.cache.join("solv");
        create_dir(&solv_dir)?;

        load_installed(&mut pool, opts, &solv_dir)?;

        let repos = repo::load(&opts.root, &opts.vars)?
            .into_iter()
            .filter(|r| r.enabled);
        let mut loaded = Vec::new();
        let mut skipped = Vec::new();
        let repodata = opts.cache.join("repodata");
        for repo in repos {
            let id = repo.id.clone();
            let result = sync(&repo, http, &repodata, opts.refresh, &mut || on_fetch(&id))
                .and_then(|synced| {
                    load_repo(&mut pool, &repo, &synced, &solv_dir).map(|id| (synced, id))
                });
            match result {
                Ok((synced, id)) => loaded.push(Loaded { repo, synced, id }),
                Err(error) => skipped.push(Skipped {
                    repo: repo.id,
                    error,
                }),
            }
        }
        Ok(Self::assemble(pool, &opts.arch, loaded, skipped))
    }

    /// Finishes a pool whose installed set and repos are loaded.
    fn assemble(mut pool: Pool, arch: &str, repos: Vec<Loaded>, skipped: Vec<Skipped>) -> Self {
        pool.prepare();
        let hidden = other_builds_of_image_sources(&pool).into_iter().collect();
        Self {
            pool,
            arch: arch.to_owned(),
            repos,
            skipped,
            hidden,
        }
    }

    /// Repos past their `metadata_expire` or that could not be refreshed.
    pub fn stale(&self) -> impl Iterator<Item = &Loaded> {
        self.repos
            .iter()
            .filter(|l| !matches!(l.synced.freshness, Freshness::Fetched | Freshness::Cached))
    }

    /// A package's long description.
    pub fn description(&self, p: &Package) -> String {
        self.pool.description(p.id)
    }

    /// Whether a package runs here: the machine's arch or `noarch`.
    fn native(&self, p: &Package) -> bool {
        p.arch == self.arch || p.arch == "noarch"
    }

    /// Packages whose name or summary contains `term` (case-insensitive),
    /// newest build of each name, exact name matches first. Image packages
    /// are included and marked installed.
    pub fn search(&self, term: &str) -> Vec<Package> {
        let needle = term.to_lowercase();
        let mut best: HashMap<String, Package> = HashMap::new();
        for id in self.pool.all() {
            if self.hidden.contains(&id) {
                continue;
            }
            let p = self.pool.package(id);
            if !self.native(&p) {
                continue;
            }
            if !p.name.to_lowercase().contains(&needle)
                && !p.summary.to_lowercase().contains(&needle)
            {
                continue;
            }
            self.keep_best(&mut best, p);
        }
        let mut out: Vec<Package> = best.into_values().collect();
        out.sort_by(|a, b| {
            let rank = |p: &Package| {
                let name = p.name.to_lowercase();
                (
                    name != needle,
                    !name.starts_with(&needle),
                    !name.contains(&needle),
                )
            };
            rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// The package a name means: the image's copy if installed, else the
    /// newest available build that the image allows.
    pub fn info(&self, spec: &str) -> Result<Option<Package>> {
        let mut best: HashMap<String, Package> = HashMap::new();
        for id in self.pool.select(spec)? {
            if self.hidden.contains(&id) {
                continue;
            }
            self.keep_best(&mut best, self.pool.package(id));
        }
        let mut found: Vec<Package> = best.into_values().collect();
        // Prefer the exact name asked for, then the native arch.
        found.sort_by_key(|p| (p.name != spec, !self.native(p)));
        Ok(found.into_iter().next())
    }

    /// What installing `specs` would add. When it can't be solved and a
    /// repo failed to load, the problems say so: a missing repo, not a
    /// missing package, is the likelier cause.
    pub fn plan(&mut self, specs: &[&str]) -> Result<Plan> {
        let hidden: Vec<SolvableId> = self.hidden.iter().copied().collect();
        Ok(match self.pool.install(specs, &hidden)? {
            Solution::Install(p) => Plan::Install(p),
            Solution::Problems(mut problems) => {
                if !self.skipped.is_empty() {
                    let repos: Vec<&str> = self.skipped.iter().map(|s| s.repo.as_str()).collect();
                    problems.push(format!(
                        "{} could not be loaded (see above); try again",
                        repos.join(", ")
                    ));
                }
                Plan::Problems(problems)
            }
        })
    }

    /// Keeps the installed copy of a name, or else its newest build.
    fn keep_best(&self, best: &mut HashMap<String, Package>, p: Package) {
        let key = format!("{}.{}", p.name, p.arch);
        match best.get(&key) {
            Some(cur) if cur.installed => {}
            Some(cur) if !p.installed && self.pool.evr_cmp(&cur.evr, &p.evr).is_ge() => {}
            _ => {
                best.insert(key, p);
            }
        }
    }
}

fn load_installed(pool: &mut Pool, opts: &Options, solv_dir: &Path) -> Result<RepoId> {
    let cache = opts.image_digest.as_ref().map(|d| {
        let key = d.strip_prefix("sha256:").unwrap_or(d);
        solv_dir.join(format!("installed-{key}.solv"))
    });
    if let Some(path) = &cache
        && path.exists()
        && let Ok(id) = pool.load_solv("@image", path, 0)
    {
        pool.set_installed(id);
        return Ok(id);
    }
    let id = pool.load_rpmdb("@image", &opts.root)?;
    if let Some(path) = &cache {
        remove_matching(solv_dir, "installed-");
        // A failed cache write only costs speed next time.
        let _ = pool.write_solv(id, path);
    }
    Ok(id)
}

fn load_repo(pool: &mut Pool, repo: &Repo, synced: &Synced, solv_dir: &Path) -> Result<RepoId> {
    let sum = synced.primary_checksum();
    let key = sum.get(..16).unwrap_or(sum);
    let path = solv_dir.join(format!("{}-{key}.solv", repo.id));
    if path.exists()
        && let Ok(id) = pool.load_solv(&repo.id, &path, repo.priority)
    {
        return Ok(id);
    }
    let id = pool.load_rpmmd(&repo.id, &synced.primary, repo.priority)?;
    remove_matching(solv_dir, &format!("{}-", repo.id));
    let _ = pool.write_solv(id, &path);
    Ok(id)
}

/// Available packages built from an image package's source, but not from
/// the same build of it. `wayland-devel` must match the image's `wayland`
/// exactly; a newer one in `updates` is the image's next update, not ours.
fn other_builds_of_image_sources(pool: &Pool) -> Vec<SolvableId> {
    let mut image: HashMap<String, HashSet<String>> = HashMap::new();
    let mut available = Vec::new();
    for id in pool.all() {
        let p = pool.package(id);
        if p.installed {
            image
                .entry(p.source_name().to_owned())
                .or_default()
                .insert(p.source);
        } else {
            available.push((id, p.source));
        }
    }
    available
        .into_iter()
        .filter(|(_, source)| {
            image
                .get(nx_solv::source_name(source))
                .is_some_and(|builds| !builds.contains(source))
        })
        .map(|(id, _)| id)
        .collect()
}

fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| Error::Write {
        path: dir.to_owned(),
        source,
    })
}

/// Removes `<prefix><hex key>.solv` files from earlier loads. The key must
/// be all hex, so `updates-` never matches `updates-archive-…`.
fn remove_matching(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let stale = name
            .to_string_lossy()
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(".solv"))
            .is_some_and(|key| !key.is_empty() && key.bytes().all(|b| b.is_ascii_hexdigit()));
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, ver: &str, arch: &str, src: &str) -> String {
        format!(
            r#"<package type="rpm"><name>{name}</name><arch>{arch}</arch>
<version epoch="0" ver="{ver}" rel="1.fc45"/><summary>{name} summary</summary>
<location href="Packages/{name}.rpm"/>
<format><rpm:sourcerpm>{src}-{ver}-1.fc45.src.rpm</rpm:sourcerpm>
<rpm:provides><rpm:entry name="{name}" flags="EQ" epoch="0" ver="{ver}" rel="1.fc45"/></rpm:provides>
</format></package>"#
        )
    }

    /// An image with wayland 1.24 and a repo that has moved on to 1.25,
    /// plus an i686 build and an unrelated package.
    fn universe(tag: &str) -> Result<Universe> {
        let dir = std::env::temp_dir().join(format!("nx-universe-{tag}-{}", std::process::id()));
        create_dir(&dir)?;
        let repos = [
            ("@image", vec![pkg("wayland", "1.24", "x86_64", "wayland")]),
            (
                "updates",
                vec![
                    pkg("wayland-devel", "1.24", "x86_64", "wayland"),
                    pkg("wayland-devel", "1.25", "x86_64", "wayland"),
                    pkg("wayland-devel", "1.25", "i686", "wayland"),
                    pkg("weston", "14", "x86_64", "weston"),
                ],
            ),
        ];
        let mut pool = Pool::new("x86_64")?;
        for (name, pkgs) in repos {
            let path = dir.join(format!("{name}.xml"));
            let xml = format!(
                r#"<?xml version="1.0"?><metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="{}">{}</metadata>"#,
                pkgs.len(),
                pkgs.concat()
            );
            std::fs::write(&path, xml).map_err(|source| Error::Write {
                path: path.clone(),
                source,
            })?;
            let id = pool.load_rpmmd(name, &path, 99)?;
            if name == "@image" {
                pool.set_installed(id);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(Universe::assemble(pool, "x86_64", Vec::new(), Vec::new()))
    }

    fn nevras(pkgs: &[Package]) -> Vec<String> {
        pkgs.iter().map(Package::nevra).collect()
    }

    #[test]
    fn search_offers_what_add_installs() -> Result<()> {
        let mut u = universe("search")?;
        let found = u.search("wayland-devel");
        assert_eq!(nevras(&found), vec!["wayland-devel-1.24-1.fc45.x86_64"]);
        let Plan::Install(plan) = u.plan(&["wayland-devel"])? else {
            panic!("wayland-devel should resolve");
        };
        assert_eq!(nevras(&plan), nevras(&found));
        Ok(())
    }

    #[test]
    fn info_skips_other_builds_and_arches() -> Result<()> {
        let u = universe("info")?;
        let p = u.info("wayland-devel")?.map(|p| p.nevra());
        assert_eq!(p.as_deref(), Some("wayland-devel-1.24-1.fc45.x86_64"));
        Ok(())
    }

    #[test]
    fn problems_blame_skipped_repos() -> Result<()> {
        let mut u = universe("skipped")?;
        u.skipped.push(Skipped {
            repo: "fedora".into(),
            error: Error::Invalid("offline".into()),
        });
        let Plan::Problems(p) = u.plan(&["nope"])? else {
            panic!("nope should not resolve");
        };
        assert_eq!(
            p.last().map(String::as_str),
            Some("fedora could not be loaded (see above); try again")
        );
        Ok(())
    }
}
