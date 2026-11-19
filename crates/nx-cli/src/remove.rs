//! `nx remove`: drops packages from the manifest, re-resolves the host
//! tier that is left (which also drops the dependencies nothing else
//! needs), and makes the new generation live. A tool-tier package simply
//! leaves; its store paths go with the generations that root them.

use std::collections::HashSet;
use std::time::Instant;

use anyhow::{Result, bail};
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
        let (tools, hosts): (Vec<String>, Vec<String>) = specs
            .iter()
            .cloned()
            .partition(|s| m.tool.packages.contains(s));
        let missing = m.remove_host(&hosts);
        if !missing.is_empty() {
            bail!(
                "{} not added with nx add (nx list shows what was)",
                missing.join(", ")
            );
        }
        m.tool.packages.retain(|a| !tools.contains(a));
        Ok(())
    })?;
    let resolved = session.resolve_manifest(ui, &session.manifest.host.packages.clone(), &[])?;
    // An allowance for a package that is no longer installed would quietly
    // pass it next time; the user should be asked again.
    let names: HashSet<&str> = resolved
        .all
        .iter()
        .map(|p| p.package.name.as_str())
        .collect();
    session
        .manifest
        .scriptlets
        .retain(|a| names.contains(a.package.as_str()));
    let keep: HashSet<String> = resolved.all.iter().map(|p| p.package.nevra()).collect();
    let mut dropped: Vec<Dropped> = session
        .current
        .iter()
        .flat_map(|g| g.lock.host.iter())
        .filter(|p| !keep.contains(&p.nevra))
        .map(|p| Dropped {
            name: p.name.clone(),
            version: p.evr().to_owned(),
        })
        .collect();
    // The tool tier needs no resolve: what is left is as it was.
    let attrs = session.manifest.tool.packages.clone();
    if let Some(tool) = session.tool.as_mut() {
        for p in tool.packages.iter().filter(|p| !attrs.contains(&p.attr)) {
            dropped.push(Dropped {
                name: p.attr.clone(),
                version: format!("{} · nixpkgs", p.version),
            });
        }
        tool.packages.retain(|p| attrs.contains(&p.attr));
        let unfree: HashSet<&str> = tool.packages.iter().map(|p| p.name.as_str()).collect();
        session
            .manifest
            .tool
            .unfree
            .retain(|n| unfree.contains(n.as_str()));
    }
    let manifest = session.manifest.clone();
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

/// A package that left: its name and version.
struct Dropped {
    name: String,
    version: String,
}

/// One `-` line per package that left either tier, then the total;
/// `generation` is `None` for a dry run.
fn render(out: &mut Stream, dropped: &[Dropped], generation: Option<u32>, secs: f64) {
    let style = out.style;
    for p in dropped {
        out.line(format_args!(
            "{} {} {}",
            style.red(ui::SYM_REMOVED),
            p.name,
            style.dim(&p.version)
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

    fn package(name: &str) -> Dropped {
        Dropped {
            name: name.into(),
            version: "1-1.fc45".into(),
        }
    }

    fn text(dropped: &[Dropped], generation: Option<u32>) -> String {
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
