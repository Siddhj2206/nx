//! `nx add`: resolves the manifest plus the new packages, downloads and
//! unpacks what the store lacks, builds the next generation and makes it
//! live through nxd.

use std::time::Instant;

use anyhow::{Context, Result};
use nx_core::ScriptletAllow;
use nx_rpm::{Package, Refresh};

use crate::apply::Session;
use crate::host::Machine;
use crate::ui::{self, Stream, Ui};

pub fn run(
    ui: &mut Ui,
    machine: &Machine,
    specs: &[String],
    dry_run: bool,
    allow: &[String],
    refresh: Refresh,
) -> Result<()> {
    let started = Instant::now();
    let allow: Vec<ScriptletAllow> = allow
        .iter()
        .map(|a| {
            ScriptletAllow::parse(a).with_context(|| {
                format!(
                    "--allow-scriptlets wants package=digest ({} or more hex digits), not {a}",
                    nx_core::manifest::ALLOW_DIGEST_LEN
                )
            })
        })
        .collect::<Result<_>>()?;
    let mut session = Session::open(ui, machine, refresh, dry_run, |_| Ok(()))?;
    let mut manifest = session.manifest.clone();
    for a in allow {
        if !manifest.scriptlets.contains(&a) {
            manifest.scriptlets.push(a);
        }
    }
    let fresh: Vec<String> = specs
        .iter()
        .filter(|s| !manifest.host.packages.contains(s))
        .cloned()
        .collect();
    manifest.add_host(&fresh);
    let resolved = session.resolve(ui, &manifest.host.packages)?;

    // A new spec nothing was resolved for is in the image already. It stays
    // in the manifest all the same (design invariant 8): intent, so it comes
    // back if the image drops it.
    let in_image: Vec<&str> = fresh
        .iter()
        .filter(|s| !resolved.all.iter().any(|p| p.requested.contains(s)))
        .map(String::as_str)
        .collect();

    if dry_run {
        render_plan(&mut ui.out, &in_image, &resolved.new);
        return Ok(());
    }
    if fresh.is_empty() {
        render_nothing(&mut ui.out, specs, &[]);
        return Ok(());
    }
    let built = session.install(ui, &manifest, &resolved.all)?;
    for name in &built.allowed {
        ui.warn(format_args!(
            "{name}: scriptlets nx can't translate are skipped, as you allowed"
        ));
    }
    for nevra in &built.outside_usr {
        ui.warn(format_args!(
            "{nevra} has files outside /usr, which the extension leaves out"
        ));
    }
    render_added(
        &mut ui.out,
        &resolved.new,
        &in_image,
        built.generation.number,
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

/// `--dry-run`: what adding would install, in columns.
fn render_plan(out: &mut Stream, in_image: &[&str], packages: &[Package]) {
    let style = out.style;
    if packages.is_empty() {
        render_nothing(out, &[], in_image);
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

/// Nothing to install: say why for each spec.
fn render_nothing(out: &mut Stream, specs: &[String], in_image: &[&str]) {
    let style = out.style;
    let sym = style.green(ui::SYM_DONE);
    if !in_image.is_empty() {
        out.line(format_args!(
            "{sym} {} already in the image",
            in_image.join(", ")
        ));
    }
    let added: Vec<&str> = specs
        .iter()
        .map(String::as_str)
        .filter(|s| !in_image.contains(s))
        .collect();
    if !added.is_empty() {
        out.line(format_args!("{sym} {} already added", added.join(", ")));
    }
    if in_image.is_empty() && added.is_empty() {
        out.line(format_args!("{sym} nothing to add"));
    }
}

/// After an install: one `+` line per new package, a line for specs the
/// image already provides, then the total.
fn render_added(
    out: &mut Stream,
    packages: &[Package],
    in_image: &[&str],
    generation: u32,
    secs: f64,
) {
    let style = out.style;
    let size: u64 = packages.iter().map(|p| p.download_size).sum();
    for p in packages {
        out.line(format_args!(
            "{} {} {}",
            style.green(ui::SYM_ADDED),
            p.name,
            style.dim(&p.evr)
        ));
    }
    if !in_image.is_empty() {
        out.line(format_args!(
            "{} {} {}",
            style.green(ui::SYM_DONE),
            in_image.join(", "),
            style.dim("already in the image · kept in the manifest in case the image drops it")
        ));
    }
    if packages.is_empty() {
        out.line(format_args!(
            "{} manifest updated · generation {generation} in {secs:.1}s",
            style.green(ui::SYM_DONE)
        ));
        return;
    }
    out.line(format_args!(
        "{} {} · generation {generation} · {} in {secs:.1}s",
        style.green(ui::SYM_DONE),
        ui::plural(packages.len(), "package") + " added",
        ui::size(size)
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    fn text(f: impl FnOnce(&mut Stream)) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        f(&mut ui.out);
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
    fn plans_in_columns_with_totals() {
        let plan = [
            package("libxkbcommon-devel", "1.11.0-1.fc45", "fedora", 60),
            package("pkgconf", "2.4-1.fc45", "updates-archive", 40),
        ];
        assert_eq!(
            text(|out| render_plan(out, &[], &plan)),
            "\n\
             \x20 + libxkbcommon-devel  1.11.0-1.fc45  fedora           60 KiB\n\
             \x20 + pkgconf             2.4-1.fc45     updates-archive  40 KiB\n\
             \n\
             \x20 2 packages · 100 KiB download, 400 KiB installed · dry run, nothing changed\n"
        );
    }

    #[test]
    fn says_why_there_is_nothing_to_add() {
        assert_eq!(
            text(|out| render_plan(out, &["glibc"], &[])),
            "  ✓ glibc already in the image\n"
        );
        let specs = ["glibc".to_owned(), "wayland-devel".to_owned()];
        assert_eq!(
            text(|out| render_nothing(out, &specs, &["glibc"])),
            "  ✓ glibc already in the image\n  ✓ wayland-devel already added\n"
        );
    }

    #[test]
    fn reports_what_was_added() {
        let added = [package("libxkbcommon-devel", "1.13.1-4.fc45", "koji", 74)];
        assert_eq!(
            text(|out| render_added(out, &added, &[], 3, 1.24)),
            "  + libxkbcommon-devel 1.13.1-4.fc45\n\
             \x20 ✓ 1 package added · generation 3 · 74 KiB in 1.2s\n"
        );
        assert_eq!(
            text(|out| render_added(out, &[], &["glibc"], 4, 0.5)),
            "  ✓ glibc already in the image · kept in the manifest in case the image drops it\n\
             \x20 ✓ manifest updated · generation 4 in 0.5s\n"
        );
    }
}
