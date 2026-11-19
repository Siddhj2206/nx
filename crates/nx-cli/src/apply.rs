//! From a manifest to a generation: resolve against the image and the
//! repos, download what the store lacks, verify and unpack it, expose the
//! tool tier's commands, assemble. Shared by every command that changes
//! the manifest.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use nx_core::generation::{self, Spec};
use nx_core::state::{self, Staged};
use nx_core::{Built, Generation, Lock, LockedPackage, LockedRepo, LockedTool, Manifest, State};
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
    /// Manifest packages the next generation leaves out ([`Session::leave_out`]).
    skipped: Vec<String>,
    /// The next generation's tool tier: the current one's until a command
    /// resolves another ([`crate::tool::Tier::resolve`]).
    pub tool: Option<LockedTool>,
    /// Tool-tier packages this command adds: their commands must not
    /// clash ([`crate::tool::check`]).
    pub fresh_tools: Vec<String>,
    /// Why each manifest entry is in its tier (`nx why`): the current
    /// generation's, plus what this command routed.
    pub routes: BTreeMap<String, String>,
    /// Resolve without preferring the current generation's builds
    /// (`nx upgrade`).
    unlocked: bool,
}

/// The solver found no way to install what was asked: a problem with the
/// request, unlike a network or cache error. `nx sync` leaves such a
/// package out; anything else fails it.
#[derive(Debug)]
pub struct Unresolvable(pub String);

impl std::fmt::Display for Unresolvable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unresolvable {}

