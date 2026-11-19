//! The host tier's view of this machine: which image, which repos, where
//! the metadata cache lives. Shared by `search`, `info` and `add`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nx_core::{DeploymentSource, OsRelease, RpmOstree};
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
    })
}

/// Loads the universe for the running system, telling the user about
/// downloads, out-of-date metadata and repos that could not be loaded.
pub fn load(ui: &mut Ui, refresh: Refresh) -> Result<Universe> {
    let opts = options(Path::new("/"), &RpmOstree, refresh)?;
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
