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
use nx_rpm::{Package, Plan, Planned, Refresh, Relative, Repo, Universe, Ureq};

use crate::changes::{self, Change};
use crate::host::{self, Machine};
use crate::keys::Keys;
use crate::tool::{self, Tier};
use crate::ui::{self, Ui};

/// Downloads at once; Fedora's mirrors are fine with a handful.
const THREADS: usize = 4;

/// Room left over after a change, so a full disk never stops the next one
/// half-way.
const SPARE: u64 = 32 << 20;

/// What a session reaches the system through: the state directory, HTTP
/// (repo metadata, packages, Koji), `nix`, and what makes a generation
/// live. Commands use [`Env::system`]; tests a temporary state, canned
/// answers and a `current` that just moves.
pub struct Env {
    pub state: State,
    pub http: Box<dyn nx_rpm::Http>,
    pub nix: Box<dyn nx_nix::Nix>,
    pub live: Box<dyn crate::live::Live>,
}

impl Env {
    /// `/var/lib/nx` (or `NX_STATE_DIR`), the network, the image's `nix`
    /// and nxd.
    pub fn system() -> Self {
        Self {
            state: host::state(),
            http: Box::new(Ureq::new()),
            nix: Box::new(nx_nix::System),
            live: Box::new(crate::live::Nxd),
        }
    }
}

/// One change to the packages, from the lock to the live generation:
/// [`Session::open`] (the manifest changed under the state lock), then
/// [`Session::plan`], then [`Session::install`] or [`finish`].
pub struct Session<'m> {
    machine: &'m Machine,
    env: Env,
    /// Held unless this is a dry run.
    guard: Option<state::Lock>,
    current: Option<Generation>,
    /// The current manifest, changed as the command asked; the repos were
    /// loaded for it.
    manifest: Manifest,
    universe: Universe,
    keys: Keys,
    /// Koji `name.arch` → the repo whose keys verified it.
    koji_repo: HashMap<String, String>,
    /// Manifest packages the next generation leaves out ([`Session::leave_out`]).
    skipped: Vec<String>,
    /// The next generation's tool tier: the current one's until
    /// [`Session::plan`] resolves another.
    tool: Option<LockedTool>,
    /// The tool tier at the pinned release, opened when first needed.
    tier: Option<Tier>,
    /// Why each manifest entry is in its tier (`nx why`): the current
    /// generation's, plus what this command routed.
    routes: BTreeMap<String, String>,
    /// Resolve without preferring the current generation's builds
    /// (`nx upgrade`).
    unlocked: bool,
    dry_run: bool,
}

/// How [`Session::plan`] treats the two tiers.
#[derive(Debug, Default)]
pub struct How<'a> {
    /// What the command names: it must resolve, never left out.
    pub asked: &'a [String],
    /// Host packages may move to newer builds (`nx upgrade --host`).
    pub newer_host: bool,
    /// The nixpkgs pin moves to the channel's current release (`nx upgrade
    /// --tool`).
    pub newer_tools: bool,
}

/// What a change will do: the host tier to build, and both tiers' changes.
pub struct Outcome {
    pub all: Vec<Planned>,
    /// Host packages the current generation doesn't have.
    pub new_host: Vec<Package>,
    pub changes: Vec<Change>,
    /// The nixpkgs release the tool tier moved to.
    pub moved: Option<String>,
    /// What realising the tool tier downloads.
    pub tool_download: u64,
}

