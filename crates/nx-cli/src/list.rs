//! `nx list`: both tiers of the current generation, from its lockfile.
//! Reads no repo metadata or catalog, so it is instant and works offline.

use anyhow::Result;
use nx_core::{Generation, LockedPackage};

use crate::host;
use crate::ui::{self, Stream, Ui};

pub fn run(ui: &mut Ui) -> Result<()> {
    let current = host::readable(ui, &host::state())?;
    render(&mut ui.out, current.as_ref());
    Ok(())
}

/// What the user asked for first, then what it pulled in, then what the
/// manifest asks for that the image provides, then the tool tier with the
/// commands each package exposes.
fn render(out: &mut Stream, current: Option<&Generation>) {
    let style = out.style;
    let Some(g) = current.filter(|g| !g.manifest.is_empty()) else {
        out.line(style.dim("Nothing added yet · nx add <name> adds a package"));
        return;
    };
    let asked = &g.manifest.host.packages;
    let (direct, deps): (Vec<&LockedPackage>, Vec<&LockedPackage>) =
        g.lock.host.iter().partition(|p| asked.contains(&p.name));
    let in_image: Vec<&String> = asked
        .iter()
        .filter(|s| !g.lock.host.iter().any(|p| p.requested.contains(s)))
        .collect();
    let tools: &[nx_core::ToolPackage] = g.lock.tool.as_ref().map_or(&[], |t| &t.packages);
    let width = g
        .lock
        .host
        .iter()
        .map(|p| p.name.len())
        .chain(in_image.iter().map(|s| s.len()))
        .chain(tools.iter().map(|p| p.attr.len()))
        .max()
        .unwrap_or(0)
        + 2;
    let mut row = |p: &LockedPackage, note: String| {
        out.line(format_args!(
            "{}{}{note}",
            ui::pad(&style.bold(&p.name), width),
            style.dim(p.evr())
        ));
    };
    for p in &direct {
        row(p, String::new());
    }
    for p in &deps {
        row(p, style.dim(&format!("  · for {}", p.requested.join(", "))));
    }
    for spec in &in_image {
        out.line(format_args!(
            "{}{}",
            ui::pad(&style.bold(spec), width),
            ui::in_image(style)
        ));
    }
    for p in tools {
        let names: Vec<&str> = p.bins.iter().map(|b| b.name.as_str()).collect();
        let runs = match names.as_slice() {
            [] => "  · nixpkgs, no commands exposed".to_owned(),
            [one] if *one == p.attr => "  · nixpkgs".to_owned(),
            _ => format!("  · nixpkgs · {}", names.join(" ")),
        };
        out.line(format_args!(
            "{}{}{}",
            ui::pad(&style.bold(&p.attr), width),
            style.dim(&p.version),
            style.dim(&runs)
        ));
    }
    out.blank();
    let mut foot = format!("{} · ", ui::plural(g.lock.host.len(), "host package"));
    if let Some(t) = g.lock.tool.as_ref().filter(|t| !t.packages.is_empty()) {
        let release = crate::tool::pinned(t);
        foot.push_str(&format!(
            "{} at nixpkgs {} · ",
            ui::plural(t.packages.len(), "tool"),
            release.version()
        ));
    }
    foot.push_str(&format!("generation {}", g.number));
    out.line(style.dim(&foot));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;
    use nx_core::{Lock, Manifest};

    fn locked(name: &str, evr: &str, requested: &[&str]) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            nevra: format!("{name}-{evr}.x86_64"),
            repo: "fedora".into(),
            sha256: String::new(),
            requested: requested.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn text(g: Option<&Generation>) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, g);
        buf.text()
    }

    #[test]
    fn lists_requested_then_dependencies() {
        let mut manifest = Manifest::default();
        manifest.add_host(&["wayland-devel".into(), "glibc".into()]);
        let mut lock = Lock::new(None, "45", "x86_64");
        lock.host = vec![
            locked("libffi-devel", "3.5.1-2.fc45", &["wayland-devel"]),
            locked("wayland-devel", "1.24.0-1.fc45", &["wayland-devel"]),
        ];
        let g = Generation {
            number: 2,
            dir: Default::default(),
            manifest,
            lock,
            activation: Default::default(),
        };
        assert_eq!(
            text(Some(&g)),
            "  wayland-devel  1.24.0-1.fc45\n\
             \x20 libffi-devel   3.5.1-2.fc45  · for wayland-devel\n\
             \x20 glibc          ✓ in the image\n\
             \n\
             \x20 2 host packages · generation 2\n"
        );
    }

    #[test]
    fn lists_tools_with_their_commands() {
        let mut manifest = Manifest::default();
        manifest.add_tool(&["ripgrep".into(), "jq".into()]);
        let mut lock = Lock::new(None, "45", "x86_64");
        let tool = |attr: &str, bins: &[&str]| nx_core::ToolPackage {
            attr: attr.into(),
            version: "1.0".into(),
            bins: bins
                .iter()
                .map(|b| nx_core::Bin {
                    name: (*b).into(),
                    path: String::new(),
                })
                .collect(),
            ..nx_core::ToolPackage::default()
        };
        lock.tool = Some(nx_core::LockedTool {
            rev: "e21fec321d73511849707a7585e4ea155ee4bf40".into(),
            release: "nixpkgs-26.11pre1122334.e21fec321d73".into(),
            packages: vec![tool("ripgrep", &["rg"]), tool("jq", &["jq"])],
            ..nx_core::LockedTool::default()
        });
        let g = Generation {
            number: 3,
            dir: Default::default(),
            manifest,
            lock,
            activation: Default::default(),
        };
        assert_eq!(
            text(Some(&g)),
            "  ripgrep  1.0  · nixpkgs · rg\n\
             \x20 jq       1.0  · nixpkgs\n\
             \n\
             \x20 0 host packages · 2 tools at nixpkgs 26.11pre1122334 · generation 3\n"
        );
    }

    #[test]
    fn says_when_empty() {
        assert_eq!(
            text(None),
            "  Nothing added yet · nx add <name> adds a package\n"
        );
    }
}
