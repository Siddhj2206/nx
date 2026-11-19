//! From a manifest to a generation: resolve against the image and the
//! repos, download what the store lacks, verify and unpack it, assemble.
//! Shared by `add` and `repo`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use nx_core::generation::{self, Spec};
use nx_core::state::{self, Staged};
use nx_core::{Built, Generation, Lock, LockedPackage, LockedRepo, Manifest, State};
use nx_rpm::packages::Job;
use nx_rpm::{Package, Plan, Planned, Refresh, Relative, Repo, RepoKeys, Universe, Ureq};

use crate::host::{self, Machine};
use crate::ui::{self, Ui};

/// Downloads at once; Fedora's mirrors are fine with a handful.
const THREADS: usize = 4;

/// Room left over after a change, so a full disk never stops the next one
/// half-way.
const SPARE: u64 = 32 << 20;

/// The machine, its state and the current generation, loaded once.
pub struct Session<'m> {
    machine: &'m Machine,
    pub state: State,
    /// Held unless this is a dry run.
    guard: Option<state::Lock>,
    pub current: Option<Generation>,
    /// The current manifest, changed as the command asked; the repos were
    /// loaded for it.
    pub manifest: Manifest,
    pub universe: Universe,
    http: Ureq,
    keys: Keys,
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

impl<'m> Session<'m> {
    /// Takes the state lock (unless this is a dry run, so two changes
    /// queue), reads the current generation, applies `change` to its
    /// manifest, and loads the repos the result names. `change` runs under
    /// the lock, so no other command's change is lost.
    pub fn open(
        ui: &mut Ui,
        machine: &'m Machine,
        refresh: Refresh,
        dry_run: bool,
        change: impl FnOnce(&mut Manifest) -> Result<()>,
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
        let current = match Generation::current(&state) {
            Ok(g) => g,
            Err(e @ nx_core::Error::Newer { .. }) => {
                return Err(anyhow::Error::new(e).context(
                    "a newer nx wrote the current generation; this one can only read (nx list, nx why)",
                ));
            }
            Err(e) => return Err(anyhow::Error::new(e).context("reading the current generation")),
        };
        let mut manifest = current
            .as_ref()
            .map(|g| g.manifest.clone())
            .unwrap_or_default();
        change(&mut manifest)?;
        let universe = host::load(ui, machine, refresh, &manifest)?;
        let keys = Keys::new(machine, &state, current.as_ref(), &manifest);
        Ok(Self {
            machine,
            state,
            guard,
            current,
            manifest,
            universe,
            http: Ureq::new(),
            keys,
            koji_repo: HashMap::new(),
        })
    }

    /// Accepts the keys now pinned for `ids`, though they differ from what
    /// the current generation recorded: the user confirmed them.
    pub fn trust(&mut self, ids: &[String]) {
        self.keys.trusted.extend(ids.iter().cloned());
    }

    fn locked(&self) -> &[LockedPackage] {
        self.current.as_ref().map_or(&[], |g| &g.lock.host)
    }

    /// Resolves `specs` (the whole manifest) into a host tier.
    pub fn resolve(&mut self, ui: &mut Ui, specs: &[String]) -> Result<Resolved> {
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        let locked: Vec<String> = self.locked().iter().map(|p| p.nevra.clone()).collect();
        let repos: HashMap<String, Repo> = self
            .universe
            .repos
            .iter()
            .map(|l| (l.repo.id.clone(), l.repo.clone()))
            .collect();
        let cache = host::cache_dir()?.join("koji");
        let (http, keys, koji_repo) = (&self.http, &mut self.keys, &mut self.koji_repo);
        let err = &mut ui.err;
        let mut koji = |rel: &Relative, stand_in: &Package| -> nx_rpm::Result<PathBuf> {
            let repo = repos
                .get(&stand_in.repo)
                .ok_or_else(|| nx_rpm::Error::Invalid(format!("no repo {}", stand_in.repo)))?;
            let repo_keys = keys.get(repo)?;
            koji_repo.insert(format!("{}.{}", rel.name, rel.arch), repo.id.clone());
            // Kept after unpacking: a locked Koji build is re-added on every
            // resolve, and that must not need the network.
            if !cache.join(rel.file_name()).exists() {
                let sym = err.style.cyan(ui::SYM_FETCH);
                err.line(format_args!(
                    "{sym} {} {} {}",
                    rel.name,
                    err.style.dim(&format!("{}-{}", rel.version, rel.release)),
                    err.style
                        .dim("from Koji (the repos have moved past the image's build)")
                ));
            }
            nx_rpm::koji(http, rel, repo_keys, &cache)
        };
        let plan = self.universe.plan(&specs, &locked, &mut koji)?;
        let all = match plan {
            Plan::Install(p) => p,
            Plan::Problems(problems) => {
                bail!("can't resolve {}\n{}", specs.join(" "), problems.join("\n"))
            }
        };
        let have: HashSet<&str> = self.locked().iter().map(|p| p.nevra.as_str()).collect();
        let new = all
            .iter()
            .filter(|p| !have.contains(p.package.nevra().as_str()))
            .map(|p| p.package.clone())
            .collect();
        Ok(Resolved { all, new })
    }

