//! `nx add`: resolves only (`--dry-run`) until the store (M2) and
//! activation (M3) land.

use anyhow::{Result, bail};
use nx_rpm::{Package, Plan, Universe};

use crate::ui::{self, Stream, Ui};

/// Refuses before anything is loaded: only `--dry-run` works yet.
pub fn check(dry_run: bool) -> Result<()> {
    if !dry_run {
        bail!("nx add can only plan for now: run nx add --dry-run");
    }
    Ok(())
}

pub fn run(ui: &mut Ui, universe: &mut Universe, specs: &[String]) -> Result<()> {
    let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
    match universe.plan(&specs)? {
        Plan::Install(packages) => {
            render(&mut ui.out, &specs, &packages);
            Ok(())
        }
        Plan::Problems(problems) => {
            bail!("can't add {}\n{}", specs.join(" "), problems.join("\n"))
        }
    }
}

/// Lists what adding `specs` installs, in columns.
fn render(out: &mut Stream, specs: &[&str], packages: &[Package]) {
    let style = out.style;
    if packages.is_empty() {
        out.line(format_args!(
            "{} {} already in the image",
            style.green(ui::SYM_DONE),
            specs.join(", ")
        ));
        return;
    }
    let column =
        |f: fn(&Package) -> &str| packages.iter().map(|p| f(p).len()).max().unwrap_or(0) + 2;
    let (name_w, evr_w, repo_w) = (column(|p| &p.name), column(|p| &p.evr), column(|p| &p.repo));
    out.blank();
    let (mut download, mut installed) = (0, 0);
    for p in packages {
        download += p.download_size;
        installed += p.install_size;
        out.line(format_args!(
            "{} {}{}{}{}",
            style.green(ui::SYM_ADDED),
            ui::pad(&p.name, name_w),
            style.dim(&ui::pad(&p.evr, evr_w)),
            style.dim(&ui::pad(&p.repo, repo_w)),
            style.dim(&ui::size(p.download_size)),
        ));
    }
    out.blank();
    out.line(style.dim(&format!(
        "{} · {} download, {} installed · dry run, nothing changed",
        ui::plural(packages.len(), "package"),
        ui::size(download),
        ui::size(installed)
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    fn text(specs: &[&str], packages: &[Package]) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, specs, packages);
        buf.text()
    }

    fn package(name: &str, evr: &str, repo: &str, kib: u64) -> Package {
        Package {
            name: name.into(),
            evr: evr.into(),
            repo: repo.into(),
            download_size: kib * 1024,
            install_size: 4 * kib * 1024,
            ..Package::default()
        }
    }

    #[test]
    fn lists_packages_in_columns_with_totals() {
        let plan = [
            package("libxkbcommon-devel", "1.11.0-1.fc45", "fedora", 60),
            package("pkgconf", "2.4-1.fc45", "updates-archive", 40),
        ];
        assert_eq!(
            text(&["libxkbcommon-devel"], &plan),
            "\n\
             \x20 + libxkbcommon-devel  1.11.0-1.fc45  fedora           60 KiB\n\
             \x20 + pkgconf             2.4-1.fc45     updates-archive  40 KiB\n\
             \n\
             \x20 2 packages · 100 KiB download, 400 KiB installed · dry run, nothing changed\n"
        );
    }

    #[test]
    fn nothing_to_add() {
        assert_eq!(text(&["glibc"], &[]), "  ✓ glibc already in the image\n");
    }
}
