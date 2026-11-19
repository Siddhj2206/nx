//! From a manifest to a generation: resolve against the image and the
//! repos, download what the store lacks, verify and unpack it, assemble.
//! Shared by `add` and `repo`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use nx_core::generation::{self, Spec};
use nx_core::{
    Built, DeploymentSource, Generation, Lock, LockedPackage, LockedRepo, Manifest, OsRelease,
    RpmOstree, State,
};
use nx_librpm::Keyring;
use nx_rpm::packages::Job;
use nx_rpm::{Package, Plan, Planned, Refresh, Relative, Universe, Ureq};

use crate::host;
use crate::ui::{self, Ui};

/// Downloads at once; Fedora's mirrors are fine with a handful.
const THREADS: usize = 4;

/// The machine, its state and the current generation, loaded once.
pub struct Session {
    pub state: State,
    /// Held unless this is a dry run.
    guard: Option<nx_core::state::Lock>,
    pub current: Option<Generation>,
    pub universe: Universe,
    http: Ureq,
    /// Repo id → its keys, read as packages need them.
    keyrings: HashMap<String, (Keyring, Vec<String>)>,
    /// Koji `name.arch` → the repo whose keys verified it.
    koji_repo: HashMap<String, String>,
}

/// What a resolve found.
pub struct Resolved {
    /// Every package of the new host tier.
    pub all: Vec<Planned>,
    /// The ones the current generation doesn't have.
    pub new: Vec<Package>,
}

impl Session {
    /// Loads everything for a change to `manifest` (or the current one).
    /// A real change takes the state lock first, so two changes queue.
    pub fn open(
        ui: &mut Ui,
        refresh: Refresh,
        dry_run: bool,
        repos: Option<&Manifest>,
    ) -> Result<Self> {
        let state = host::state();
        let guard = if dry_run {
            None
        } else {
            Some(state.lock().with_context(|| {
                format!(
                    "nx needs write access to {} (set NX_STATE_DIR to use another directory)",
                    state.root().display()
                )
            })?)
        };
        let current = Generation::current(&state).context("reading the current generation")?;
        let empty = Manifest::default();
        let manifest = repos
            .or(current.as_ref().map(|g| &g.manifest))
            .unwrap_or(&empty);
        let universe = host::load(ui, refresh, manifest)?;
        Ok(Self {
            state,
            guard,
            current,
            universe,
            http: Ureq::new(),
            keyrings: HashMap::new(),
            koji_repo: HashMap::new(),
        })
    }

    pub fn manifest(&self) -> Manifest {
        self.current
            .as_ref()
            .map(|g| g.manifest.clone())
            .unwrap_or_default()
    }

    fn locked(&self) -> &[LockedPackage] {
        self.current.as_ref().map_or(&[], |g| &g.lock.host)
    }

    /// Resolves `specs` (the whole manifest) into a host tier.
    pub fn resolve(&mut self, ui: &mut Ui, specs: &[String]) -> Result<Resolved> {
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        let locked: Vec<String> = self.locked().iter().map(|p| p.nevra.clone()).collect();
        let repos: HashMap<String, nx_rpm::Repo> = self
            .universe
            .repos
            .iter()
            .map(|l| (l.repo.id.clone(), l.repo.clone()))
            .collect();
        let cache = host::cache_dir()?.join("koji");
        let keys = host::keys_dir(&self.state);
        let (http, keyrings, koji_repo) = (&self.http, &mut self.keyrings, &mut self.koji_repo);
        let err = &mut ui.err;
        let mut koji = |rel: &Relative, stand_in: &Package| -> nx_rpm::Result<PathBuf> {
            let repo = repos
                .get(&stand_in.repo)
                .ok_or_else(|| nx_rpm::Error::Invalid(format!("no repo {}", stand_in.repo)))?;
            let (keyring, fps) = keyring_for(keyrings, repo, &keys)?;
            koji_repo.insert(format!("{}.{}", rel.name, rel.arch), repo.id.clone());
            // Kept after unpacking: a locked Koji build is re-added on every
            // resolve, and that must not need the network.
            if cache.join(rel.file_name()).exists() {
                return nx_rpm::koji(http, rel, keyring, fps, &cache);
            }
            let sym = err.style.cyan(ui::SYM_FETCH);
            err.line(format_args!(
                "{sym} {} {} {}",
                rel.name,
                err.style.dim(&format!("{}-{}", rel.version, rel.release)),
                err.style
                    .dim("from Koji (the repos have moved past the image's build)")
            ));
            nx_rpm::koji(http, rel, keyring, fps, &cache)
        };
        let plan = self.universe.plan(&specs, &locked, &mut koji)?;
        let all = match plan {
            Plan::Install(p) => p,
            Plan::Problems(problems) => {
                bail!("can't resolve {}\n{}", specs.join(" "), problems.join("\n"))
            }
        };
        let have: Vec<&str> = self.locked().iter().map(|p| p.nevra.as_str()).collect();
        let new = all
            .iter()
            .filter(|p| !have.contains(&p.package.nevra().as_str()))
            .map(|p| p.package.clone())
            .collect();
        Ok(Resolved { all, new })
    }

