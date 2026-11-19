//! `nx gc`: removes generations nothing needs and the store entries only
//! they used. Kept: the current generation, the newest for each deployed
//! image (booted, staged and rollback: design invariant 5) and the few
//! newest, so `nx rollback` has somewhere to go.

use anyhow::{Context, Result};
use nx_core::select;

use crate::host::{self, Machine};
use crate::ui::{self, Stream, Ui};

/// `nx sync`'s tail: removes what `nx gc` would, saying so only when
/// something went. Returns how many generations were removed.
pub fn collect(ui: &mut Ui, machine: &Machine) -> Result<usize> {
    let removed = remove(machine, false)?;
    if !removed.0.is_empty() || removed.1 > 0 {
        render(&mut ui.out, &removed.0, removed.1, false);
    }
    Ok(removed.0.len())
}

pub fn run(ui: &mut Ui, machine: &Machine, dry_run: bool) -> Result<()> {
    let (dropped, packages) = remove(machine, dry_run)?;
    render(&mut ui.out, &dropped, packages, dry_run);
    Ok(())
}

/// What `nx gc` drops: generations and unused store entries. Removes them
/// unless `dry_run`.
fn remove(machine: &Machine, dry_run: bool) -> Result<(Vec<u32>, usize)> {
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
    let plan = select::gc_plan(&state, &deployed)?;
    let store = select::unreferenced(&state, &plan.keep)?;
    if let Some(guard) = &guard {
        for n in &plan.drop {
            select::remove_generation(&state, guard, *n)?;
        }
        for sha in &store {
            state.store().remove(guard, sha)?;
        }
    }
    Ok((plan.drop, store.len()))
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