    /// Downloads, verifies and unpacks what the store lacks, builds the
    /// next generation from `manifest` and `all`, and makes it live. A
    /// generation nxd refuses stays on disk, not current.
    pub fn install(&mut self, ui: &mut Ui, manifest: &Manifest, all: &[Planned]) -> Result<Built> {
        let guard = self.guard.take().context("a dry run cannot install")?;
        let mut files = self.fetch(ui, all)?;
        let host = self.unpack(&guard, all, &mut files)?;
        let lock = self.lock(host)?;
        let image_root = self.machine.image_root()?;
        let built = generation::build(
            &self.state,
            &guard,
            &Spec {
                manifest,
                lock: &lock,
                image_root: &image_root,
                os_id: &self.machine.release.id,
            },
        )?;
        crate::live::activate(
            ui,
            &self.state,
            built.generation.number,
            manifest.host.backend,
        )?;
        Ok(built)
    }

    /// Downloads what the store lacks, after checking there is room for it
    /// and for its unpacked files. Returns each package's file by NEVRA.
    fn fetch(&self, ui: &mut Ui, all: &[Planned]) -> Result<HashMap<String, PathBuf>> {
        let store = self.state.store();
        let missing: Vec<&Planned> = all
            .iter()
            .filter(|p| !store.contains(&p.package.sha256) || p.file.is_some())
            .collect();
        // Koji builds are on disk already.
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
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        check_space(
            &dir,
            jobs.iter().map(|j| j.package.download_size).sum(),
            self.state.root(),
            missing.iter().map(|p| p.package.install_size).sum(),
        )?;
        let files = download(ui, &self.http, &jobs, &dir)?;
        Ok(jobs
            .iter()
            .zip(files)
            .map(|(j, f)| (j.package.nevra(), f))
            .collect())
    }

    /// Verifies and unpacks each new package into the store, one at a time
    /// (librpm is single-threaded), and moves them in together once their
    /// files are on disk. Returns the lockfile's host tier.
    fn unpack(
        &mut self,
        guard: &state::Lock,
        all: &[Planned],
        files: &mut HashMap<String, PathBuf>,
    ) -> Result<Vec<LockedPackage>> {
        let store = self.state.store();
        let mut staged: Vec<Staged> = Vec::new();
        let mut locked = Vec::new();
        for p in all {
            let nevra = p.package.nevra();
            let from = self.source(p, files)?;
            if let Some(file) = &from.file {
                let repo = self
                    .universe
                    .repo(&from.repo)
                    .with_context(|| format!("no keys for {nevra}"))?
                    .repo
                    .clone();
                let hint = self.keys.change_hint(&repo.id);
                let repo_keys = self.keys.get(&repo)?;
                let stage = store.stage(guard, &from.sha256, |dest| -> Result<()> {
                    nx_librpm::unpack(file, &repo_keys.keyring, dest)
                        .with_context(|| format!("unpacking {nevra}{hint}"))?;
                    Ok(())
                })?;
                staged.extend(stage);
                if p.file.is_none() {
                    let _ = std::fs::remove_file(file);
                }
            }
            locked.push(LockedPackage {
                name: p.package.name.clone(),
                nevra,
                repo: p.package.repo.clone(),
                sha256: from.sha256,
                requested: p.requested.clone(),
            });
        }
        store.commit(guard, staged)?;
        Ok(locked)
    }

    /// Where `p`'s file is, what it must hash to, and whose keys sign it.
    fn source(&self, p: &Planned, files: &mut HashMap<String, PathBuf>) -> Result<Source> {
        Ok(match &p.file {
            Some(f) => Source {
                file: Some(f.clone()),
                sha256: nx_rpm::packages::sha256_file(f)?,
                repo: self
                    .koji_repo
                    .get(&format!("{}.{}", p.package.name, p.package.arch))
                    .cloned()
                    .unwrap_or_default(),
            },
            None => Source {
                file: files.remove(&p.package.nevra()),
                sha256: p.package.sha256.clone(),
                repo: p.package.repo.clone(),
            },
        })
    }

    /// The lockfile for `host`: the image, and every repo's metadata and
    /// keys. An image repo with no readable key is recorded without one;
    /// an added repo's key must check out.
    fn lock(&mut self, host: Vec<LockedPackage>) -> Result<Lock> {
        let mut lock = Lock::new(
            self.machine.digest(),
            &self.machine.release.version_id,
            self.universe.arch(),
        );
        lock.host = host;
        for l in &self.universe.repos {
            let added = self.keys.is_added(&l.repo.id);
            let keys = match self.keys.get(&l.repo) {
                Ok(k) => k.fingerprints.clone(),
                Err(e) if added => return Err(e.into()),
                Err(_) => Vec::new(),
            };
            lock.repos.push(LockedRepo {
                id: l.repo.id.clone(),
                primary: l.synced.primary_checksum().to_owned(),
                revision: l.synced.repomd.revision.clone(),
                keys,
            });
        }
        Ok(lock)
    }
}

