//! `nx apply [file]`: makes the system match a manifest. With a file (one
//! copied from another machine, or `/var/lib/nx/current/manifest.toml` kept
//! in dotfiles) it replaces the manifest; without one it builds the current
//! manifest again. Both tiers resolve as `nx add` would, keeping what is
//! locked where it still fits, and the generation goes live through nxd.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use nx_core::{LockedTool, Manifest};
use nx_rpm::Refresh;

use crate::apply::Session;
use crate::host::{self, Machine};
use crate::tool::{self, Tier};
use crate::ui::{self, Stream, Ui};
use crate::upgrade::{self, Change};

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
    converge(ui, &mut session, dry_run, started)
}

fn read(path: &Path) -> Result<Manifest> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Manifest::parse(&text, path).context("not an nx manifest")
}

fn converge(ui: &mut Ui, session: &mut Session, dry_run: bool, started: Instant) -> Result<()> {
    let manifest = session.manifest.clone();
    let resolved = session.resolve_manifest(ui, &manifest.host.packages, &[])?;
    let before: BTreeMap<String, String> = session
        .current
        .iter()
        .flat_map(|g| g.lock.host.iter())
        .map(|p| (p.name.clone(), p.evr().to_owned()))
        .collect();
    let after: BTreeMap<String, String> = resolved
        .all
        .iter()
        .map(|p| (p.package.name.clone(), p.package.evr.clone()))
        .collect();
    let mut changes = upgrade::diff(&before, &after, "");

    let previous = session.tool.clone();
    let tools = if manifest.tool.packages.is_empty() {
        previous.as_ref().map(|t| LockedTool {
            packages: Vec::new(),
            ..t.clone()
        })
    } else {
        let tier = Tier::open(ui, previous.as_ref(), false)?;
        let system = tool::system(session.universe.arch());
        let locked = tier.resolve(ui, &manifest, previous.as_ref(), &system)?;
        if !dry_run {
            let plan = tier.plan(&locked, &manifest.caches)?;
            tier.realise(ui, &locked, &plan, &manifest, &system)?;
        }
        Some(locked)
    };
    let versions = |t: Option<&LockedTool>| -> BTreeMap<String, String> {
        t.iter()
            .flat_map(|t| t.packages.iter())
            .map(|p| (p.attr.clone(), p.version.clone()))
            .collect()
    };
    changes.extend(upgrade::diff(
        &versions(previous.as_ref()),
        &versions(tools.as_ref()),
        " · nixpkgs",
    ));
    session.fresh_tools = manifest
        .tool
        .packages
        .iter()
        .filter(|a| previous.as_ref().and_then(|p| p.package(a)).is_none())
        .cloned()
        .collect();
    session.tool = tools;

    let unchanged = changes.is_empty()
        && session
            .current
            .as_ref()
            .is_some_and(|g| g.manifest == manifest);
    if unchanged {
        ui.done("the system matches the manifest");
        return Ok(());
    }
    if dry_run {
        render(&mut ui.out, &changes, None, 0.0);
        return Ok(());
    }
    let built = session.install(ui, &manifest, &resolved.all)?;
    render(
        &mut ui.out,
        &changes,
        Some(built.generation.number),
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

fn render(out: &mut Stream, changes: &[Change], generation: Option<u32>, secs: f64) {
    upgrade::lines(out, changes);
    let style = out.style;
    let what = if changes.is_empty() {
        "manifest applied".to_owned()
    } else {
        format!("{} changed", ui::plural(changes.len(), "package"))
    };
    match generation {
        Some(n) => out.line(format_args!(
            "{} {what} · generation {n} in {secs:.1}s",
            style.green(ui::SYM_DONE)
        )),
        None => out.line(style.dim(&format!("{what} · dry run, nothing changed"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    #[test]
    fn lists_both_tiers_then_the_generation() {
        let changes = [
            Change::Added {
                name: "wayland-devel".into(),
                version: "1.24.0-1.fc45".into(),
            },
            Change::Added {
                name: "ripgrep".into(),
                version: "15.2.0 · nixpkgs".into(),
            },
        ];
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, &changes, Some(3), 4.0);
        render(&mut ui.out, &[], None, 0.0);
        assert_eq!(
            buf.text(),
            "  + wayland-devel 1.24.0-1.fc45\n\
             \x20 + ripgrep 15.2.0 · nixpkgs\n\
             \x20 ✓ 2 packages changed · generation 3 in 4.0s\n\
             \x20 manifest applied · dry run, nothing changed\n"
        );
    }

    #[test]
    fn refuses_a_file_that_is_not_a_manifest() {
        let path = std::env::temp_dir().join(format!("nx-apply-{}.toml", std::process::id()));
        let _ = std::fs::write(&path, "[host\n");
        let err = read(&path).map(|_| ()).map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&path);
        assert_eq!(err, Err("not an nx manifest".to_owned()));
    }
}
