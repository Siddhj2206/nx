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
    let (style, verbose) = (out.style, out.verbose);
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

    // Rows of three columns: name, version, note. A dependency is counted
    // on its entry's row; -v lists it under the entry.
    let mut host: Vec<[String; 3]> = Vec::new();
    let for_entry = |entry: &str| -> Vec<&LockedPackage> {
        deps.iter()
            .copied()
            .filter(|p| p.requested.first().map(String::as_str) == Some(entry))
            .collect()
    };
    let mut shown: Vec<&str> = Vec::new();
    for p in &direct {
        let mine = for_entry(&p.name);
        let note = match (mine.len(), verbose) {
            (0, _) | (_, true) => String::new(),
            (1, false) => "with 1 dependency".to_owned(),
            (n, false) => format!("with {n} dependencies"),
        };
        host.push([p.name.clone(), p.evr().to_owned(), note]);
        if verbose {
            for d in &mine {
                host.push([format!("  {}", d.name), d.evr().to_owned(), String::new()]);
            }
        }
        shown.extend(mine.iter().map(|d| d.name.as_str()));
    }
    // What an entry that names a provide or file pulled in.
    for p in deps.iter().filter(|p| !shown.contains(&p.name.as_str())) {
        host.push([
            p.name.clone(),
            p.evr().to_owned(),
            format!("for {}", p.requested.join(", ")),
        ]);
    }
    for spec in &in_image {
        host.push([(*spec).clone(), String::new(), "in the image".to_owned()]);
    }
    let tool_rows: Vec<[String; 3]> = tools
        .iter()
        .map(|p| {
            let names: Vec<&str> = p.bins.iter().map(|b| b.name.as_str()).collect();
            let runs = match names.as_slice() {
                [] => "no commands exposed".to_owned(),
                [one] if *one == p.attr => String::new(),
                _ => names.join(" "),
            };
            [p.attr.clone(), p.version.clone(), runs]
        })
        .collect();

    // One set of columns for both sections.
    let all = || host.iter().chain(&tool_rows);
    let name_w = all().map(|r| r[0].chars().count()).max().unwrap_or(0);
    let version_w = all().map(|r| r[1].chars().count()).max().unwrap_or(0);
    let section = |out: &mut Stream, title: &str, detail: &str, rows: &[[String; 3]]| {
        if rows.is_empty() {
            return;
        }
        out.line(format_args!("{} {}", style.bold(title), style.dim(detail)));
        for [name, version, note] in rows {
            let mut line = format!("  {}", ui::pad(name, name_w));
            if !version.is_empty() || !note.is_empty() {
                line.push_str("  ");
            }
            if note.is_empty() {
                line.push_str(&style.dim(version));
            } else if note == "in the image" {
                line.push_str(&ui::pad("", version_w));
                line.push_str("  ");
                line.push_str(&ui::in_image(style));
            } else {
                line.push_str(&style.dim(&ui::pad(version, version_w)));
                line.push_str("  ");
                line.push_str(&style.dim(note));
            }
            out.line(line.trim_end());
        }
        out.blank();
    };
    section(out, "Host", "· Fedora RPMs on the image's /usr", &host);
    let release = g
        .lock
        .tool
        .as_ref()
        .map(|t| format!("· nixpkgs {}", crate::tool::pinned(t).version()))
        .unwrap_or_default();
    section(out, "Tools", &release, &tool_rows);

    let mut foot = vec![ui::plural(g.lock.host.len(), "host package")];
    if !tools.is_empty() {
        foot.push(ui::plural(tools.len(), "tool"));
    }
    if verbose {
        foot.push(format!("generation {}", g.number));
    }
    out.line(style.dim(&foot.join(" · ")));
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

    fn text(g: Option<&Generation>, verbose: bool) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        ui.set_verbose(verbose);
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
            locked("libxml2", "2.14.6-1.fc45", &["wayland-devel"]),
        ];
        let g = Generation {
            number: 2,
            dir: Default::default(),
            manifest,
            lock,
            activation: Default::default(),
        };
        assert_eq!(
            text(Some(&g), false),
            "  Host · Fedora RPMs on the image's /usr\n\
             \x20   wayland-devel  1.24.0-1.fc45  with 2 dependencies\n\
             \x20   glibc                         ✓ in the image\n\
             \n\
             \x20 3 host packages\n"
        );
        assert_eq!(
            text(Some(&g), true),
            "  Host · Fedora RPMs on the image's /usr\n\
             \x20   wayland-devel   1.24.0-1.fc45\n\
             \x20     libffi-devel  3.5.1-2.fc45\n\
             \x20     libxml2       2.14.6-1.fc45\n\
             \x20   glibc                          ✓ in the image\n\
             \n\
             \x20 3 host packages · generation 2\n"
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
            packages: vec![
                tool("ripgrep", &["rg"]),
                nx_core::ToolPackage {
                    version: "1.7.1".into(),
                    ..tool("jq", &["jq"])
                },
            ],
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
            text(Some(&g), false),
            "  Tools · nixpkgs 26.11pre1122334\n\
             \x20   ripgrep  1.0    rg\n\
             \x20   jq       1.7.1\n\
             \n\
             \x20 0 host packages · 2 tools\n"
        );
    }

    #[test]
    fn says_when_empty() {
        assert_eq!(
            text(None, false),
            "  Nothing added yet · nx add <name> adds a package\n"
        );
    }
}
