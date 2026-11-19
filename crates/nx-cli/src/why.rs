//! `nx why <name|path>`: who provides a package, command or file, and the
//! generation that added it.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use nx_core::{Generation, LockedPackage, State, ToolPackage};
use nx_rpm::{Package, Refresh, Universe};

use crate::host::{self, Machine};
use crate::ui::{self, Stream, Ui};

/// Who provides the thing asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// A host-tier package, and the generation that first added it (of the
    /// run of generations leading to the current one).
    Host {
        package: LockedPackage,
        current: u32,
        added: u32,
        /// Why `nx add` put it in the host tier, if it was asked for.
        route: Option<String>,
    },
    /// A tool-tier package, with the command asked about and the release.
    Tool {
        package: ToolPackage,
        command: Option<String>,
        release: String,
        current: u32,
        added: u32,
        /// Why `nx add` put it in the tool tier.
        route: Option<String>,
    },
    Image(Package),
}

pub fn run(ui: &mut Ui, machine: &Machine, what: &str) -> Result<()> {
    let state = host::state();
    let current = host::readable(ui, &state)?;
    if let Some(g) = &current
        && let Some(found) = in_generation(&state, g, what)?
    {
        render(&mut ui.out, what, &found);
        return Ok(());
    }
    if let Some(g) = &current
        && let Some(found) = in_tool_tier(&state, g, what)
    {
        render(&mut ui.out, what, &found);
        return Ok(());
    }
    let opts = machine.options(Refresh::IfMissing)?;
    let image = Universe::load_image(&opts).context("reading the image's packages")?;
    let found = if what.contains('/') {
        image.image_owners(what)?.into_iter().next()
    } else {
        match image.info(what)? {
            Some(p) => Some(p),
            None => {
                let mut owner = None;
                for dir in nx_rpm::COMMAND_DIRS {
                    owner = image
                        .image_owners(&format!("/{dir}/{what}"))?
                        .into_iter()
                        .next();
                    if owner.is_some() {
                        break;
                    }
                }
                owner
            }
        }
    };
    match found {
        Some(p) => {
            render(&mut ui.out, what, &Owner::Image(p));
            Ok(())
        }
        None => bail!("nothing nx knows of provides {what} (try nx search {what})"),
    }
}

/// The host-tier package that provides `what`: a package name, a path in
/// the extension, or a command ([`nx_rpm::COMMAND_DIRS`]).
fn in_generation(state: &State, g: &Generation, what: &str) -> Result<Option<Owner>> {
    let found = if what.contains('/') {
        owner_of_path(state, g, what)
    } else {
        g.lock.package(what).cloned().or_else(|| {
            nx_rpm::COMMAND_DIRS
                .iter()
                .find_map(|dir| owner_of_path(state, g, &format!("/{dir}/{what}")))
        })
    };
    let Some(package) = found else {
        return Ok(None);
    };
    let mut added = g.number;
    for n in (1..g.number).rev() {
        let had = Generation::read(state, n)
            .is_ok_and(|older| older.lock.host.iter().any(|p| p.nevra == package.nevra));
        if !had {
            break;
        }
        added = n;
    }
    let route = g.lock.routes.get(&package.name).cloned();
    Ok(Some(Owner::Host {
        package,
        current: g.number,
        added,
        route,
    }))
}

/// The tool-tier package that is `what`: its attribute, an exposed
/// command, or a path in one of its outputs.
fn in_tool_tier(state: &State, g: &Generation, what: &str) -> Option<Owner> {
    let tool = g.lock.tool.as_ref()?;
    let (package, command) = tool.packages.iter().find_map(|p| {
        if p.attr == what {
            return Some((p, None));
        }
        if let Some(b) = p.bins.iter().find(|b| b.name == what) {
            return Some((p, Some(b.name.clone())));
        }
        p.paths()
            .any(|o| what == o || what.starts_with(&format!("{o}/")))
            .then_some((p, None))
    })?;
    let mut added = g.number;
    for n in (1..g.number).rev() {
        let had = Generation::read(state, n).is_ok_and(|older| {
            older
                .lock
                .tool
                .as_ref()
                .and_then(|t| t.package(&package.attr))
                .is_some_and(|p| p.outputs == package.outputs)
        });
        if !had {
            break;
        }
        added = n;
    }
    Some(Owner::Tool {
        package: package.clone(),
        command,
        release: tool.release.clone(),
        current: g.number,
        added,
        route: g.lock.routes.get(&package.attr).cloned(),
    })
}

fn owner_of_path(state: &State, g: &Generation, path: &str) -> Option<LockedPackage> {
    let rel = relative(path)?;
    std::fs::symlink_metadata(g.sysext().join(&rel)).ok()?;
    let store = state.store();
    g.lock
        .host
        .iter()
        .find(|p| std::fs::symlink_metadata(store.path(&p.sha256).join(&rel)).is_ok())
        .cloned()
}

