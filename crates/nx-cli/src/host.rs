//! The host tier's view of this machine: which image, which repos, where
//! the metadata cache and nx's state live. Shared by every host-tier
//! command.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nx_core::{
    Deployment, DeploymentSource, Deployments, Fallback, Generation, Journal, Manifest, OsRelease,
    RepoEntry, State,
};
use nx_rpm::repo::{Added, Repo};
use nx_rpm::{Freshness, Options, Refresh, Universe, Ureq, Vars};

use crate::ui::Ui;

/// The running system as the host tier reads it, once per command: its
/// root, the image's os-release, the deployments and the architecture.
/// Commands read `/`; tests build one from a fixture root.
pub struct Machine {
    pub root: PathBuf,
    pub release: OsRelease,
    /// `None` off a bootc system.
    deployments: Option<Deployments>,
    /// The image the host tier is resolved for: the booted one, or the one
    /// a deployment [`targeting`](Machine::targeting) names.
    digest: Option<String>,
    pub arch: String,
}

impl Machine {
    /// Reads the system under `root`. On a bootc system (ostree's
    /// `run/ostree-booted` marker) the deployments must be readable: a
    /// lockfile without the image digest could not be bound to the image.
    pub fn read(root: &Path, source: &dyn DeploymentSource) -> Result<Self> {
        let release = OsRelease::read(root).context("reading the image's os-release")?;
        let deployments = if root.join("run/ostree-booted").exists() {
            Some(
                source
                    .deployments()
                    .context("reading the image's deployments")?,
            )
        } else {
            None
        };
        let digest = deployments
            .as_ref()
            .and_then(|d| d.booted()?.image_digest.clone());
        Ok(Self {
            root: root.to_owned(),
            release,
            deployments,
            digest,
            arch: std::env::consts::ARCH.to_owned(),
        })
    }

    /// This machine as `deployment`'s image will see it: its tree is the
    /// root (repo files, rpmdb, os-release and keys come from it) and its
    /// digest is the one generations are keyed by. For re-resolving the host
    /// tier for a staged or rollback image before it boots.
    pub fn targeting(&self, deployment: &Deployment) -> Result<Self> {
        let root = deployment.root(&self.root.join("sysroot"));
        if !root.join("usr").is_dir() {
            bail!("can't read the deployment at {}", root.display());
        }
        let release = OsRelease::read(&root)
            .with_context(|| format!("reading the os-release of {}", root.display()))?;
        Ok(Self {
            root,
            release,
            deployments: None,
            digest: deployment.image_digest.clone(),
            arch: self.arch.clone(),
        })
    }

    /// The deployments on a bootc system.
    pub fn deployments(&self) -> Option<&Deployments> {
        self.deployments.as_ref()
    }

    /// The image's digest; `None` off a bootc system. Outside one, the
    /// installed set is read from the rpmdb each time instead of from a
    /// cache.
    pub fn digest(&self) -> Option<String> {
        self.digest.clone()
    }

    /// The booted image's own root, for checking what the image ships: the
    /// deployment's tree under `/sysroot`, which no extension overlays, or
    /// the root itself off a bootc system. Never `/` on a bootc system,
    /// where `/usr` shows the merged extension too.
    pub fn image_root(&self) -> Result<PathBuf> {
        let Some(deployments) = &self.deployments else {
            return Ok(self.root.clone());
        };
        let booted = deployments
            .booted()
            .context("rpm-ostree reports no booted deployment")?;
        let root = booted.root(&self.root.join("sysroot"));
        if !root.join("usr").is_dir() {
            bail!("can't read the booted deployment at {}", root.display());
        }
        Ok(root)
    }

    pub fn vars(&self) -> Vars {
        Vars {
            releasever: self.release.version_id.clone(),
            basearch: self.arch.clone(),
        }
    }

    /// Resolver options for this machine, with metadata cached in `cache`.
    pub fn options(&self, refresh: Refresh, cache: PathBuf) -> Options {
        Options {
            root: self.root.clone(),
            cache,
            image_digest: self.digest(),
            vars: self.vars(),
            arch: self.arch.clone(),
            refresh,
            extra: Vec::new(),
        }
    }
}

/// Where repo metadata and downloads are cached: the cache all
/// administrators share (`/var/lib/nx/cache`) when this user can write it,
/// so a second administrator's first change doesn't download the metadata
/// again; otherwise their own, `$XDG_CACHE_HOME/nx` or `~/.cache/nx`.
pub fn cache_dir() -> Result<PathBuf> {
    cache_dir_for(&state())
}

/// [`cache_dir`] for the state directory `state`.
pub fn cache_dir_for(state: &State) -> Result<PathBuf> {
    if let Some(shared) = state.shared_cache() {
        return Ok(shared);
    }
    user_cache_dir()
}

fn user_cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("nx"));
    }
    let home = std::env::var_os("HOME").context("neither XDG_CACHE_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".cache/nx"))
}

/// nx's state directory: `NX_STATE_DIR`, else `/var/lib/nx`. Until nxd
/// (which owns `/var/lib/nx`) exists, changing state needs write access to
/// it; the variable points a VM test or a user at another directory.
pub fn state() -> State {
    let root = std::env::var_os("NX_STATE_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(
            || PathBuf::from(nx_core::state::DEFAULT_ROOT),
            PathBuf::from,
        );
    State::new(root)
}

/// Whether `NX_STATE_DIR` points nx at another state directory. nxd only
/// serves `/var/lib/nx`, so then nothing can be activated.
pub fn state_overridden() -> bool {
    std::env::var_os("NX_STATE_DIR").is_some_and(|d| !d.is_empty())
}

