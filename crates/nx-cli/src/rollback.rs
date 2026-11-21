//! `nx rollback [N]`: makes an earlier generation current again. Nothing
//! is resolved or downloaded: the generation is already on disk.

use anyhow::{Context, Result, bail};
use nx_core::{Generation, State};

use crate::changes::{self, Change};
use crate::host::{self, Machine};
use crate::ui::{self, Problem, Stream, Ui};

pub fn run(ui: &mut Ui, machine: &Machine, target: Option<u32>, dry_run: bool) -> Result<()> {
    let state = host::state();
    let _guard = state
        .lock()
        .map_err(|e| host::not_allowed(&state, "changing packages", e))?;
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
            Problem::new(format!(
                "generation {number} was built for {} and doesn't fit the booted image",
                generation
                    .lock
                    .image
                    .as_deref()
                    .map_or("no image", crate::ui::short_digest)
            ))
            .reason(why)
            .fix("The ones that do", "nx generations")
        })?;
    }
    let changes = switch(&current, &generation);
    if !dry_run {
        crate::live::activate(ui, &state, number)?;
    }
    render(&mut ui.out, &changes, current.number, number, dry_run);
    Ok(())
}

/// What going from `current` to `to` changes, in both tiers.
fn switch(current: &Generation, to: &Generation) -> Vec<Change> {
    let mut changes = changes::host_locks(&current.lock.host, &to.lock.host);
    changes.extend(changes::tools(
        current.lock.tool.as_ref(),
        to.lock.tool.as_ref(),
    ));
    changes
}

/// The table, then `✓ rolled back · 2 packages change` (or for a dry run,
/// what it would do); `-v` names the generations (rule 4).
fn render(out: &mut Stream, changes: &[Change], from: u32, to: u32, dry_run: bool) {
    let style = out.style;
    changes::table(out, changes);
    let mut parts = vec![
        if dry_run {
            "would roll back"
        } else {
            "rolled back"
        }
        .to_owned(),
    ];
    if out.verbose {
        let was = if dry_run { "now" } else { "was" };
        parts.push(format!("generation {to} ({was} {from})"));
    }
    parts.push(if changes.is_empty() {
        "the packages stay the same".to_owned()
    } else {
        ui::plural(changes.len(), "package") + " change"
    });
    if dry_run {
        parts.push("dry run, nothing changed".to_owned());
        out.line(style.dim(&parts.join(" · ")));
    } else {
        out.line(format_args!(
            "{} {}",
            style.green(ui::SYM_DONE),
            parts.join(" · ")
        ));
    }
}

/// The generation to go to: `target`, or the one `current` was built
/// from, or (for one with no parent) the newest one numbered before it.
fn pick(state: &State, current: &Generation, target: Option<u32>) -> Result<u32> {
    let all = state.generations()?;
    let number = current.number;
    match target {
        Some(n) if n == number => bail!("generation {n} is already current"),
        Some(n) if all.contains(&n) => Ok(n),
        Some(n) => Err(Problem::new(format!("there is no generation {n}"))
            .fix("The generations", "nx generations")
            .into()),
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
    fn shows_what_the_switch_changes() {
        let package = |name: &str, evr: &str| nx_core::LockedPackage {
            name: name.into(),
            nevra: format!("{name}-{evr}.x86_64"),
            repo: "fedora".into(),
            sha256: String::new(),
            requested: vec![name.into()],
        };
        let mut now = generation(5, Some(4));
        now.lock.host = vec![package("wayland-devel", "1.26-1.fc45")];
        let mut back = generation(4, None);
        back.lock.host = vec![
            package("wayland-devel", "1.25-1.fc45"),
            package("jq", "1.8-1.fc45"),
        ];
        let out = crate::ui::SharedBuf::default();
        let mut ui = Ui::plain(out.clone(), crate::ui::SharedBuf::default());
        render(&mut ui.out, &switch(&now, &back), 5, 4, true);
        assert_eq!(
            out.text(),
            "  + jq             1.8-1.fc45\n\
             \x20 ↑ wayland-devel  1.26-1.fc45 → 1.25-1.fc45\n\
             \x20 would roll back · 2 packages change · dry run, nothing changed\n"
        );
        let out = crate::ui::SharedBuf::default();
        let mut ui = Ui::plain(out.clone(), crate::ui::SharedBuf::default());
        ui.set_verbose(true);
        render(&mut ui.out, &[], 5, 4, false);
        assert_eq!(
            out.text(),
            "  ✓ rolled back · generation 4 (was 5) · the packages stay the same\n"
        );
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