    /// Downloads, verifies and unpacks what the store lacks, then builds the
    /// next generation from `manifest` and `all`.
    pub fn install(&mut self, ui: &mut Ui, manifest: &Manifest, all: &[Planned]) -> Result<Built> {
        let guard = self.guard.take().context("a dry run cannot install")?;
        let store = self.state.store();

        // Download what isn't in the store; Koji builds are on disk already.
        let missing: Vec<&Planned> = all
            .iter()
            .filter(|p| p.file.is_some() || !store.contains(&p.package.sha256))
            .collect();
        let jobs: Vec<Job> = missing
            .iter()
            .filter(|p| p.file.is_none())
            .map(|p| {
                let from = self.universe.repo(&p.package.repo).with_context(|| {
                    format!("{} comes from an unloaded repo", p.package.nevra())
                })?;
                Ok(Job {
                    from,
                    package: &p.package,
                })
            })
            .collect::<Result<_>>()?;
        let dir = host::cache_dir()?.join("packages");
        let files = download(ui, &self.http, &jobs, &dir)?;
        let mut downloaded: HashMap<String, PathBuf> = jobs
            .iter()
            .zip(files)
            .map(|(j, f)| (j.package.nevra(), f))
            .collect();

        // Verify and unpack, one at a time (librpm is single-threaded).
        let mut locked = Vec::new();
        for p in all {
            let nevra = p.package.nevra();
            let (file, sha256, repo_id) = match &p.file {
                Some(f) => (
                    Some(f.clone()),
                    nx_rpm::packages::sha256_file(f)?,
                    self.koji_repo
                        .get(&format!("{}.{}", p.package.name, p.package.arch))
                        .cloned()
                        .unwrap_or_default(),
                ),
                None => (
                    downloaded.remove(&nevra),
                    p.package.sha256.clone(),
                    p.package.repo.clone(),
                ),
            };
            if let Some(file) = file {
                let repo = self
                    .universe
                    .repo(&repo_id)
                    .with_context(|| format!("no keys for {nevra}"))?
                    .repo
                    .clone();
                let (keyring, _) =
                    keyring_for(&mut self.keyrings, &repo, &host::keys_dir(&self.state))?;
                store.insert(&guard, &sha256, |dest| -> Result<()> {
                    nx_librpm::unpack(&file, keyring, dest)
                        .with_context(|| format!("unpacking {nevra}"))?;
                    Ok(())
                })?;
                if p.file.is_none() {
                    let _ = std::fs::remove_file(&file);
                }
            }
            locked.push(LockedPackage {
                name: p.package.name.clone(),
                nevra,
                repo: p.package.repo.clone(),
                sha256,
                requested: p.requested.clone(),
            });
        }

        let source = RpmOstree;
        let release = OsRelease::read(Path::new("/")).context("reading the image's os-release")?;
        let digest = source
            .deployments()
            .ok()
            .and_then(|d| d.booted().and_then(|b| b.image_digest.clone()));
        let mut lock = Lock::new(digest, &release.version_id, std::env::consts::ARCH);
        lock.host = locked;
        let keys = host::keys_dir(&self.state);
        for l in &self.universe.repos {
            // Pin every repo's keys, used or not; one that has none (or an
            // unreadable one) is recorded without.
            let _ = keyring_for(&mut self.keyrings, &l.repo, &keys);
            lock.repos.push(LockedRepo {
                id: l.repo.id.clone(),
                primary: l.synced.primary_checksum().to_owned(),
                revision: l.synced.repomd.revision.clone(),
                keys: self
                    .keyrings
                    .get(&l.repo.id)
                    .map(|(_, fps)| fps.clone())
                    .unwrap_or_default(),
            });
        }
        let image_root = host::image_root(&source);
        let built = generation::build(
            &self.state,
            &guard,
            &Spec {
                manifest,
                lock: &lock,
                image_root: &image_root,
                os_id: &release.id,
            },
        )?;
        Ok(built)
    }
}

fn keyring_for<'a>(
    keyrings: &'a mut HashMap<String, (Keyring, Vec<String>)>,
    repo: &nx_rpm::Repo,
    keys: &Path,
) -> nx_rpm::Result<&'a (Keyring, Vec<String>)> {
    if !keyrings.contains_key(&repo.id) {
        let k = nx_rpm::keyring(repo, Path::new("/"), keys)?;
        keyrings.insert(repo.id.clone(), k);
    }
    keyrings
        .get(&repo.id)
        .ok_or_else(|| nx_rpm::Error::Invalid(format!("no keys for {}", repo.id)))
}

/// Downloads on worker threads; this thread draws a `↓` line as each lands.
fn download(ui: &mut Ui, http: &Ureq, jobs: &[Job], dir: &Path) -> Result<Vec<PathBuf>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let (tx, rx) = mpsc::channel::<Package>();
    std::thread::scope(|s| {
        let worker = s.spawn(move || {
            nx_rpm::download_all(http, jobs, dir, THREADS, &|p| {
                let _ = tx.send(p.clone());
            })
        });
        for p in rx {
            let st = ui.err.style;
            ui.err.line(format_args!(
                "{} {} {}  {}",
                st.cyan(ui::SYM_FETCH),
                p.name,
                st.dim(&p.evr),
                st.dim(&ui::size(p.download_size))
            ));
        }
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("the download thread panicked"))?
            .context("downloading packages")
    })
}