/// The packages a generation left out that stay out: still in the manifest
/// and not what the command is about.
fn still_left_out(skipped: &[String], specs: &[String], asked: &[String]) -> Vec<String> {
    skipped
        .iter()
        .filter(|s| specs.contains(s) && !asked.contains(s))
        .cloned()
        .collect()
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
                    "changing packages needs an administrator (a member of wheel): no write access to {}",
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
        let tool = current.as_ref().and_then(|g| g.lock.tool.clone());
        let routes = current
            .as_ref()
            .map(|g| g.lock.routes.clone())
            .unwrap_or_default();
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
            skipped: Vec::new(),
            tool,
            fresh_tools: Vec::new(),
            routes,
            unlocked: false,
        })
    }

    /// Lets the next resolve move host-tier packages to newer builds: it no
    /// longer prefers the ones the current generation has.
    pub fn unlock(&mut self) {
        self.unlocked = true;
    }

    /// Accepts the keys now pinned for `ids`, though they differ from what
    /// the current generation recorded: the user confirmed them.
    pub fn trust(&mut self, ids: &[String]) {
        self.keys.trusted.extend(ids.iter().cloned());
    }

    fn locked(&self) -> &[LockedPackage] {
        self.current.as_ref().map_or(&[], |g| &g.lock.host)
    }

    /// Records that the next generation was resolved without `specs`.
    pub fn leave_out(&mut self, specs: Vec<String>) {
        self.skipped = specs;
    }

    /// Resolves the manifest for a change `nx sync` didn't make: like
    /// [`Session::resolve`], but a package the current generation left out
    /// (it didn't resolve then) stays out while it still doesn't, so one
    /// unavailable package doesn't block every other change. `asked` are
    /// the packages this command is about; they are never left out, so a
    /// typo still fails.
    pub fn resolve_manifest(
        &mut self,
        ui: &mut Ui,
        specs: &[String],
        asked: &[String],
    ) -> Result<Resolved> {
        let left_out = still_left_out(
            self.current.as_ref().map_or(&[], |g| &g.lock.skipped),
            specs,
            asked,
        );
        match self.resolve(ui, specs) {
            Err(e) if !left_out.is_empty() && e.downcast_ref::<Unresolvable>().is_some() => {}
            other => return other,
        }
        let kept: Vec<String> = specs
            .iter()
            .filter(|s| !left_out.contains(s))
            .cloned()
            .collect();
        // What is left fails by itself: that is the problem to report.
        let resolved = self.resolve(ui, &kept)?;
        ui.warn(format_args!(
            "still leaving out {}: they didn't resolve before and still don't",
            left_out.join(", ")
        ));
        self.skipped = left_out;
        Ok(resolved)
    }

    /// Resolves `specs` (the whole manifest) into a host tier.
    pub fn resolve(&mut self, ui: &mut Ui, specs: &[String]) -> Result<Resolved> {
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        let locked: Vec<String> = if self.unlocked {
            Vec::new()
        } else {
            self.locked().iter().map(|p| p.nevra.clone()).collect()
        };
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
                return Err(Unresolvable(format!(
                    "can't resolve {}\n{}",
                    specs.join(" "),
                    problems.join("\n")
                ))
                .into());
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

    /// Downloads, verifies and unpacks what the store lacks and builds the
    /// next generation from `manifest`, `all` and the session's tool tier,
    /// whose outputs must be realised already. It stays on disk, not
    /// current: making it live is [`Session::install`]'s job, or the
    /// caller's for an image that isn't booted. The tool tier's outputs
    /// become GC roots once the generation has its final name.
    pub fn build(&mut self, ui: &mut Ui, manifest: &Manifest, all: &[Planned]) -> Result<Built> {
        let guard = self.guard.take().context("a dry run cannot build")?;
        let mut files = self.fetch(ui, all)?;
        let host = self.unpack(&guard, all, &mut files)?;
        let store = self.state.store();
        let asked = |name: &str| {
            manifest
                .host
                .packages
                .iter()
                .any(|s| s.split('@').next() == Some(name))
        };
        let image_only = image_only(&host, &asked, &|sha| store.path(sha));
        if !image_only.is_empty() {
            return Err(image_hint(&image_only));
        }
        let image_root = self.machine.image_root()?;
        let tool = self.expose(ui, manifest, &host, &image_root)?;
        let mut lock = self.lock(host)?;
        lock.tool = tool;
        lock.routes = self
            .routes
            .iter()
            .filter(|(entry, _)| manifest.has(entry))
            .map(|(e, r)| (e.clone(), r.clone()))
            .collect();
        let has_bins = lock
            .tool
            .as_ref()
            .is_some_and(|t| t.bins().next().is_some());
        let shim = if has_bins {
            Some(crate::tool::shim(&self.state)?)
        } else {
            None
        };
        let built = generation::build(
            &self.state,
            &guard,
            &Spec {
                manifest,
                lock: &lock,
                image_root: &image_root,
                os_id: &self.machine.release.id,
                image_has: &|name| self.universe.in_image(name),
                shim: shim.as_deref(),
            },
        )
        .map_err(refusal_error)?;
        crate::tool::add_roots(&built.generation)?;
        Ok(built)
    }

    /// The tool tier for this image: which commands each package exposes,
    /// given what the image and the host tier (`host`) already have.
    fn expose(
        &self,
        ui: &mut Ui,
        manifest: &Manifest,
        host: &[LockedPackage],
        image_root: &Path,
    ) -> Result<Option<LockedTool>> {
        let Some(mut tool) = self.tool.clone() else {
            return Ok(None);
        };
        // Collected since it was realised: fetch it again. If that fails,
        // the host tier still gets its generation, without those commands.
        if crate::tool::incomplete(&tool)
            && let Err(e) = crate::tool::restore(ui, &tool, &manifest.caches)
        {
            ui.warn(format_args!(
                "tool-tier commands whose Nix paths are gone are left out until nx upgrade --tool: {e:#}"
            ));
        }
        let store = self.state.store();
        let owner = |name: &str| -> Option<String> {
            let dirs = nx_rpm::COMMAND_DIRS;
            if dirs
                .iter()
                .any(|d| std::fs::symlink_metadata(image_root.join(d).join(name)).is_ok())
            {
                return Some("the image".to_owned());
            }
            host.iter()
                .find(|p| {
                    let root = store.path(&p.sha256);
                    dirs.iter()
                        .any(|d| std::fs::symlink_metadata(root.join(d).join(name)).is_ok())
                })
                .map(|p| format!("the host tier's {}", p.name))
        };
        let exposure = crate::tool::expose(
            &mut tool,
            &owner,
            &manifest.overrides.names,
            &crate::tool::commands,
        );
        let previous = self.current.as_ref().and_then(|g| g.lock.tool.as_ref());
        crate::tool::check(
            ui,
            &tool,
            &exposure,
            &self.fresh_tools,
            previous,
            &manifest.overrides.names,
        )?;
        Ok(Some(tool))
    }

    /// [`Session::build`], then makes the generation live through nxd. A
    /// generation nxd refuses stays on disk, not current. The staged and
    /// rollback images get theirs too (design invariant 6), best effort:
    /// the change is live either way.
    pub fn install(&mut self, ui: &mut Ui, manifest: &Manifest, all: &[Planned]) -> Result<Built> {
        let built = self.build(ui, manifest, all)?;
        crate::live::activate(
            ui,
            &self.state,
            built.generation.number,
            manifest.host.backend,
        )?;
        if let Err(e) = crate::sync::deployed(ui, self.machine, Refresh::IfMissing) {
            ui.warn(format_args!(
                "the staged and rollback images were not updated: {e:#} (nx sync retries)"
            ));
        }
        Ok(built)
    }

    /// Downloads what the store lacks, after checking there is room for it
    /// and for its unpacked files. Returns each package's file by NEVRA.
    fn fetch(&self, ui: &mut Ui, all: &[Planned]) -> Result<HashMap<String, PathBuf>> {
        let store = self.state.store();
        let missing: Vec<&Planned> = all
            .iter()
            .filter(|p| {
                // An entry unpacked before nx read scriptlets is fetched
                // again for its record.
                !store.contains(&p.package.sha256)
                    || store.meta(&p.package.sha256).ok().flatten().is_none()
                    || p.file.is_some()
            })
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
                    let unpacked = nx_librpm::unpack(file, &repo_keys.keyring, dest)
                        .with_context(|| format!("unpacking {nevra}{hint}"))?;
                    state::write_meta(dest, &unpacked.meta)?;
                    Ok(())
                })?;
                if stage.is_none() && store.meta(&from.sha256)?.is_none() {
                    // In the store, from before scriptlets were read: add
                    // the record from the file's verified header.
                    let meta = nx_librpm::metadata(file, &repo_keys.keyring)
                        .with_context(|| format!("reading {nevra}{hint}"))?;
                    state::write_meta(&store.path(&from.sha256), &meta)?;
                }
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
        lock.skipped = self.skipped.clone();
        lock.parent = self.current.as_ref().map(|g| g.number);
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

