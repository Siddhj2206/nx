//! `nx rollback [N]`: makes an earlier generation current again. Nothing
//! is resolved or downloaded: the generation is already on disk.

use anyhow::{Context, Result, bail};
use nx_core::{Generation, State};

use crate::host::{self, Machine};
use crate::ui::Ui;

pub fn run(ui: &mut Ui, machine: &Machine, target: Option<u32>) -> Result<()> {
    let state = host::state();
    let _guard = state.lock().with_context(|| {
        format!(
            "nx needs write access to {} (set NX_STATE_DIR to use another directory)",
            state.root().display()
        )
    })?;
    let current = Generation::current(&state)
        .context("reading the current generation")?
        .context("nothing is installed yet, so there is nothing to roll back")?;
    let number = pick(&state, current.number, target)?;
    let generation =
        Generation::read(&state, number).with_context(|| format!("reading generation {number}"))?;
    // Merging a generation built for another image could shadow that
    // image's files (design invariant 2); M5 adds the compatibility check
    // that lets some of them through.
    if let (Some(booted), Some(built)) = (machine.digest(), &generation.lock.image)
        && *built != booted
    {
        bail!(
            "generation {number} was built for another image ({}); only generations built for the booted image can be activated",
            short(built)
        );
    }
    crate::live::activate(ui, &state, number, generation.manifest.host.backend)?;
    ui.done(format_args!(
        "generation {number} is current · was {} · {}",
        current.number,
        crate::ui::plural(generation.lock.host.len(), "package")
    ));
    Ok(())
}

/// The generation to go to: `target`, or the newest one before `current`.
fn pick(state: &State, current: u32, target: Option<u32>) -> Result<u32> {
    let all = state.generations()?;
    match target {
        Some(n) if n == current => bail!("generation {n} is already current"),
        Some(n) if all.contains(&n) => Ok(n),
        Some(n) => bail!("there is no generation {n} (nx generations lists them)"),
        None => all
            .into_iter()
            .rev()
            .find(|n| *n < current)
            .with_context(|| {
                format!("generation {current} is the oldest; nothing to roll back to")
            }),
    }
}

fn short(digest: &str) -> String {
    digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .chars()
        .take(12)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_newest_earlier_generation() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for n in [1, 3, 4, 7] {
            std::fs::create_dir_all(dir.join(format!("generations/{n}")))?;
        }
        let state = State::new(&dir);
        assert_eq!(pick(&state, 7, None)?, 4);
        assert_eq!(pick(&state, 4, None)?, 3);
        assert_eq!(pick(&state, 7, Some(1))?, 1);
        assert!(pick(&state, 1, None).is_err());
        assert!(pick(&state, 7, Some(7)).is_err());
        assert!(pick(&state, 7, Some(5)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn shortens_digests() {
        assert_eq!(short("sha256:0123456789abcdef0123"), "0123456789ab");
    }
}
