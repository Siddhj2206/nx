//! `nx rollback [N]`: makes an earlier generation current again. Nothing
//! is resolved or downloaded: the generation is already on disk.

use anyhow::{Context, Result, bail};
use nx_core::{Generation, State};

use crate::host::{self, Machine};
use crate::ui::Ui;

pub fn run(ui: &mut Ui, machine: &Machine, target: Option<u32>, dry_run: bool) -> Result<()> {
    let state = host::state();
    let _guard = state.lock().with_context(|| {
        format!(
            "changing packages needs an administrator (a member of wheel): no write access to {}",
            state.root().display()
        )
    })?;
    let current = Generation::current(&state)
        .context("reading the current generation")?
        .context("nothing is installed yet, so there is nothing to roll back")?;
    let number = pick(&state, &current, target)?;
    let generation =
        Generation::read(&state, number).with_context(|| format!("reading generation {number}"))?;
    // A generation built for another image is merged only if it still fits
    // the booted one (design invariant 2). On a bootc system one with no
    // digest was built elsewhere, so it is checked too.
    if let Some(booted) = machine.digest()
        && generation.lock.image.as_ref() != Some(&booted)
    {
        crate::compat::check(machine, &generation).map_err(|why| {
            anyhow::anyhow!(
                "generation {number} was built for {} and doesn't fit the booted image: {why}",
                generation
                    .lock
                    .image
                    .as_deref()
                    .map_or("no image", crate::ui::short_digest)
            )
        })?;
    }
    if dry_run {
        ui.out.line(ui.out.style.dim(&format!(
            "would make generation {number} current (now {}) · dry run, nothing changed",
            current.number
        )));
        return Ok(());
    }
    crate::live::activate(ui, &state, number, generation.manifest.host.backend)?;
    ui.done(format_args!(
        "generation {number} is current · was {} · {}",
        current.number,
        crate::ui::plural(generation.lock.host.len(), "package")
    ));
    Ok(())
}

/// The generation to go to: `target`, or the one `current` was built
/// from, or (for one with no parent) the newest one numbered before it.
fn pick(state: &State, current: &Generation, target: Option<u32>) -> Result<u32> {
    let all = state.generations()?;
    let number = current.number;
    match target {
        Some(n) if n == number => bail!("generation {n} is already current"),
        Some(n) if all.contains(&n) => Ok(n),
        Some(n) => bail!("there is no generation {n} (nx generations lists them)"),
        None => current
            .lock
            .parent
            .filter(|p| all.contains(p))
            .or_else(|| all.into_iter().rev().find(|n| *n < number))
            .with_context(|| format!("generation {number} is the oldest; nothing to roll back to")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(number: u32, parent: Option<u32>) -> Generation {
        let mut lock = nx_core::Lock::new(None, "45", "x86_64");
        lock.parent = parent;
        Generation {
            number,
            dir: Default::default(),
            manifest: nx_core::Manifest::default(),
            lock,
            activation: Default::default(),
        }
    }

    #[test]
    fn undoes_the_change_not_the_numbering() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for n in [1, 3, 4, 7] {
            std::fs::create_dir_all(dir.join(format!("generations/{n}")))?;
        }
        let state = State::new(&dir);
        // 7 was built from 3 (4 was rolled back from); a gone parent or no
        // parent falls back to the numbering.
        assert_eq!(pick(&state, &generation(7, Some(3)), None)?, 3);
        assert_eq!(pick(&state, &generation(7, Some(2)), None)?, 4);
        assert_eq!(pick(&state, &generation(4, None), None)?, 3);
        assert_eq!(pick(&state, &generation(7, None), Some(1))?, 1);
        assert!(pick(&state, &generation(1, None), None).is_err());
        assert!(pick(&state, &generation(7, None), Some(7)).is_err());
        assert!(pick(&state, &generation(7, None), Some(5)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
