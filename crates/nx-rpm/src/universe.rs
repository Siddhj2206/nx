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
use crate::packages::Relative;
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
    /// Repos the manifest adds, loaded after the image's.
    pub extra: Vec<Repo>,
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
    /// Hidden builds of image relatives whose exact build no repo has; the
    /// exact build comes from Koji when a plan needs one.
    missing: HashSet<SolvableId>,
    /// Source package name → the image's build of it
    /// (`wayland-1.24.0-1.fc45.src.rpm`).
    image_sources: HashMap<String, String>,
    /// Package files added from Koji, and where they are.
    koji: Option<RepoId>,
    files: HashMap<SolvableId, PathBuf>,
}

/// What `nx add` would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Install(Vec<Planned>),
    Problems(Vec<String>),
}

/// One package of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub package: Package,
    /// The specs that pull it in.
    pub requested: Vec<String>,
    /// Already downloaded and verified (a Koji build).
    pub file: Option<PathBuf>,
}

/// How many rounds of Koji downloads a plan may take: a Koji build can need
/// another relative the repos lack, but never many.
const KOJI_ROUNDS: usize = 3;

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
            .filter(|r| r.enabled)
            .chain(opts.extra.iter().cloned());
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

    /// The image's installed set alone, no repos: enough to say what the
    /// image ships, and fast (one `.solv` load).
    pub fn load_image(opts: &Options) -> Result<Self> {
        let mut pool = Pool::new(&opts.arch)?;
        let solv_dir = opts.cache.join("solv");
        create_dir(&solv_dir)?;
        load_installed(&mut pool, opts, &solv_dir)?;
        Ok(Self::assemble(pool, &opts.arch, Vec::new(), Vec::new()))
    }

    /// Finishes a pool whose installed set and repos are loaded.
    fn assemble(mut pool: Pool, arch: &str, repos: Vec<Loaded>, skipped: Vec<Skipped>) -> Self {
        pool.prepare();
        let relatives = Relatives::find(&pool);
        Self {
            pool,
            arch: arch.to_owned(),
            repos,
            skipped,
            hidden: relatives.hidden,
            missing: relatives.missing,
            image_sources: relatives.image_sources,
            koji: None,
            files: HashMap::new(),
        }
    }

    /// The architecture packages are resolved for.
    pub fn arch(&self) -> &str {
        &self.arch
    }

    /// The loaded repo a package comes from.
    pub fn repo(&self, id: &str) -> Option<&Loaded> {
        self.repos.iter().find(|l| l.repo.id == id)
    }

    /// Image packages that own `path`.
    pub fn image_owners(&self, path: &str) -> Result<Vec<Package>> {
        Ok(self
            .pool
            .image_owners(path)?
            .into_iter()
            .map(|id| self.pool.package(id))
            .collect())
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

    /// Why `p` plugs into the system and so belongs in the host tier
    /// ([`crate::plugs::plugs_in`]), or `None` for a standalone package.
    pub fn plugs_in(&self, p: &Package, toolchains: &crate::plugs::Toolchains) -> Option<String> {
        crate::plugs::plugs_in(
            crate::plugs::Meta {
                name: &p.name,
                provides: &self.pool.deps(p.id, nx_solv::Deps::Provides),
                requires: &self.pool.deps(p.id, nx_solv::Deps::Requires),
                files: &self.pool.files(p.id),
            },
            toolchains,
        )
    }

    /// Whether the image has a package named `name`.
    pub fn in_image(&self, name: &str) -> bool {
        self.pool
            .select(name)
            .is_ok_and(|ids| ids.iter().any(|id| self.pool.package(*id).installed))
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

    /// What installing `specs` would add. `locked` NEVRAs (what the current
    /// generation has) are preferred, so adding one package doesn't move the
    /// others. An image relative whose exact build the repos lack is fetched
    /// with `koji` (given the relative and the newer build that stood in for
    /// it) and added to the pool. When it can't be solved and a repo failed
    /// to load, the problems say so: a missing repo, not a missing package,
    /// is the likelier cause.
    pub fn plan(
        &mut self,
        specs: &[&str],
        locked: &[String],
        koji: &mut dyn FnMut(&Relative, &Package) -> Result<PathBuf>,
    ) -> Result<Plan> {
        let mut for_pool = Vec::new();
        for spec in specs {
            for_pool.push(self.for_pool(spec)?);
        }
        let solv: Vec<&str> = for_pool.iter().map(String::as_str).collect();
        let favored = self.locked_ids(locked)?;
        let mut problems = Vec::new();
        for _ in 0..=KOJI_ROUNDS {
            let hidden: Vec<SolvableId> = self.hidden.iter().copied().collect();
            let involved = match self.pool.install(&solv, &hidden, &favored)? {
                Solution::Install(p) => {
                    let planned = self.attribute(specs, &solv, p, &hidden, &favored)?;
                    return Ok(Plan::Install(planned));
                }
                Solution::Problems(p) => {
                    problems = p.messages;
                    p.involved
                }
            };
            // A hidden build in the way whose exact build no repo has: the
            // image's build is on Koji. One per name, native arches only.
            let mut needed: Vec<Package> = Vec::new();
            for id in involved.into_iter().filter(|id| self.missing.contains(id)) {
                let p = self.pool.package(id);
                if self.native(&p) && !needed.iter().any(|n| n.name == p.name && n.arch == p.arch) {
                    needed.push(p);
                }
            }
            if needed.is_empty() {
                break;
            }
            for stand_in in needed {
                self.add_from_koji(&stand_in, koji)?;
            }
            self.pool.prepare();
        }
        Ok(Plan::Problems(self.blame_skipped(problems)))
    }

    /// A spec as the pool reads it: `foo@copr:owner/project` names the repo
    /// by its id (`foo@copr:owner:project`), which must be loaded.
    fn for_pool(&self, spec: &str) -> Result<String> {
        let Some((name, repo)) = nx_solv::split_repo(spec) else {
            return Ok(spec.to_owned());
        };
        let id = repo::id_of(repo);
        if self.repo(&id).is_none() {
            return Err(Error::Invalid(format!(
                "{spec}: no repo {repo} (nx repo list shows the repos)"
            )));
        }
        Ok(format!("{name}@{id}"))
    }

    /// Fetches the image's exact build of `stand_in`'s name and adds it.
    fn add_from_koji(
        &mut self,
        stand_in: &Package,
        koji: &mut dyn FnMut(&Relative, &Package) -> Result<PathBuf>,
    ) -> Result<()> {
        let rel = self.relative(stand_in).ok_or_else(|| {
            Error::Invalid(format!("{} has no build in the image", stand_in.source))
        })?;
        let file = koji(&rel, stand_in)?;
        let repo = match self.koji {
            Some(r) => r,
            None => {
                let r = self.pool.create_empty("koji", repo::DEFAULT_PRIORITY)?;
                self.koji = Some(r);
                r
            }
        };
        let id = self.pool.add_rpm(repo, &file)?;
        let got = self.pool.package(id);
        let exact = self.image_sources.get(got.source_name()) == Some(&got.source);
        if got.name != rel.name || got.arch != rel.arch || !exact {
            return Err(Error::Invalid(format!(
                "Koji served {} for {}",
                got.nevra(),
                rel.nevra()
            )));
        }
        self.files.insert(id, file);
        // Its name is no longer missing; the newer builds stay hidden.
        let pool = &self.pool;
        self.missing.retain(|m| {
            let q = pool.package(*m);
            !(q.name == stand_in.name && q.arch == stand_in.arch)
        });
        Ok(())
    }

    /// The image's build of the source `p` comes from, as a Koji path.
    fn relative(&self, p: &Package) -> Option<Relative> {
        let srpm = self.image_sources.get(p.source_name())?;
        let stem = srpm.strip_suffix(".src.rpm").unwrap_or(srpm);
        let mut parts = stem.rsplitn(3, '-');
        let (release, version, source) = (parts.next()?, parts.next()?, parts.next()?);
        Some(Relative {
            name: p.name.clone(),
            arch: p.arch.clone(),
            source: source.to_owned(),
            version: version.to_owned(),
            release: release.to_owned(),
        })
    }

    /// Pool ids of the locked NEVRAs still available.
    fn locked_ids(&self, locked: &[String]) -> Result<Vec<SolvableId>> {
        let mut ids = Vec::new();
        for nevra in locked {
            for id in self.pool.select(nevra)? {
                if !self.hidden.contains(&id) && self.pool.package(id).nevra() == *nevra {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    /// Which specs pull in each package: each spec solved on its own.
    /// `specs` are as the user wrote them, `solv` as the pool reads them.
    fn attribute(
        &mut self,
        specs: &[&str],
        solv: &[&str],
        packages: Vec<Package>,
        hidden: &[SolvableId],
        favored: &[SolvableId],
    ) -> Result<Vec<Planned>> {
        let mut wanted: Vec<(String, HashSet<String>)> = Vec::new();
        if specs.len() > 1 {
            for (spec, solv) in specs.iter().zip(solv) {
                let names = match self.pool.install(&[solv], hidden, favored)? {
                    Solution::Install(p) => p.into_iter().map(|p| p.name).collect(),
                    Solution::Problems(_) => HashSet::new(),
                };
                wanted.push(((*spec).to_owned(), names));
            }
        }
        Ok(packages
            .into_iter()
            .map(|package| {
                let mut requested: Vec<String> = wanted
                    .iter()
                    .filter(|(_, names)| names.contains(&package.name))
                    .map(|(spec, _)| spec.clone())
                    .collect();
                if requested.is_empty() {
                    requested = specs.iter().map(|s| (*s).to_owned()).collect();
                }
                Planned {
                    file: self.files.get(&package.id).cloned(),
                    package,
                    requested,
                }
            })
            .collect())
    }

    /// libsolv's words, in nx's terms, and a note on repos that failed.
    fn blame_skipped(&self, problems: Vec<String>) -> Vec<String> {
        let mut problems: Vec<String> = problems
            .into_iter()
            .map(|p| p.replace(" is disabled", " is not the image's build of its source"))
            .collect();
        if !self.skipped.is_empty() {
            let repos: Vec<&str> = self.skipped.iter().map(|s| s.repo.as_str()).collect();
            problems.push(format!(
                "{} could not be loaded (see above); try again",
                repos.join(", ")
            ));
        }
        problems
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

/// What the image's builds mean for the repos' packages.
struct Relatives {
    /// Available packages built from an image package's source, but not
    /// from the same build of it. `wayland-devel` must match the image's
    /// `wayland` exactly; a newer one in `updates` is the image's next
    /// update, not ours.
    hidden: HashSet<SolvableId>,
    /// Hidden packages whose name and arch no repo offers in the exact
    /// build.
    missing: HashSet<SolvableId>,
    image_sources: HashMap<String, String>,
}

impl Relatives {
    fn find(pool: &Pool) -> Self {
        let mut image: HashMap<String, HashSet<String>> = HashMap::new();
        let mut installed_names = HashSet::new();
        let mut available = Vec::new();
        for id in pool.all() {
            let p = pool.package(id);
            if p.installed {
                installed_names.insert((p.name.clone(), p.arch.clone()));
                image
                    .entry(p.source_name().to_owned())
                    .or_default()
                    .insert(p.source);
            } else {
                available.push(p);
            }
        }
        let other_build = |p: &Package| {
            image
                .get(p.source_name())
                .is_some_and(|builds| !builds.contains(&p.source))
        };
        let mut hidden = HashSet::new();
        let mut exact = HashSet::new();
        for p in &available {
            if other_build(p) {
                hidden.insert(p.id);
            } else {
                exact.insert((p.name.clone(), p.arch.clone()));
            }
        }
        let missing = available
            .iter()
            .filter(|p| hidden.contains(&p.id))
            .filter(|p| {
                let key = (p.name.clone(), p.arch.clone());
                !exact.contains(&key) && !installed_names.contains(&key)
            })
            .map(|p| p.id)
            .collect();
        let image_sources = image
            .into_iter()
            .filter_map(|(name, builds)| Some((name, builds.into_iter().max()?)))
            .collect();
        Self {
            hidden,
            missing,
            image_sources,
        }
    }
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
        load(
            tag,
            vec![
                ("@image", vec![pkg("wayland", "1.24", "x86_64", "wayland")]),
                (
                    "updates",
                    vec![
                        pkg("wayland-devel", "1.24", "x86_64", "wayland"),
                        pkg("wayland-devel", "1.25", "x86_64", "wayland"),
                        pkg("wayland-devel", "1.25", "i686", "wayland"),
                        pkg("weston", "14", "x86_64", "weston"),
                        pkg("weston", "15", "x86_64", "weston"),
                    ],
                ),
            ],
        )
    }

    fn load(tag: &str, repos: Vec<(&str, Vec<String>)>) -> Result<Universe> {
        let dir = std::env::temp_dir().join(format!("nx-universe-{tag}-{}", std::process::id()));
        create_dir(&dir)?;
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

    fn planned(plan: &[Planned]) -> Vec<String> {
        plan.iter().map(|p| p.package.nevra()).collect()
    }

    fn no_koji(rel: &Relative, _: &Package) -> Result<PathBuf> {
        Err(Error::Invalid(format!(
            "unexpected Koji fetch of {}",
            rel.nevra()
        )))
    }

    #[test]
    fn search_offers_what_add_installs() -> Result<()> {
        let mut u = universe("search")?;
        let found = u.search("wayland-devel");
        assert_eq!(nevras(&found), vec!["wayland-devel-1.24-1.fc45.x86_64"]);
        let Plan::Install(plan) = u.plan(&["wayland-devel"], &[], &mut no_koji)? else {
            panic!("wayland-devel should resolve");
        };
        assert_eq!(planned(&plan), nevras(&found));
        assert_eq!(plan[0].requested, ["wayland-devel"]);
        Ok(())
    }

    #[test]
    fn keeps_locked_builds_and_says_who_wanted_what() -> Result<()> {
        let mut u = universe("locked")?;
        let locked = ["weston-14-1.fc45.x86_64".to_owned()];
        let Plan::Install(plan) = u.plan(&["weston", "wayland-devel"], &locked, &mut no_koji)?
        else {
            panic!("should resolve");
        };
        assert_eq!(
            planned(&plan),
            [
                "wayland-devel-1.24-1.fc45.x86_64",
                "weston-14-1.fc45.x86_64"
            ]
        );
        assert_eq!(plan[0].requested, ["wayland-devel"]);
        assert_eq!(plan[1].requested, ["weston"]);
        let Plan::Install(plan) = u.plan(&["weston"], &[], &mut no_koji)? else {
            panic!("should resolve");
        };
        assert_eq!(planned(&plan), ["weston-15-1.fc45.x86_64"]);
        Ok(())
    }

    #[test]
    fn a_repo_suffix_names_a_loaded_repo() -> Result<()> {
        let mut u = universe("pin")?;
        let err = u.plan(&["weston@copr:o/p"], &[], &mut no_koji);
        assert!(
            matches!(&err, Err(Error::Invalid(m)) if m.contains("no repo copr:o/p")),
            "{err:?}"
        );
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
        let Plan::Problems(p) = u.plan(&["nope"], &[], &mut no_koji)? else {
            panic!("nope should not resolve");
        };
        assert_eq!(
            p.last().map(String::as_str),
            Some("fedora could not be loaded (see above); try again")
        );
        Ok(())
    }
}

#[cfg(test)]
mod koji_tests {
    use super::*;
    use crate::testing::testdata;

    /// libxkbcommon 1.13.1-4.fc45 in the image, with what its -devel needs;
    /// the repo has only a moved-on 1.14 -devel, which (like every real
    /// -devel) needs its own build of the library.
    fn universe() -> Result<Universe> {
        let image = r#"<package type="rpm"><name>libxkbcommon</name><arch>x86_64</arch>
<version epoch="0" ver="1.13.1" rel="4.fc45"/><summary>s</summary><location href="x.rpm"/>
<format><rpm:sourcerpm>libxkbcommon-1.13.1-4.fc45.src.rpm</rpm:sourcerpm><rpm:provides>
<rpm:entry name="libxkbcommon" flags="EQ" epoch="0" ver="1.13.1" rel="4.fc45"/>
<rpm:entry name="libxkbcommon(x86-64)" flags="EQ" epoch="0" ver="1.13.1" rel="4.fc45"/>
<rpm:entry name="libxkbcommon.so.0()(64bit)"/><rpm:entry name="libxkbregistry.so.0()(64bit)"/>
<rpm:entry name="pkgconfig(libxml-2.0)"/></rpm:provides>
<file>/usr/bin/pkg-config</file></format></package>"#;
        let devel = r#"<package type="rpm"><name>libxkbcommon-devel</name><arch>x86_64</arch>
<version epoch="0" ver="1.14.0" rel="1.fc45"/><summary>s</summary><location href="y.rpm"/>
<format><rpm:sourcerpm>libxkbcommon-1.14.0-1.fc45.src.rpm</rpm:sourcerpm><rpm:provides>
<rpm:entry name="libxkbcommon-devel" flags="EQ" epoch="0" ver="1.14.0" rel="1.fc45"/>
</rpm:provides><rpm:requires>
<rpm:entry name="libxkbcommon(x86-64)" flags="EQ" epoch="0" ver="1.14.0" rel="1.fc45"/>
</rpm:requires></format></package>"#;
        let dir = std::env::temp_dir().join(format!("nx-universe-koji-{}", std::process::id()));
        create_dir(&dir)?;
        let mut pool = Pool::new("x86_64")?;
        for (name, pkg) in [("@image", image), ("updates", devel)] {
            let path = dir.join(format!("{name}.xml"));
            let xml = format!(
                r#"<?xml version="1.0"?><metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="1">{pkg}</metadata>"#
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

    #[test]
    fn fetches_the_exact_build_from_koji() -> Result<()> {
        let mut u = universe()?;
        let mut asked = Vec::new();
        let plan = u.plan(&["libxkbcommon-devel"], &[], &mut |rel, stand_in| {
            asked.push((rel.clone(), stand_in.nevra()));
            Ok(testdata("libxkbcommon-devel-1.13.1-4.fc45.x86_64.rpm"))
        })?;
        let Plan::Install(plan) = plan else {
            panic!("should resolve through Koji: {plan:?}");
        };
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0].0,
            Relative {
                name: "libxkbcommon-devel".into(),
                arch: "x86_64".into(),
                source: "libxkbcommon".into(),
                version: "1.13.1".into(),
                release: "4.fc45".into(),
            }
        );
        assert_eq!(asked[0].1, "libxkbcommon-devel-1.14.0-1.fc45.x86_64");
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].package.nevra(),
            "libxkbcommon-devel-1.13.1-4.fc45.x86_64"
        );
        assert_eq!(plan[0].package.repo, "koji");
        assert!(plan[0].file.is_some());
        Ok(())
    }
}
