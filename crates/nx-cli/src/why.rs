//! `nx why <name|path>`: who provides a package, command or file, and the
//! generation that added it.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use nx_core::{Generation, LockedPackage, State};
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
    let opts = machine.options(Refresh::IfMissing)?;
    let image = Universe::load_image(&opts).context("reading the image's packages")?;
    let found = if what.contains('/') {
        image.image_owners(what)?.into_iter().next()
    } else {
        match image.info(what)? {
            Some(p) => Some(p),
            None => image
                .image_owners(&format!("/usr/bin/{what}"))?
                .into_iter()
                .next(),
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
/// the extension, or a command in `/usr/bin`.
fn in_generation(state: &State, g: &Generation, what: &str) -> Result<Option<Owner>> {
    let found = if what.contains('/') {
        owner_of_path(state, g, what)
    } else {
        g.lock
            .package(what)
            .cloned()
            .or_else(|| owner_of_path(state, g, &format!("/usr/bin/{what}")))
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
    Ok(Some(Owner::Host {
        package,
        current: g.number,
        added,
    }))
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
                "you added it".to_owned()
            } else {
                format!("needed by {}", package.requested.join(", "))
            };
            out.row("Why", &reason);
            let generation = if added == current {
                format!("{current}")
            } else {
                format!("{current} {}", style.dim(&format!("· added in {added}")))
            };
            out.row("Generation", &generation);
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
    fn paths_stay_relative() {
        assert_eq!(relative("/usr/bin/rg"), Some(PathBuf::from("usr/bin/rg")));
        assert_eq!(relative("/usr/../etc"), None);
    }
}
