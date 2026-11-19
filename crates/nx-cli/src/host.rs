//! The host tier's view of this machine: which image, which repos, where
//! the metadata cache and nx's state live. Shared by every host-tier
//! command.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nx_core::{DeploymentSource, Manifest, OsRelease, RpmOstree, State};
use nx_rpm::repo::{Added, Repo};
use nx_rpm::{Freshness, Options, Refresh, Universe, Ureq, Vars};

use crate::ui::Ui;

/// nx's cache for unprivileged commands: `$XDG_CACHE_HOME/nx`, else
/// `~/.cache/nx`. nxd keeps its own under `/var/lib/nx/cache` (M3).
pub fn cache_dir() -> Result<PathBuf> {
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

/// Where `nx repo add` pins third-party keys.
pub fn keys_dir(state: &State) -> PathBuf {
    state.root().join("keys")
}

/// The booted image's own root, for checking what the image ships: the
/// deployment's tree under `/sysroot`, which no extension overlays, or `/`
/// off a bootc system.
pub fn image_root(source: &dyn DeploymentSource) -> PathBuf {
    source
        .deployments()
        .ok()
        .and_then(|d| d.booted().map(|b| b.root(Path::new("/sysroot"))))
        .filter(|r| r.join("usr").is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The repos a manifest adds.
pub fn added_repos(manifest: &Manifest, vars: &Vars) -> Result<Vec<Repo>> {
    manifest
        .repos
        .iter()
        .map(|r| {
            Repo::added(
                &Added {
                    name: &r.name,
                    baseurl: r.baseurl.as_deref(),
                    metalink: r.metalink.as_deref(),
                    gpgkey: r.gpgkey.as_deref(),
                    priority: r.priority,
                },
                vars,
            )
            .with_context(|| format!("repo {} in the manifest", r.name))
        })
        .collect()
}

/// Resolver options for the booted system under `root`.
pub fn options(root: &Path, source: &dyn DeploymentSource, refresh: Refresh) -> Result<Options> {
    let release = OsRelease::read(root).context("reading the image's os-release")?;
    // Outside a bootc system there is no digest; the installed set is then
    // read from the rpmdb each time instead of from a cache.
    let image_digest = source
        .deployments()
        .ok()
        .and_then(|d| d.booted().and_then(|b| b.image_digest.clone()));
    let arch = std::env::consts::ARCH.to_owned();
    Ok(Options {
        root: root.to_owned(),
        cache: cache_dir()?,
        image_digest,
        vars: Vars {
            releasever: release.version_id.clone(),
            basearch: arch.clone(),
        },
        arch,
        refresh,
        extra: Vec::new(),
    })
}

/// Loads the universe for the running system and the repos `manifest`
/// adds, telling the user about downloads, out-of-date metadata and repos
/// that could not be loaded.
pub fn load(ui: &mut Ui, refresh: Refresh, manifest: &Manifest) -> Result<Universe> {
    let mut opts = options(Path::new("/"), &RpmOstree, refresh)?;
    opts.extra = added_repos(manifest, &opts.vars)?;
    let err = &mut ui.err;
    let universe = Universe::load(&opts, &Ureq::new(), &mut |repo| {
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
    let mut expired = Vec::new();
    for loaded in universe.stale() {
        match &loaded.synced.freshness {
            Freshness::Unreachable(error) => ui.warn(format_args!(
                "using cached metadata for {}: {error}",
                loaded.repo.id
            )),
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

/// The current generation's manifest, or an empty one.
pub fn current_manifest(state: &State) -> Result<Manifest> {
    Ok(nx_core::Generation::current(state)
        .context("reading the current generation")?
        .map(|g| g.manifest)
        .unwrap_or_default())
}
