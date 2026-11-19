//! `nx select`: at boot, points `current` at the generation built for the
//! image that booted (design, *Image upgrade lifecycle*). Runs as root
//! before `systemd-sysext` merges the extension, from local state only:
//! boot never waits on the network (invariant 7).
//!
//! No generation for this digest (the image changed without nx noticing)
//! means the newest earlier one, if it still fits (invariant 2). When it
//! doesn't, an empty generation is made current, so the host tier is
//! simply not merged and the system boots cleanly; `nx sync` resolves
//! for the new image once the machine is up.

use std::path::Path;

use anyhow::{Context, Result};
use nx_core::generation::{self, Spec};
use nx_core::select;
use nx_core::{BootedOstree, Generation, Lock};

use crate::host::{self, Machine};
use crate::ui::{self, Ui};

pub fn run(ui: &mut Ui) -> Result<()> {
    let result = select(ui);
    if result.is_err() {
        // Which generation fits this image is unknown: merging whatever
        // `current` points at could shadow the image's files (invariants 1
        // and 2), so merge nothing. The next boot, or `nx sync`, tries again.
        let _ = host::state().clear_current();
    }
    result
}

fn select(ui: &mut Ui) -> Result<()> {
    let root = Path::new("/");
    let machine = Machine::read(root, &BootedOstree::new(root)?)?;
    let Some(digest) = machine.digest() else {
        ui.done("not a bootc system; nothing to select");
        return Ok(());
    };
    let state = host::state();
    let guard = state.lock().with_context(|| {
        format!(
            "selecting a generation needs write access to {}",
            state.root().display()
        )
    })?;
    let current = state.current_number().context("reading `current`")?;
    let short = ui::short_digest(&digest);
    if let Some(n) = select::newest_for(&state, &digest)? {
        if current != Some(n) {
            state.set_current(n)?;
        }
        ui.done(format_args!(
            "generation {n} for the booted image ({short})"
        ));
        return Ok(());
    }
    // Nothing built for this image: the generation that was current, or the
    // newest one nx can read, is the candidate.
    let previous = match current.map(|n| Generation::read(&state, n)) {
        Some(Ok(g)) => Some(g),
        _ => select::locks(&state)?
            .into_iter()
            .rev()
            .find_map(|(n, _)| Generation::read(&state, n).ok()),
    };
    let Some(previous) = previous else {
        ui.done("no generations yet; nothing to select");
        return Ok(());
    };
    match crate::compat::check(&machine, &previous) {
        Ok(()) => {
            state.set_current(previous.number)?;
            ui.warn(format_args!(
                "no generation for the booted image ({short}); generation {} still fits it, nx sync will resolve for it",
                previous.number
            ));
        }
        Err(why) => {
            let lock = {
                let mut lock = Lock::new(
                    Some(digest.clone()),
                    &machine.release.version_id,
                    &machine.arch,
                );
                lock.parent = Some(previous.number);
                lock.pending = true;
                lock
            };
            let image_root = machine.image_root()?;
            let built = generation::build(
                &state,
                &guard,
                &Spec {
                    manifest: &previous.manifest,
                    lock: &lock,
                    image_root: &image_root,
                    os_id: &machine.release.id,
                    image_has: &|_| false,
                },
            )?;
            state.set_current(built.generation.number)?;
            ui.warn(format_args!(
                "no generation for the booted image ({short}), and generation {} doesn't fit it: {why}; \
                 the host tier is not merged until nx sync resolves for this image",
                previous.number
            ));
        }
    }
    Ok(())
}
