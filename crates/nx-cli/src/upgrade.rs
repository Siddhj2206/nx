//! `nx upgrade`: moves both tiers to what their sources have now (design,
//! *Default update policy*). The host tier re-resolves without preferring
//! the current builds, so packages unrelated to the image take the repos'
//! newer ones; image relatives stay the image's exact builds, which only
//! the image moves. The tool tier moves its pin to the channel's current
//! release and evaluates every package there.

use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::Result;
use nx_rpm::Refresh;

use crate::apply::Session;
use crate::host::Machine;
use crate::tool::{self, Tier};
use crate::ui::{self, Stream, Ui};

/// What changed for one package.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    Upgraded {
        name: String,
        from: String,
        to: String,
    },
    Added {
        name: String,
        version: String,
    },
    Removed {
        name: String,
        version: String,
    },
}

pub fn run(
    ui: &mut Ui,
    machine: &Machine,
    (host, tool): (bool, bool),
    dry_run: bool,
    refresh: Refresh,
) -> Result<()> {
    let started = Instant::now();
    // Neither flag: both tiers.
    let (do_host, do_tool) = if host || tool {
        (host, tool)
    } else {
        (true, true)
    };
    let mut session = Session::open(ui, machine, refresh, dry_run, |_| Ok(()))?;
    let manifest = session.manifest.clone();
    if do_host {
        session.unlock();
    }
    let resolved = session.resolve_manifest(ui, &manifest.host.packages, &[])?;
    let mut changes = Vec::new();
    if do_host {
        let before: BTreeMap<String, String> = session
            .current
            .iter()
            .flat_map(|g| g.lock.host.iter())
            .map(|p| (p.name.clone(), p.evr().to_owned()))
            .collect();
        let after: BTreeMap<String, String> = resolved
            .all
            .iter()
            .map(|p| (p.package.name.clone(), p.package.evr.clone()))
            .collect();
        changes.extend(diff(&before, &after, ""));
    }
    let mut moved = None;
    if do_tool && tool::available() && !manifest.tool.packages.is_empty() {
        let tier = Tier::open(ui, session.tool.as_ref(), true)?;
        let was = session.tool.as_ref().map(|t| t.rev.clone());
        let same_pin = was.as_deref() == Some(tier.release.rev.as_str());
        // At the same pin, only paths the store lost need doing: evaluate
        // everything again (their derivations may be gone too) and realise.
        let lost = session.tool.as_ref().is_some_and(tool::incomplete);
        if !same_pin || lost {
            let system = tool::system(session.universe.arch());
            let locked = tier.resolve(ui, &manifest, None, &system)?;
            let version = |t: &nx_core::LockedTool| -> BTreeMap<String, String> {
                t.packages
                    .iter()
                    .map(|p| (p.attr.clone(), p.version.clone()))
                    .collect()
            };
            let before = session.tool.as_ref().map(version).unwrap_or_default();
            changes.extend(diff(&before, &version(&locked), " · nixpkgs"));
            if !dry_run {
                let plan = tier.plan(&locked, &manifest.caches)?;
                tier.realise(ui, &locked, &plan, &manifest, &system)?;
            }
            moved = Some(if same_pin {
                format!("{}, realised again", tier.release.version())
            } else {
                tier.release.version().to_owned()
            });
            session.tool = Some(locked);
        }
    }
    if changes.is_empty() && moved.is_none() {
        ui.done("everything is up to date");
        return Ok(());
    }
    if dry_run {
        render(&mut ui.out, &changes, moved.as_deref(), None, 0.0);
        return Ok(());
    }
    let built = session.install(ui, &manifest, &resolved.all)?;
    render(
        &mut ui.out,
        &changes,
        moved.as_deref(),
        Some(built.generation.number),
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

/// Package by package, from `before` to `after` (name → version); `tag`
/// follows the version.
fn diff(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    tag: &str,
) -> Vec<Change> {
    let mut out = Vec::new();
    for (name, to) in after {
        match before.get(name) {
            Some(from) if from != to => out.push(Change::Upgraded {
                name: name.clone(),
                from: from.clone(),
                to: format!("{to}{tag}"),
            }),
            Some(_) => {}
            None => out.push(Change::Added {
                name: name.clone(),
                version: format!("{to}{tag}"),
            }),
        }
    }
    for (name, from) in before {
        if !after.contains_key(name) {
            out.push(Change::Removed {
                name: name.clone(),
                version: format!("{from}{tag}"),
            });
        }
    }
    out
}

fn render(
    out: &mut Stream,
    changes: &[Change],
    moved: Option<&str>,
    generation: Option<u32>,
    secs: f64,
) {
    let style = out.style;
    for c in changes {
        match c {
            Change::Upgraded { name, from, to } => out.line(format_args!(
                "{} {name} {}",
                style.cyan(ui::SYM_UPGRADED),
                style.dim(&format!("{from} → {to}"))
            )),
            Change::Added { name, version } => out.line(format_args!(
                "{} {name} {}",
                style.green(ui::SYM_ADDED),
                style.dim(version)
            )),
            Change::Removed { name, version } => out.line(format_args!(
                "{} {name} {}",
                style.red(ui::SYM_REMOVED),
                style.dim(version)
            )),
        }
    }
    let upgraded = changes
        .iter()
        .filter(|c| matches!(c, Change::Upgraded { .. }))
        .count();
    let mut what = format!("{} upgraded", ui::plural(upgraded, "package"));
    if let Some(v) = moved {
        what.push_str(&format!(" · nixpkgs {v}"));
    }
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

    fn map(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn lists_what_moved() {
        let changes = diff(
            &map(&[("ripgrep", "15.1.0"), ("jq", "1.8.2"), ("old", "1")]),
            &map(&[("ripgrep", "15.2.0"), ("jq", "1.8.2"), ("new", "2")]),
            " · nixpkgs",
        );
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, &changes, Some("26.11pre1122334"), Some(4), 2.0);
        assert_eq!(
            buf.text(),
            "  + new 2 · nixpkgs\n\
             \x20 ↑ ripgrep 15.1.0 → 15.2.0 · nixpkgs\n\
             \x20 - old 1 · nixpkgs\n\
             \x20 ✓ 1 package upgraded · nixpkgs 26.11pre1122334 · generation 4 in 2.0s\n"
        );
    }
}