/// Whether the tree at `root` has a command `name` in one of the
/// directories on `PATH` ([`nx_rpm::COMMAND_DIRS`]).
pub fn has_command(root: &Path, name: &str) -> bool {
    nx_rpm::COMMAND_DIRS
        .iter()
        .any(|d| std::fs::symlink_metadata(root.join(d).join(name)).is_ok())
}

/// Where `nx repo add` pins third-party keys.
pub fn keys_dir(state: &State) -> PathBuf {
    state.root().join("keys")
}

/// The generation read-only commands show (design invariant 9): the
/// current one or, when a newer nx wrote it, the newest one this nx reads,
/// with a warning.
pub fn readable(ui: &mut Ui, state: &State) -> Result<Option<Generation>> {
    let snap = Generation::readable(state).context("reading the current generation")?;
    if let Some(n) = snap.newer {
        match &snap.generation {
            Some(g) => ui.warn(format_args!(
                "generation {n} was written by a newer nx; showing generation {}",
                g.number
            )),
            None => ui.warn(format_args!(
                "generation {n} was written by a newer nx, and no older one is readable"
            )),
        }
    }
    Ok(snap.generation)
}

/// A manifest entry as the repo loader takes it.
pub fn added(e: &RepoEntry) -> Added<'_> {
    Added {
        name: &e.name,
        baseurl: e.baseurl.as_deref(),
        metalink: e.metalink.as_deref(),
        gpgkey: e.gpgkey.as_deref(),
        priority: e.priority,
    }
}

/// The repos a manifest adds.
pub fn added_repos(manifest: &Manifest, vars: &Vars) -> Result<Vec<Repo>> {
    manifest
        .repos
        .iter()
        .map(|r| {
            Repo::added(&added(r), vars).with_context(|| format!("repo {} in the manifest", r.name))
        })
        .collect()
}

/// Loads the universe for `machine` and the repos `manifest` adds,
/// telling the user about downloads, out-of-date metadata and repos that
/// could not be loaded.
pub fn load(
    ui: &mut Ui,
    machine: &Machine,
    refresh: Refresh,
    manifest: &Manifest,
) -> Result<Universe> {
    load_from(
        ui,
        machine,
        refresh,
        manifest,
        &Ureq::new(),
        &Journal::system("nx"),
        cache_dir()?,
    )
}

/// [`load`] through `http`, with metadata cached in `cache`.
pub fn load_from(
    ui: &mut Ui,
    machine: &Machine,
    refresh: Refresh,
    manifest: &Manifest,
    http: &dyn nx_rpm::Http,
    journal: &Journal,
    cache: PathBuf,
) -> Result<Universe> {
    let mut opts = machine.options(refresh, cache);
    opts.extra = added_repos(manifest, &opts.vars)?;
    let err = &mut ui.err;
    let universe = Universe::load(&opts, http, &mut |repo| {
        let sym = err.style.cyan(crate::ui::SYM_FETCH);
        err.line(format_args!("{sym} {repo} metadata"));
    })
    .context("loading the host tier's repos")?;
    for skipped in &universe.skipped {
        ui.warn(format_args!(
            "skipped repo {}: {}",
            skipped.repo, skipped.error
        ));
    }
    for loaded in &universe.repos {
        if let Freshness::OtherMirror(error) = &loaded.synced.freshness {
            journal.fallback(
                Fallback::OtherMirror,
                &format!(
                    "{} metadata from {}: {error}",
                    loaded.repo.id, loaded.synced.base
                ),
            );
        }
    }
    let mut expired = Vec::new();
    for loaded in universe.stale() {
        match &loaded.synced.freshness {
            Freshness::Unreachable(error) => {
                ui.warn(format_args!(
                    "using cached metadata for {}: {error}",
                    loaded.repo.id
                ));
                journal.fallback(
                    Fallback::CachedMetadata,
                    &format!("cached metadata for {}: {error}", loaded.repo.id),
                );
            }
            _ => expired.push(loaded.repo.id.as_str()),
        }
    }
    if !expired.is_empty() {
        ui.warn(format_args!(
            "metadata for {} is out of date; --refresh updates it",
            expired.join(", ")
        ));
    }
    Ok(universe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str, bootc: bool) -> std::io::Result<PathBuf> {
        let root = std::env::temp_dir().join(format!("nx-host-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("usr/lib"))?;
        std::fs::write(
            root.join("usr/lib/os-release"),
            "ID=fedora\nVERSION_ID=45\n",
        )?;
        if bootc {
            std::fs::create_dir_all(root.join("run"))?;
            std::fs::write(root.join("run/ostree-booted"), "")?;
        }
        Ok(root)
    }

    struct Failing;
    impl DeploymentSource for Failing {
        fn deployments(&self) -> nx_core::Result<Deployments> {
            Err(nx_core::Error::Invalid("rpm-ostree is broken".into()))
        }
    }

    #[test]
    fn off_bootc_the_root_is_the_image() -> Result<()> {
        let root = fixture("plain", false)?;
        let m = Machine::read(&root, &Failing)?;
        assert_eq!(m.digest(), None);
        assert_eq!(m.image_root()?, root);
        assert_eq!(m.vars().releasever, "45");
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn on_bootc_the_deployments_must_be_readable() -> Result<()> {
        let root = fixture("bootc", true)?;
        assert!(Machine::read(&root, &Failing).is_err());
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