/// Where a package's file is, what it must hash to, and the repo whose
/// keys sign it.
struct Source {
    /// `None` when the store has it already.
    file: Option<PathBuf>,
    sha256: String,
    repo: String,
}

/// Repo keys, read as packages need them. An added repo's keys must match
/// what the current generation recorded (design, *Repositories and COPR*:
/// a changed key blocks the repo until the user confirms it).
struct Keys {
    root: PathBuf,
    dir: PathBuf,
    loaded: HashMap<String, RepoKeys>,
    /// Added repo id → its name and the fingerprints the current
    /// generation recorded.
    pinned: HashMap<String, (String, Vec<String>)>,
    /// Added repo ids → names.
    added: HashMap<String, String>,
    /// Repos whose new key the user confirmed (`nx repo add --new-key`).
    trusted: Vec<String>,
    generation: u32,
}

impl Keys {
    fn new(
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
                    .filter(|r| !r.keys.is_empty())
                    .filter_map(|r| {
                        let name = added.get(&r.id)?;
                        Some((r.id.clone(), (name.clone(), r.keys.clone())))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            root: machine.root.clone(),
            dir: host::keys_dir(state),
            loaded: HashMap::new(),
            pinned,
            added,
            trusted: Vec::new(),
            generation: current.map_or(0, |g| g.number),
        }
    }

    fn is_added(&self, id: &str) -> bool {
        self.added.contains_key(id)
    }

    fn get(&mut self, repo: &Repo) -> nx_rpm::Result<&RepoKeys> {
        if !self.loaded.contains_key(&repo.id) {
            let keys = nx_rpm::keyring(repo, &self.root, &self.dir)?;
            if let Some((name, was)) = self.pinned.get(&repo.id)
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

    /// What to suggest when an added repo's package fails to verify.
    fn change_hint(&self, id: &str) -> String {
        self.added.get(id).map_or_else(String::new, |name| {
            format!(
                " (if {name} changed its signing key, check the new one and run nx repo add {name} --new-key)"
            )
        })
    }
}

/// Refuses a change the disk can't hold: `download` bytes into `cache`
/// and `unpack` bytes into `store`, plus [`SPARE`] on each filesystem.
fn check_space(cache: &Path, download: u64, store: &Path, unpack: u64) -> Result<()> {
    if state::same_filesystem(cache, store) {
        return need(store, download + unpack);
    }
    need(cache, download)?;
    need(store, unpack)
}

fn need(path: &Path, bytes: u64) -> Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let free = state::free_space(path)?;
    if free < bytes + SPARE {
        bail!(
            "not enough space on the filesystem holding {}: {} needed, {} free",
            path.display(),
            ui::size(bytes + SPARE),
            ui::size(free)
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use nx_rpm::repo::Added;

    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/rpm")
            .join(name)
    }

    #[test]
    fn a_changed_key_blocks_the_repo_until_trusted() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-cli-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let vars = nx_rpm::Vars {
            releasever: "45".into(),
            basearch: "x86_64".into(),
        };
        let repo = Repo::added(
            &Added {
                name: "copr:lilay/topgrade",
                baseurl: None,
                metalink: None,
                gpgkey: None,
                priority: None,
            },
            &vars,
        )?;
        std::fs::copy(
            testdata("copr-lilay-topgrade.gpg"),
            dir.join(nx_rpm::packages::pinned_key(&repo.id)),
        )?;
        let name = "copr:lilay/topgrade".to_owned();
        let mut keys = Keys {
            root: PathBuf::from("/"),
            dir: dir.clone(),
            loaded: HashMap::new(),
            pinned: HashMap::from([(
                repo.id.clone(),
                (
                    name.clone(),
                    vec!["0000000000000000000000000000000000000000".into()],
                ),
            )]),
            added: HashMap::from([(repo.id.clone(), name)]),
            trusted: Vec::new(),
            generation: 3,
        };
        let err = keys
            .get(&repo)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.starts_with("the key pinned for copr:lilay/topgrade changed since generation 3"),
            "{err}"
        );
        keys.trusted.push(repo.id.clone());
        assert_eq!(keys.get(&repo)?.fingerprints.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn refuses_what_the_disk_cannot_hold() -> Result<()> {
        let dir = std::env::temp_dir();
        check_space(&dir, 1 << 20, &dir, 1 << 20)?;
        let err = check_space(&dir, u64::MAX / 4, &dir, 0)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.starts_with("not enough space on the filesystem holding"),
            "{err}"
        );
        Ok(())
    }
}