impl Outcome {
    /// Everything downloaded: new host packages and the tool tier.
    pub fn download(&self) -> u64 {
        self.new_host.iter().map(|p| p.download_size).sum::<u64>() + self.tool_download
    }
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
        Self::open_in(Env::system(), ui, machine, refresh, dry_run, change)
    }

    /// [`Session::open`] through `env`.
    pub fn open_in(
        env: Env,
        ui: &mut Ui,
        machine: &'m Machine,
        refresh: Refresh,
        dry_run: bool,
        change: impl FnOnce(&mut Manifest) -> Result<()>,
    ) -> Result<Self> {
        let state = &env.state;
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
        let current = match Generation::current(state) {
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
        let cache = host::cache_dir_for(state)?;
        let universe = host::load_from(ui, machine, refresh, &manifest, &*env.http, cache)?;
        let keys = Keys::new(machine, state, current.as_ref(), &manifest);
        let tool = current.as_ref().and_then(|g| g.lock.tool.clone());
        let routes = current
            .as_ref()
            .map(|g| g.lock.routes.clone())
            .unwrap_or_default();
        Ok(Self {
            machine,
            env,
            guard,
            current,
            manifest,
            universe,
            keys,
            koji_repo: HashMap::new(),
            skipped: Vec::new(),
            tool,
            tier: None,
            routes,
            unlocked: false,
            dry_run,
        })
    }

    pub fn state(&self) -> &State {
        &self.env.state
    }

    /// The generation the change builds on.
    pub fn current(&self) -> Option<&Generation> {
        self.current.as_ref()
    }

    /// The manifest as the command changed it.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Changes the manifest further, under the lock the session holds. Its
    /// repos were loaded when the session opened, so they can't change
    /// here; that is [`Session::open`]'s `change`.
    pub fn edit(&mut self, change: impl FnOnce(&mut Manifest)) -> Result<()> {
        let repos = self.manifest.repos.clone();
        change(&mut self.manifest);
        if self.manifest.repos != repos {
            bail!("the repos changed after they were loaded");
        }
        Ok(())
    }

    /// The image and the repos the manifest names.
    pub fn universe(&self) -> &Universe {
        &self.universe
    }

    /// The next generation's tool tier as planned so far.
    pub fn tool(&self) -> Option<&LockedTool> {
        self.tool.as_ref()
    }

    /// Records why `entry` is in its tier, for `nx why`.
    pub fn route(&mut self, entry: &str, why: String) {
        self.routes.insert(entry.to_owned(), why);
    }

    /// The tool tier at the pinned release (the channel's current one when
    /// nothing is pinned yet), opened once.
    pub fn tier(&mut self, ui: &mut Ui) -> Result<&mut Tier> {
        let tier = match self.tier.take() {
            Some(t) => t,
            None => Tier::open(ui, self.tool.as_ref(), false)?,
        };
        Ok(self.tier.insert(tier))
    }

    /// Resolves both tiers for the session's manifest, as `how` says, and
    /// (unless a dry run) realises the tool tier. Nothing is built yet:
    /// [`Session::install`] or [`finish`] does that.
    pub fn plan(&mut self, ui: &mut Ui, how: &How) -> Result<Outcome> {
        self.unlocked = how.newer_host;
        let specs = self.manifest.host.packages.clone();
        let resolved = self.resolve_manifest(ui, &specs, how.asked)?;
        let mut changes = crate::changes::host(self.locked(), &resolved.all);
        let before = self.tool.clone();
        let (moved, tool_download) = if how.newer_tools {
            self.upgrade_tools(ui)?
        } else {
            (None, self.resolve_tools(ui)?)
        };
        changes.extend(crate::changes::tools(before.as_ref(), self.tool.as_ref()));
        Ok(Outcome {
            all: resolved.all,
            new_host: resolved.new,
            changes,
            moved,
            tool_download,
        })
    }

    /// The manifest's tool tier at the pinned release: what the lock has
    /// stays as it is, what it lacks is evaluated and realised. Returns
    /// what realising downloads.
    fn resolve_tools(&mut self, ui: &mut Ui) -> Result<u64> {
        let previous = self.tool.clone();
        if let Some(kept) = without_new(previous.as_ref(), &self.manifest.tool.packages) {
            self.tool = kept;
            return Ok(0);
        }
        if !tool::available() {
            bail!(
                "the tool tier needs Nix in the image, and {} is missing",
                nx_nix::STORE
            );
        }
        let system = tool::system(self.universe.arch());
        let manifest = self.manifest.clone();
        let dry_run = self.dry_run;
        self.tier(ui)?;
        let (Some(tier), nix) = (self.tier.as_ref(), &*self.env.nix) else {
            bail!("the nixpkgs release didn't open");
        };
        let locked = tier.resolve(ui, nix, &manifest, previous.as_ref(), &system)?;
        let plan = tier.plan(&locked, &manifest.caches)?;
        if !dry_run {
            tier.realise(ui, nix, &locked, &plan, &manifest, &system)?;
        }
        self.tool = Some(locked);
        Ok(plan.sum.download)
    }

    /// The tool tier at the channel's current release: every package
    /// evaluated there, or kept as it is when the release no longer has it.
    /// At an unchanged pin only paths the store lost are done again.
    /// Returns the release moved to and what realising downloads.
    fn upgrade_tools(&mut self, ui: &mut Ui) -> Result<(Option<String>, u64)> {
        if !tool::available() || self.manifest.tool.packages.is_empty() {
            return Ok((None, self.resolve_tools(ui)?));
        }
        let tier = Tier::open(ui, self.tool.as_ref(), true)?;
        let same_pin =
            self.tool.as_ref().map(|t| t.rev.as_str()) == Some(tier.release.rev.as_str());
        let lost = self.tool.as_ref().is_some_and(tool::incomplete);
        if same_pin && !lost {
            return Ok((None, self.resolve_tools(ui)?));
        }
        let system = tool::system(self.universe.arch());
        let manifest = self.manifest.clone();
        let (locked, held) = tier.resolve_holding(
            ui,
            &*self.env.nix,
            &manifest,
            None,
            self.tool.as_ref(),
            &system,
        )?;
        if !held.is_empty() {
            ui.warn(format_args!(
                "nixpkgs {} has no usable {}: keeping the build you have (nx remove {} to drop it)",
                tier.release.version(),
                held.join(", "),
                if held.len() == 1 { "it" } else { "them" }
            ));
        }
        let mut download = 0;
        if !self.dry_run {
            let plan = tier.plan(&locked, &manifest.caches)?;
            download = plan.sum.download;
            tier.realise(ui, &*self.env.nix, &locked, &plan, &manifest, &system)?;
        }
        let version = tier.release.version();
        let moved = if same_pin {
            format!("{version}, realised again")
        } else {
            version.to_owned()
        };
        self.tool = Some(locked);
        Ok((Some(moved), download))
    }

    /// Accepts the keys now pinned for `ids`, though they differ from what
    /// the current generation recorded: the user confirmed them.
    pub fn trust(&mut self, ids: &[String]) {
        self.keys.trust(ids);
    }

    /// Checks repo `id`'s packages against the key pinned as `sha` (one
    /// this command fetched and showed), not the current generation's.
    pub fn choose_key(&mut self, id: &str, sha: String) {
        self.keys.choose(id, sha);
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
        let cache = host::cache_dir_for(&self.env.state)?.join("koji");
        let (http, keys, koji_repo) = (&*self.env.http, &mut self.keys, &mut self.koji_repo);
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
    pub fn build(&mut self, ui: &mut Ui, all: &[Planned]) -> Result<Built> {
        // Held until the session ends, so `install` activates under it too.
        let guard = self.guard.take().context("a dry run cannot build")?;
        let manifest = self.manifest.clone();
        let built = self.build_locked(ui, &guard, &manifest, all);
        self.guard = Some(guard);
        built
    }

    fn build_locked(
        &mut self,
        ui: &mut Ui,
        guard: &state::Lock,
        manifest: &Manifest,
        all: &[Planned],
    ) -> Result<Built> {
        let asked = |name: &str| {
            manifest
                .host
                .packages
                .iter()
                .any(|s| s.split('@').next() == Some(name))
        };
        // What the manifest names first: an image-only package is refused
        // before its dependencies are downloaded.
        let (first, rest): (Vec<Planned>, Vec<Planned>) =
            all.iter().cloned().partition(|p| asked(&p.package.name));
        let mut files = self.fetch(ui, &first)?;
        let mut host = self.unpack(guard, &first, &mut files)?;
        let store = self.env.state.store();
        let image_only = image_only(&host, &asked, &|sha| store.path(sha));
        if !image_only.is_empty() {
            return Err(image_hint(&image_only));
        }
        let mut files = self.fetch(ui, &rest)?;
        host.extend(self.unpack(guard, &rest, &mut files)?);
        // In the plan's order, as before the split.
        let mut by_nevra: HashMap<String, LockedPackage> =
            host.into_iter().map(|p| (p.nevra.clone(), p)).collect();
        let host: Vec<LockedPackage> = all
            .iter()
            .filter_map(|p| by_nevra.remove(&p.package.nevra()))
            .collect();
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
            Some(crate::tool::shim(&self.env.state)?)
        } else {
            None
        };
        let built = generation::build(
            &self.env.state,
            guard,
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
        crate::tool::add_roots(&*self.env.nix, &built.generation)?;
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
            && let Err(e) = crate::tool::restore(ui, &*self.env.nix, &tool, &manifest.caches)
        {
            ui.warn(format_args!(
                "tool-tier commands whose Nix paths are gone are left out until nx upgrade --tool: {e:#}"
            ));
        }
        let store = self.env.state.store();
        let owner = |name: &str| -> Option<String> {
            if host::has_command(image_root, name) {
                return Some("the image".to_owned());
            }
            host.iter()
                .find(|p| host::has_command(&store.path(&p.sha256), name))
                .map(|p| format!("the host tier's {}", p.name))
        };
        let exposure = crate::tool::expose(
            &mut tool,
            &owner,
            &manifest.overrides.names,
            &crate::tool::commands,
        );
        let previous = self.current.as_ref().and_then(|g| g.lock.tool.as_ref());
        // Packages this change adds: their commands must not clash.
        let fresh: Vec<String> = tool
            .packages
            .iter()
            .filter(|p| previous.and_then(|t| t.package(&p.attr)).is_none())
            .map(|p| p.attr.clone())
            .collect();
        crate::tool::check(
            ui,
            &tool,
            &exposure,
            &fresh,
            previous,
            &manifest.overrides.without,
        )?;
        Ok(Some(tool))
    }

    /// [`Session::build`], then makes the generation live through nxd. A
    /// generation nxd refuses stays on disk, not current. The state lock is
    /// held until `current` has moved: a command that waited on it must
    /// build on this generation, or this change is lost. The staged and
    /// rollback images get theirs too (design invariant 6), best effort:
    /// the change is live either way.
    pub fn install(&mut self, ui: &mut Ui, all: &[Planned]) -> Result<Built> {
        let built = self.build(ui, all)?;
        self.env.live.activate(
            ui,
            &self.env.state,
            built.generation.number,
            self.manifest.host.backend,
        )?;
        for name in &built.allowed {
            ui.warn(format_args!(
                "{name}: scriptlets nx can't translate are skipped, as you allowed"
            ));
        }
        for nevra in &built.outside_usr {
            ui.warn(format_args!(
                "{nevra} has files outside /usr, which the extension leaves out"
            ));
        }
        // The sync below takes the lock itself, once per image.
        self.guard = None;
        if let Err(e) = crate::sync::deployed(ui, self.machine, &self.env.state, Refresh::IfMissing)
        {
            ui.warn(format_args!(
                "the staged and rollback images were not updated: {e:#} (nx sync retries)"
            ));
        }
        Ok(built)
    }

    /// Downloads what the store lacks, after checking there is room for it
    /// and for its unpacked files. Returns each package's file by NEVRA.
    fn fetch(&self, ui: &mut Ui, all: &[Planned]) -> Result<HashMap<String, PathBuf>> {
        let store = self.env.state.store();
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
        let dir = host::cache_dir_for(&self.env.state)?.join("packages");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        check_space(
            &dir,
            jobs.iter().map(|j| j.package.download_size).sum(),
            self.env.state.root(),
            missing.iter().map(|p| p.package.install_size).sum(),
        )?;
        let files = download(ui, &*self.env.http, &jobs, &dir)?;
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
        let store = self.env.state.store();
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
                key: self.keys.pin_of(&l.repo.id),
            });
        }
        Ok(lock)
    }
}

