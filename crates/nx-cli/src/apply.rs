//! `nx apply [file]`: makes the system match a manifest. With a file (one
//! copied from another machine, or `/var/lib/nx/current/manifest.toml` kept
//! in dotfiles) it replaces the manifest; without one it builds the current
//! manifest again. Both tiers resolve as `nx add` would, keeping what is
//! locked where it still fits, and the generation goes live through nxd.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use nx_core::Manifest;
use nx_rpm::Refresh;

use crate::host::{self, Machine};
use crate::session::{self, How, Session};
use crate::tool;
use crate::ui::Ui;

pub fn run(
    ui: &mut Ui,
    machine: &Machine,
    file: Option<&Path>,
    dry_run: bool,
    refresh: Refresh,
) -> Result<()> {
    let started = Instant::now();
    let wanted = file.map(read).transpose()?;
    if wanted
        .as_ref()
        .is_some_and(|w| !w.tool.packages.is_empty() && !tool::available())
    {
        bail!(
            "the manifest has nixpkgs tools, and the image has no Nix ({} is missing)",
            nx_nix::STORE
        );
    }
    // Keys are fetched (and shown) before the lock, as `nx repo add` does,
    // and pinned under it.
    let current = host::readable(ui, &host::state())?;
    let unpinned = wanted
        .as_ref()
        .map(|w| crate::repo::fetch_unpinned(ui, machine, w, current.as_ref()))
        .transpose()?;
    let mut session = Session::open(ui, machine, refresh, dry_run, |m| {
        if let Some(w) = &wanted {
            *m = w.clone();
        }
        Ok(())
    })?;
    if let Some(u) = unpinned.filter(|_| !dry_run) {
        u.pin(&mut session)?;
    }
    let outcome = session.plan(ui, &How::default())?;
    let unchanged = outcome.changes.is_empty()
        && session
            .current()
            .is_some_and(|g| g.manifest == *session.manifest());
    if unchanged {
        ui.done("the system matches the manifest");
        return Ok(());
    }
    session::finish(ui, &mut session, &outcome, started)
}

fn read(path: &Path) -> Result<Manifest> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Manifest::parse(&text, path).context("not an nx manifest")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_file_that_is_not_a_manifest() {
        let path = std::env::temp_dir().join(format!("nx-apply-{}.toml", std::process::id()));
        let _ = std::fs::write(&path, "[host\n");
        let err = read(&path).map(|_| ()).map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&path);
        assert_eq!(err, Err("not an nx manifest".to_owned()));
    }
}
