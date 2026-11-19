//! `nx remove`: drops packages from the manifest, re-resolves the host
//! tier that is left (which also drops the dependencies nothing else
//! needs), and makes the new generation live. A tool-tier package simply
//! leaves; its store paths go with the generations that root them.

use std::collections::HashSet;
use std::time::Instant;

use anyhow::{Result, bail};
use nx_core::{LockedTool, Manifest};
use nx_rpm::Refresh;

use crate::apply::{self, How, Session};
use crate::host::{self, Machine};
use crate::ui::Ui;

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
    let tool = host::readable(ui, &host::state())?
        .and_then(|g| g.lock.tool)
        .unwrap_or_default();
    let mut session = Session::open(ui, machine, refresh, dry_run, |m| {
        drop_from(m, &tool, specs)
    })?;
    let outcome = session.plan(ui, &How::default())?;
    // An allowance for a package that is no longer installed would quietly
    // pass it next time; the user should be asked again.
    let names: HashSet<&str> = outcome
        .all
        .iter()
        .map(|p| p.package.name.as_str())
        .collect();
    session
        .manifest
        .scriptlets
        .retain(|a| names.contains(a.package.as_str()));
    apply::finish(ui, &mut session, &outcome, started)
}

/// Takes `specs` out of `m`. A tool package's unfree allowance goes with
/// it, unless a package that stays has the same name; an entry nx can't
/// match to a package is the user's to remove.
fn drop_from(m: &mut Manifest, tool: &LockedTool, specs: &[String]) -> Result<()> {
    let specs: Vec<String> = specs.iter().map(|s| attr_for(m, tool, s)).collect();
    let (tools, hosts): (Vec<String>, Vec<String>) =
        specs.into_iter().partition(|s| m.tool.packages.contains(s));
    let missing = m.remove_host(&hosts);
    if !missing.is_empty() {
        bail!(
            "{} not added with nx add (nx list shows what was)",
            missing.join(", ")
        );
    }
    m.tool.packages.retain(|a| !tools.contains(a));
    let stays = |name: &str| {
        tool.packages
            .iter()
            .any(|k| k.name == name && m.tool.packages.contains(&k.attr))
    };
    let gone: Vec<String> = tool
        .packages
        .iter()
        .filter(|p| tools.contains(&p.attr) && !stays(&p.name))
        .map(|p| p.name.clone())
        .collect();
    m.tool.unfree.retain(|n| !gone.contains(n));
    Ok(())
}

/// The manifest entry `spec` means: itself, or the tool-tier attribute
/// whose name or exposed command it is.
fn attr_for(m: &Manifest, tool: &LockedTool, spec: &str) -> String {
    if m.has(spec) {
        return spec.to_owned();
    }
    tool.packages
        .iter()
        .find(|p| p.name == spec || p.bins.iter().any(|b| b.name == spec))
        .map_or_else(|| spec.to_owned(), |p| p.attr.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nx_core::{Bin, ToolPackage};

    fn tool() -> LockedTool {
        let package = |attr: &str, name: &str, bin: &str| ToolPackage {
            attr: attr.into(),
            name: name.into(),
            bins: vec![Bin {
                name: bin.into(),
                path: String::new(),
            }],
            ..ToolPackage::default()
        };
        LockedTool {
            packages: vec![
                package("ripgrep", "ripgrep", "rg"),
                package("terraform", "terraform", "terraform"),
                package("terraform_1", "terraform", "terraform1"),
                package("ngrok", "ngrok", "ngrok"),
            ],
            ..LockedTool::default()
        }
    }

    #[test]
    fn a_command_names_its_package() {
        let mut m = Manifest::default();
        m.add_tool(&["ripgrep".into()]);
        m.add_host(&["wayland-devel".into()]);
        assert_eq!(attr_for(&m, &tool(), "rg"), "ripgrep");
        assert_eq!(attr_for(&m, &tool(), "wayland-devel"), "wayland-devel");
        assert_eq!(attr_for(&m, &tool(), "nope"), "nope");
    }

    #[test]
    fn an_unfree_allowance_leaves_with_its_last_package() -> Result<()> {
        let mut m = Manifest::default();
        m.add_tool(&["terraform".into(), "terraform_1".into(), "ngrok".into()]);
        m.tool.unfree = vec!["terraform".into(), "ngrok".into(), "vault".into()];
        drop_from(&mut m, &tool(), &["terraform".into(), "ngrok".into()])?;
        // terraform_1 still needs it; vault isn't nx's to judge.
        assert_eq!(m.tool.unfree, ["terraform", "vault"]);
        drop_from(&mut m, &tool(), &["terraform1".into()])?;
        assert_eq!(m.tool.unfree, ["vault"]);
        assert!(drop_from(&mut m, &tool(), &["nope".into()]).is_err());
        Ok(())
    }
}