/// The tool tier for `attrs` when the lock has every one of them: the lock
/// less what left, with nothing to evaluate. `None` when something is new.
fn without_new(previous: Option<&LockedTool>, attrs: &[String]) -> Option<Option<LockedTool>> {
    if attrs
        .iter()
        .any(|a| previous.and_then(|p| p.package(a)).is_none())
    {
        return None;
    }
    Some(previous.map(|p| {
        LockedTool {
            packages: p
                .packages
                .iter()
                .filter(|t| attrs.contains(&t.attr))
                .cloned()
                .collect(),
            ..p.clone()
        }
    }))
}

/// Builds what `outcome` planned and makes it live (unless a dry run),
/// then reports both tiers' changes: the end of `nx remove`, `nx upgrade`
/// and `nx apply`.
pub fn finish(
    ui: &mut Ui,
    session: &mut Session,
    outcome: &Outcome,
    started: std::time::Instant,
) -> Result<()> {
    let generation = if session.dry_run {
        None
    } else {
        Some(session.install(ui, &outcome.all)?.generation.number)
    };
    changes::lines(&mut ui.out, &outcome.changes);
    changes::summary(
        &mut ui.out,
        &outcome.changes,
        &changes::Done {
            generation,
            moved: outcome.moved.as_deref(),
            download: outcome.download(),
            secs: started.elapsed().as_secs_f64(),
        },
    );
    Ok(())
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
fn download(
    ui: &mut Ui,
    http: &dyn nx_rpm::Http,
    jobs: &[Job],
    dir: &Path,
) -> Result<Vec<PathBuf>> {
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
mod session_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_only_when_the_manifest_names_something_new() {
        let lock = LockedTool {
            rev: "r".into(),
            packages: ["ripgrep", "jq"]
                .iter()
                .map(|a| nx_core::ToolPackage {
                    attr: (*a).into(),
                    ..nx_core::ToolPackage::default()
                })
                .collect(),
            ..LockedTool::default()
        };
        let attrs = |l: Option<LockedTool>| -> Vec<String> {
            l.map(|l| l.packages.into_iter().map(|p| p.attr).collect())
                .unwrap_or_default()
        };
        // A removal keeps the rest as it was, the pin too.
        let kept = without_new(Some(&lock), &["jq".into()]);
        assert_eq!(kept.clone().map(attrs), Some(vec!["jq".to_owned()]));
        assert_eq!(kept.flatten().map(|l| l.rev).as_deref(), Some("r"));
        // Nothing asked: an empty tier at the same pin; nothing ever: none.
        assert_eq!(without_new(Some(&lock), &[]).map(attrs), Some(vec![]));
        assert!(matches!(without_new(None, &[]), Some(None)));
        // Something new needs the release.
        assert!(without_new(Some(&lock), &["jq".into(), "fd".into()]).is_none());
        assert!(without_new(None, &["fd".into()]).is_none());
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
