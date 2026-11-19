//! `nx remove`: drops packages from the manifest, re-resolves the host
//! tier that is left (which also drops the dependencies nothing else
//! needs), and makes the new generation live. A tool-tier package simply
//! leaves; its store paths go with the generations that root them.

use std::collections::HashSet;
use std::time::Instant;

use anyhow::{Result, bail};
use nx_rpm::Refresh;

use crate::apply::Session;
use crate::host::{self, Machine};
use crate::ui::{self, Stream, Ui};

pub fn run(
    ui: &mut Ui,
    machine: &Machine,
    specs: &[String],
    dry_run: bool,
    refresh: Refresh,
) -> Result<()> {
    let started = Instant::now();
    // `nx add rg` added ripgrep: `nx remove rg` means it too. Only names to
    // look up; the manifest itself is changed under the lock.
    let tool_names = host::readable(ui, &host::state())?
        .and_then(|g| g.lock.tool)
        .unwrap_or_default();
    let mut session = Session::open(ui, machine, refresh, dry_run, |m| {
        let specs: Vec<String> = specs.iter().map(|s| attr_for(m, &tool_names, s)).collect();
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
        let (kept, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut tool.packages)
            .into_iter()
            .partition(|p| attrs.contains(&p.attr));
        tool.packages = kept;
        // A removed package's unfree allowance goes with it, unless a package
        // that stays has the same name. Only those: an entry nx can't match
        // to a package is the user's to remove.
        for p in &gone {
            dropped.push(Dropped {
                name: p.attr.clone(),
                version: format!("{} · nixpkgs", p.version),
            });
            if !tool.packages.iter().any(|k| k.name == p.name) {
                session.manifest.tool.unfree.retain(|n| *n != p.name);
            }
        }
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

/// The manifest entry `spec` means: itself, or the tool-tier attribute
/// whose name or exposed command it is.
fn attr_for(m: &nx_core::Manifest, tool: &nx_core::LockedTool, spec: &str) -> String {
    if m.has(spec) {
        return spec.to_owned();
    }
    tool.packages
        .iter()
        .find(|p| p.name == spec || p.bins.iter().any(|b| b.name == spec))
        .map_or_else(|| spec.to_owned(), |p| p.attr.clone())
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
    fn a_command_names_its_package() {
        let mut m = nx_core::Manifest::default();
        m.add_tool(&["ripgrep".into()]);
        m.add_host(&["wayland-devel".into()]);
        let tool = nx_core::LockedTool {
            packages: vec![nx_core::ToolPackage {
                attr: "ripgrep".into(),
                name: "ripgrep".into(),
                bins: vec![nx_core::Bin {
                    name: "rg".into(),
                    path: String::new(),
                }],
                ..nx_core::ToolPackage::default()
            }],
            ..nx_core::LockedTool::default()
        };
        assert_eq!(attr_for(&m, &tool, "rg"), "ripgrep");
        assert_eq!(attr_for(&m, &tool, "wayland-devel"), "wayland-devel");
        assert_eq!(attr_for(&m, &tool, "nope"), "nope");
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
