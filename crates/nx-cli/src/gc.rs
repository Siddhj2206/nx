//! `nx gc`: removes generations nothing needs and the store entries only
//! they used. Kept: the current generation, the newest for each deployed
//! image (booted, staged and rollback: design invariant 5) and the few
//! newest, so `nx rollback` has somewhere to go.

use anyhow::{Context, Result};
use nx_core::State;
use nx_core::select;

use crate::host::{self, Machine};
use crate::ui::{self, Stream, Ui};

pub fn run(ui: &mut Ui, machine: &Machine, dry_run: bool) -> Result<()> {
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
    render(&mut ui.out, &plan.drop, store.len(), dry_run, &state);
    Ok(())
}

fn render(out: &mut Stream, dropped: &[u32], packages: usize, dry_run: bool, _state: &State) {
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