/// What only the image can use, by the directory a package ships (as the
/// RPM names it: the kernel's are under `/lib`): content read before
/// `systemd-sysext` merges anything, or loaded into the kernel.
const IMAGE_ONLY: &[(&str, &str)] = &[
    (
        "lib/modules",
        "ships kernel modules, which only the image's kernel can load",
    ),
    (
        "usr/lib/modules",
        "ships kernel modules, which only the image's kernel can load",
    ),
    (
        "usr/share/selinux/packages",
        "ships an SELinux policy module, which only the image can install",
    ),
    (
        "usr/lib/dracut/modules.d",
        "ships initrd content, which runs before nx's packages are there",
    ),
];

/// The packages of `host` the manifest names (`asked`) that only work
/// from the image, with the reason; `root` is where a package's files are.
/// A dependency with such content (memcached's `memcached-selinux`) is
/// left to the scriptlet translator, whose refusal an allowance can lift.
fn image_only(
    host: &[LockedPackage],
    asked: &dyn Fn(&str) -> bool,
    root: &dyn Fn(&str) -> PathBuf,
) -> Vec<(String, &'static str)> {
    host.iter()
        .filter(|p| asked(&p.name))
        .filter_map(|p| {
            let dir = root(&p.sha256);
            IMAGE_ONLY
                .iter()
                .find(|(path, _)| {
                    std::fs::read_dir(dir.join(path)).is_ok_and(|mut d| d.next().is_some())
                })
                .map(|(_, why)| (p.name.clone(), *why))
        })
        .collect()
}