fn relative(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(path).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::RootDir | Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn render(out: &mut Stream, what: &str, owner: &Owner) {
    let style = out.style;
    out.blank();
    match owner {
        Owner::Host {
            package,
            current,
            added,
            route,
        } => {
            out.line(format_args!(
                "{} {}",
                style.bold(&package.name),
                style.dim(package.evr())
            ));
            out.blank();
            if what != package.name {
                out.row("Asked", what);
            }
            out.row(
                "Tier",
                &format!("host {}", style.dim("· Fedora RPM on the image's /usr")),
            );
            out.row("Repo", &package.repo);
            let reason = if package.requested.contains(&package.name) {
                you_added(style, route.as_deref(), false)
            } else {
                format!("needed by {}", package.requested.join(", "))
            };
            out.row("Why", &reason);
            out.row("Generation", &generation_row(style, *current, *added));
        }
        Owner::Tool {
            package,
            command,
            release,
            current,
            added,
            route,
        } => {
            out.line(format_args!(
                "{} {}",
                style.bold(&package.attr),
                style.dim(&package.version)
            ));
            out.blank();
            if what != package.attr {
                out.row("Asked", what);
            }
            out.row(
                "Tier",
                &format!("tool {}", style.dim("· nixpkgs, sealed beside the image")),
            );
            out.row("Release", release);
            for o in &package.outputs {
                out.row("Store", &o.path);
            }
            if let Some(c) = command
                .as_ref()
                .and_then(|c| package.bins.iter().find(|b| &b.name == c))
            {
                out.row("Runs", &c.path);
            }
            out.row("Why", &you_added(style, route.as_deref(), package.unfree));
            out.row("Generation", &generation_row(style, *current, *added));
        }
        Owner::Image(p) => {
            out.line(format_args!(
                "{} {}  {}",
                style.bold(&p.name),
                style.dim(&p.evr),
                ui::in_image(style)
            ));
            out.blank();
            if what != p.name {
                out.row("Asked", what);
            }
            out.row(
                "Tier",
                &format!("image {}", style.dim("· nx never changes it")),
            );
        }
    }
    out.blank();
}

/// `you added it · <why nx add routed it there>`. The recorded reason
/// already says whether an unfree package was allowed.
fn you_added(style: ui::Style, route: Option<&str>, unfree: bool) -> String {
    match route {
        Some(r) => format!("you added it {}", style.dim(&format!("· {r}"))),
        None if unfree => format!("you added it {}", style.dim("· unfree, allowed")),
        None => "you added it".to_owned(),
    }
}

fn generation_row(style: ui::Style, current: u32, added: u32) -> String {
    if added == current {
        format!("{current}")
    } else {
        format!("{current} {}", style.dim(&format!("· added in {added}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    fn text(what: &str, owner: &Owner) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, what, owner);
        buf.text()
    }

    #[test]
    fn explains_a_dependency() {
        let owner = Owner::Host {
            package: LockedPackage {
                name: "libffi-devel".into(),
                nevra: "libffi-devel-3.5.1-2.fc45.x86_64".into(),
                repo: "fedora".into(),
                sha256: String::new(),
                requested: vec!["wayland-devel".into()],
            },
            current: 3,
            added: 2,
            route: None,
        };
        assert_eq!(
            text("/usr/include/ffi.h", &owner),
            "\n\
             \x20 libffi-devel 3.5.1-2.fc45\n\
             \n\
             \x20 Asked       /usr/include/ffi.h\n\
             \x20 Tier        host · Fedora RPM on the image's /usr\n\
             \x20 Repo        fedora\n\
             \x20 Why         needed by wayland-devel\n\
             \x20 Generation  3 · added in 2\n\
             \n"
        );
    }

    #[test]
    fn says_the_image_owns_it() {
        let p = Package {
            name: "ripgrep".into(),
            evr: "14.1.1-3.fc45".into(),
            ..Package::default()
        };
        let out = text("rg", &Owner::Image(p));
        assert!(
            out.contains("  ripgrep 14.1.1-3.fc45  ✓ in the image\n"),
            "{out}"
        );
        assert!(out.contains("  Asked       rg\n"), "{out}");
    }

    #[test]
    fn explains_a_tool() {
        let rg = "/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0";
        let owner = Owner::Tool {
            package: ToolPackage {
                attr: "ripgrep".into(),
                name: "ripgrep".into(),
                version: "15.2.0".into(),
                outputs: vec![nx_core::ToolOutput {
                    name: "out".into(),
                    path: rg.into(),
                }],
                bins: vec![nx_core::Bin {
                    name: "rg".into(),
                    path: format!("{rg}/bin/rg"),
                }],
                ..ToolPackage::default()
            },
            command: Some("rg".into()),
            release: "nixpkgs-26.11pre1122334.e21fec321d73".into(),
            current: 4,
            added: 4,
            route: Some("a standalone tool".into()),
        };
        assert_eq!(
            text("rg", &owner),
            format!(
                "\n\
                 \x20 ripgrep 15.2.0\n\
                 \n\
                 \x20 Asked       rg\n\
                 \x20 Tier        tool · nixpkgs, sealed beside the image\n\
                 \x20 Release     nixpkgs-26.11pre1122334.e21fec321d73\n\
                 \x20 Store       {rg}\n\
                 \x20 Runs        {rg}/bin/rg\n\
                 \x20 Why         you added it · a standalone tool\n\
                 \x20 Generation  4\n\
                 \n"
            )
        );
    }

    #[test]
    fn paths_stay_relative() {
        assert_eq!(relative("/usr/bin/rg"), Some(PathBuf::from("usr/bin/rg")));
        assert_eq!(relative("/usr/../etc"), None);
    }
}
