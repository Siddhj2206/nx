//! `nx remove`: drops packages from the manifest, re-resolves what is
//! left (which also drops the dependencies nothing else needs), and makes
//! the new generation live.

use std::collections::HashSet;
use std::time::Instant;

use anyhow::{Result, bail};
use nx_core::LockedPackage;
use nx_rpm::Refresh;

use crate::apply::Session;
use crate::host::Machine;
use crate::ui::{self, Stream, Ui};

pub fn run(
    ui: &mut Ui,
    machine: &Machine,
    specs: &[String],
    dry_run: bool,
    refresh: Refresh,
) -> Result<()> {
    let started = Instant::now();
    let mut session = Session::open(ui, machine, refresh, dry_run, |m| {
        let missing = m.remove_host(specs);
        if !missing.is_empty() {
            bail!(
                "{} not added with nx add (nx list shows what was)",
                missing.join(", ")
            );
        }
        Ok(())
    })?;
    let manifest = session.manifest.clone();
    let resolved = session.resolve(ui, &manifest.host.packages)?;
    let keep: HashSet<String> = resolved.all.iter().map(|p| p.package.nevra()).collect();
    let dropped: Vec<LockedPackage> = session
        .current
        .iter()
        .flat_map(|g| g.lock.host.iter())
        .filter(|p| !keep.contains(&p.nevra))
        .cloned()
        .collect();
    if dry_run {
        render(&mut ui.out, &dropped, None, started.elapsed().as_secs_f64());
        return Ok(());
    }
    let built = session.install(ui, &manifest, &resolved.all)?;
    render(
        &mut ui.out,
        &dropped,
        Some(built.generation.number),
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

/// One `-` line per package that left the host tier, then the total;
/// `generation` is `None` for a dry run.
fn render(out: &mut Stream, dropped: &[LockedPackage], generation: Option<u32>, secs: f64) {
    let style = out.style;
    for p in dropped {
        out.line(format_args!(
            "{} {} {}",
            style.red(ui::SYM_REMOVED),
            p.name,
            style.dim(p.evr())
        ));
    }
    let what = if dropped.is_empty() {
        "manifest updated".to_owned()
    } else {
        format!("{} removed", ui::plural(dropped.len(), "package"))
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

    fn package(name: &str) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            nevra: format!("{name}-1-1.fc45.x86_64"),
            ..LockedPackage::default()
        }
    }

    fn text(dropped: &[LockedPackage], generation: Option<u32>) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, dropped, generation, 0.25);
        buf.text()
    }

    #[test]
    fn lists_what_left_and_the_generation() {
        let dropped = [package("wayland-devel"), package("libffi-devel")];
        assert_eq!(
            text(&dropped, Some(5)),
            "  - wayland-devel 1-1.fc45\n\
             \x20 - libffi-devel 1-1.fc45\n\
             \x20 ✓ 2 packages removed · generation 5 in 0.2s\n"
        );
        assert_eq!(
            text(&dropped[..1], None),
            "  - wayland-devel 1-1.fc45\n  1 package removed · dry run, nothing changed\n"
        );
        assert_eq!(
            text(&[], Some(6)),
            "  ✓ manifest updated · generation 6 in 0.2s\n"
        );
    }
}