/// The image hint (design, *Image-level packages*): why, and the line for
/// the user's Containerfile. nx never changes the image itself.
fn image_hint(packages: &[(String, &str)]) -> anyhow::Error {
    let mut msg = String::new();
    for (name, why) in packages {
        msg.push_str(&format!("{name} {why}\n"));
    }
    let names: Vec<&str> = packages.iter().map(|(n, _)| n.as_str()).collect();
    msg.push_str(&format!(
        "Put it in your image instead:\n  RUN dnf install -y {}",
        names.join(" ")
    ));
    anyhow::anyhow!(msg)
}

/// Turns a scriptlet refusal into the message the user needs: what was
/// left, what to do about it.
fn refusal_error(e: nx_core::Error) -> anyhow::Error {
    let nx_core::Error::Scriptlets(refusals) = e else {
        return e.into();
    };
    let mut msg = String::new();
    for r in &refusals {
        msg.push_str(&format!("{} has scriptlets nx can't translate\n", r.nevra));
        for l in &r.leftovers {
            msg.push_str(&format!("  {}\n", l.source));
            for line in l.lines.iter().take(12) {
                msg.push_str(&format!("    {line}\n"));
            }
            if l.lines.len() > 12 {
                msg.push_str(&format!("    … {} more lines\n", l.lines.len() - 12));
            }
        }
    }
    let names: Vec<&str> = refusals.iter().map(|r| r.name.as_str()).collect();
    msg.push_str(&format!(
        "nx never runs scriptlets. Put it in your image instead:\n  RUN dnf install -y {}\n",
        names.join(" ")
    ));
    msg.push_str("or, if you read the text above and it is safe to skip, add it anyway:\n");
    let flags: Vec<String> = refusals
        .iter()
        .map(|r| {
            format!(
                "--allow-scriptlets {}={}",
                r.name,
                &r.digest[..nx_core::manifest::ALLOW_DIGEST_LEN]
            )
        })
        .collect();
    msg.push_str(&format!(
        "  nx add <what you asked for> {}",
        flags.join(" ")
    ));
    anyhow::anyhow!(msg)
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

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_left_out_package_stays_out_unless_asked_for_or_gone() {
        let skipped = v(&["a", "b"]);
        let specs = v(&["a", "b", "c"]);
        assert_eq!(still_left_out(&skipped, &specs, &[]), v(&["a", "b"]));
        // Asked for again: it must resolve now.
        assert_eq!(still_left_out(&skipped, &specs, &v(&["a"])), v(&["b"]));
        // Removed from the manifest: nothing to leave out.
        assert_eq!(still_left_out(&skipped, &v(&["b", "c"]), &[]), v(&["b"]));
    }

    #[test]
    fn kernel_modules_get_the_image_hint() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-image-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("kmod/lib/modules/7.3.3/extra"))?;
        std::fs::create_dir_all(dir.join("devel/usr/include"))?;
        std::fs::create_dir_all(dir.join("empty/usr/lib/modules"))?;
        let package = |name: &str| LockedPackage {
            name: name.into(),
            nevra: String::new(),
            repo: String::new(),
            sha256: name.into(),
            requested: Vec::new(),
        };
        std::fs::create_dir_all(dir.join("dep/usr/share/selinux/packages/x.pp"))?;
        let host = [
            package("kmod"),
            package("devel"),
            package("empty"),
            package("dep"),
        ];
        let found = image_only(&host, &|n| n != "dep", &|sha| dir.join(sha));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            image_hint(&found).to_string(),
            "kmod ships kernel modules, which only the image's kernel can load\n\
             Put it in your image instead:\n  RUN dnf install -y kmod"
        );
        Ok(())
    }
}
