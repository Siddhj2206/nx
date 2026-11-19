//! `nx gc`: removes generations nothing needs and the store entries only
//! they used. Kept: the current generation and its parent, where `nx
//! rollback` goes, the newest for each deployed image (booted, staged and
//! rollback: design invariant 5) and the manifest's `gc.keep` newest (5 by
//! default). Then `nix store gc`: a
//! removed generation's GC roots went with it, so its Nix paths go too,
//! after the kept generations that lack roots (one `nx select` made at
//! boot, before the daemon ran) get theirs.

use anyhow::{Context, Result};
use nx_core::{Generation, State, select};

use crate::host::{self, Machine};
use crate::tool;
use crate::ui::{self, Stream, Ui};

/// `nx sync`'s tail: removes what `nx gc` would, saying so only when
/// something went. With `nix` (the daily `nx sync --refresh`) the Nix
/// store is collected too: old tool tiers (the weekly upgrade leaves one
/// each time) would otherwise stay until someone runs `nx gc`. Returns how
/// many generations were removed.
pub fn collect(ui: &mut Ui, machine: &Machine, nix: bool) -> Result<usize> {
    let (dropped, packages, keep) = remove(machine, false)?;
    if !dropped.is_empty() || packages > 0 {
        render(&mut ui.out, &dropped, packages, false);
    }
    if nix && tool::available() {
        // The host tier is done; a Nix failure is only a warning here.
        if let Err(e) = collect_nix(ui, &nx_nix::System, &host::state(), &keep) {
            ui.warn(format_args!("the Nix store was not collected: {e:#}"));
        }
    }
    Ok(dropped.len())
}

pub fn run(ui: &mut Ui, machine: &Machine, dry_run: bool) -> Result<()> {
    let (dropped, packages, keep) = remove(machine, dry_run)?;
    render(&mut ui.out, &dropped, packages, dry_run);
    if !dry_run && tool::available() {
        collect_nix(ui, &nx_nix::System, &host::state(), &keep)?;
    }
    Ok(())
}

/// Roots what kept generations hold, then lets Nix delete the rest.
fn collect_nix(ui: &mut Ui, nix: &dyn nx_nix::Nix, state: &State, keep: &[u32]) -> Result<()> {
    for n in keep {
        let Ok(g) = Generation::read(state, *n) else {
            continue;
        };
        let Some(t) = g.lock.tool.as_ref().filter(|t| !t.packages.is_empty()) else {
            continue;
        };
        if !tool::rooted(&g) {
            tool::root(nix, &g, t).with_context(|| {
                format!(
                    "generation {n}'s Nix paths have no GC roots, so nix store gc would delete them"
                )
            })?;
            ui.step(format_args!("rooted generation {n}'s Nix paths"));
        }
    }
    let freed = nx_nix::nix::collect_garbage(nix).context("running nix store gc")?;
    let style = ui.out.style;
    ui.out.line(format_args!(
        "{} {}",
        style.green(ui::SYM_DONE),
        if freed.is_empty() {
            "nix store gc"
        } else {
            &freed
        }
    ));
    Ok(())
}

/// What `nx gc` drops: generations and unused store entries. Removes them
/// unless `dry_run`. Also returns the generations kept.
fn remove(machine: &Machine, dry_run: bool) -> Result<(Vec<u32>, usize, Vec<u32>)> {
    let state = host::state();
    // Off a bootc system nothing is deployed; on one, `Machine::read` has
    // already refused to go on without the deployments.
    let deployed: Vec<String> = machine.deployments().map_or_else(Vec::new, |d| {
        d.all()
            .iter()
            .filter_map(|d| d.image_digest.clone())
            .collect()
    });
    let guard = if dry_run {
        None
    } else {
        Some(state.lock().with_context(|| {
            format!(
                "removing generations needs an administrator (a member of wheel): no write access to {}",
                state.root().display()
            )
        })?)
    };
    // The current manifest says how many to keep.
    let keep = state
        .current_number()
        .ok()
        .flatten()
        .and_then(|n| Generation::read(&state, n).ok())
        .map_or_else(
            || nx_core::manifest::Gc::default().keep,
            |g| g.manifest.gc.keep,
        );
    let plan = select::gc_plan(&state, &deployed, keep)?;
    let store = select::unreferenced(&state, &plan.keep)?;
    if let Some(guard) = &guard {
        for n in &plan.drop {
            select::remove_generation(&state, guard, *n)?;
        }
        for sha in &store {
            state.store().remove(guard, sha)?;
        }
        // What an earlier gc set aside: removable now if this is root
        // (nx sync runs gc as root every day).
        state.purge_trash(guard);
    }
    Ok((plan.drop, store.len(), plan.keep))
}

fn render(out: &mut Stream, dropped: &[u32], packages: usize, dry_run: bool) {
    let style = out.style;
    for n in dropped {
        out.line(format_args!(
            "{} generation {n}",
            style.red(ui::SYM_REMOVED)
        ));
    }
    let what = format!(
        "{}, {} unused in the store",
        ui::plural(dropped.len(), "generation"),
        ui::plural(packages, "package")
    );
    if dry_run {
        out.line(style.dim(&format!("{what} · dry run, nothing changed")));
    } else if dropped.is_empty() && packages == 0 {
        out.line(format_args!(
            "{} nothing to remove",
            style.green(ui::SYM_DONE)
        ));
    } else {
        out.line(format_args!("{} removed {what}", style.green(ui::SYM_DONE)));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::ui::SharedBuf;

    /// Records each `nix` call; `store gc` answers with a summary.
    #[derive(Default)]
    struct Recording(RefCell<Vec<Vec<String>>>);

    impl nx_nix::Nix for Recording {
        fn run(&self, args: &[String], _: &mut dyn FnMut(&str)) -> nx_nix::Result<String> {
            self.0.borrow_mut().push(args.to_vec());
            Ok(if args.first().is_some_and(|a| a == "store") {
                "2 store paths deleted, 1.0 MiB freed\n".into()
            } else {
                String::new()
            })
        }
    }

    #[test]
    fn roots_kept_tool_tiers_before_collecting_the_nix_store() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-gc-nix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // An output that exists, so it counts as in the store.
        let out = dir.join("out");
        std::fs::create_dir_all(&out)?;
        let mut lock = nx_core::Lock::new(None, "45", "x86_64");
        lock.tool = Some(nx_core::LockedTool {
            packages: vec![nx_core::ToolPackage {
                attr: "ripgrep".into(),
                outputs: vec![nx_core::ToolOutput {
                    name: "out".into(),
                    path: out.display().to_string(),
                }],
                ..nx_core::ToolPackage::default()
            }],
            ..nx_core::LockedTool::default()
        });
        let gen_dir = dir.join("state/generations/3");
        std::fs::create_dir_all(&gen_dir)?;
        std::fs::write(gen_dir.join("lock.toml"), lock.to_toml()?)?;
        std::fs::write(
            gen_dir.join("manifest.toml"),
            nx_core::Manifest::default().to_toml()?,
        )?;
        let state = State::new(dir.join("state"));
        let nix = Recording::default();
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        collect_nix(&mut ui, &nix, &state, &[3])?;
        let calls = nix.0.borrow().clone();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0][..2], ["build", "--out-link"]);
        assert_eq!(calls[1], ["store", "gc"]);
        assert_eq!(buf.text(), "  ✓ 2 store paths deleted, 1.0 MiB freed\n");
        Ok(())
    }
}
