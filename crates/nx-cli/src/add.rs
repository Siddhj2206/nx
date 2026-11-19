//! `nx add`: routes each spec to a tier (design, *Tier routing*), resolves
//! the manifest plus the new packages, fetches what the stores lack,
//! builds the next generation and makes it live through nxd.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use nx_core::{CliTools, ScriptletAllow};
use nx_rpm::{Package, Refresh};

use crate::apply::Session;
use crate::host::Machine;
use crate::route::{self, Forced, Nixpkg, Route, Rpm};
use crate::tool::{self, Tier};
use crate::ui::{self, Stream, Ui};

/// What `nx add` was told besides the packages.
#[derive(Debug, Default)]
pub struct Options {
    pub dry_run: bool,
    pub allow_scriptlets: Vec<String>,
    pub refresh: Option<Refresh>,
    pub forced: Option<Forced>,
    pub allow_unfree: bool,
    pub overrides: Vec<String>,
}

/// A tool-tier package the command adds.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolAdded {
    attr: String,
    version: String,
}

pub fn run(ui: &mut Ui, machine: &Machine, specs: &[String], opts: &Options) -> Result<()> {
    let started = Instant::now();
    let allow: Vec<ScriptletAllow> = opts
        .allow_scriptlets
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
    if opts.forced == Some(Forced::Tool) && !tool::available() {
        bail!(
            "the tool tier needs Nix in the image, and {} is missing",
            nx_nix::STORE
        );
    }
    let refresh = opts.refresh.unwrap_or(Refresh::IfMissing);
    let mut session = Session::open(ui, machine, refresh, opts.dry_run, |_| Ok(()))?;
    let mut manifest = session.manifest.clone();
    for a in allow {
        if !manifest.scriptlets.contains(&a) {
            manifest.scriptlets.push(a);
        }
    }
    for o in &opts.overrides {
        if !manifest.overrides.names.contains(o) {
            manifest.overrides.names.push(o.clone());
        }
    }

    // Where each new spec goes.
    let use_tool = opts.forced != Some(Forced::Host)
        && tool::available()
        && (manifest.routing.cli_tools == CliTools::Nix || opts.forced == Some(Forced::Tool));
    let mut tier: Option<Tier> = None;
    let (mut fresh_host, mut fresh_tool, mut problems) = (Vec::new(), Vec::new(), Vec::new());
    for spec in specs {
        if manifest.has(spec) {
            continue;
        }
        let rpm = if route::host_only(spec) {
            None
        } else {
            session.universe.info(spec)?.map(|p| Rpm {
                plugs: session.universe.plugs_in(&p),
                name: p.name,
                installed: p.installed,
            })
        };
        let (mut nix, mut similar) = (None, Vec::new());
        if use_tool && !route::host_only(spec) {
            let t = match tier.as_mut() {
                Some(t) => t,
                None => tier.insert(Tier::open(ui, session.tool.as_ref(), false)?),
            };
            let catalog = t.catalog(ui)?;
            nix = catalog.find(spec).map(|p| Nixpkg {
                attr: p.attr.clone(),
                pname: p.pname.clone(),
                unfree: p.unfree,
                allowed: opts.allow_unfree || manifest.tool.unfree.contains(&p.pname),
                broken: p.broken || p.unavailable,
            });
            if rpm.is_none() && nix.is_none() {
                similar = catalog.similar(spec, 3);
            }
        }
        let route = route::decide(
            spec,
            opts.forced,
            manifest.routing.cli_tools,
            rpm.as_ref(),
            nix.as_ref(),
            &similar,
        );
        match route {
            // Without the catalog, the solver has the last word on a name.
            Route::Refuse(_) if !use_tool => fresh_host.push(spec.clone()),
            Route::Host(_) => fresh_host.push(spec.clone()),
            Route::Tool { attr, .. } => {
                if let Some(n) = nix.as_ref().filter(|n| n.unfree)
                    && !manifest.tool.unfree.contains(&n.pname)
                {
                    manifest.tool.unfree.push(n.pname.clone());
                }
                if !manifest.tool.packages.contains(&attr) && !fresh_tool.contains(&attr) {
                    fresh_tool.push(attr);
                }
            }
            Route::Refuse(why) => problems.push(why),
        }
    }
    if !problems.is_empty() {
        bail!(problems.join("\n"));
    }
    manifest.add_host(&fresh_host);
    manifest.add_tool(&fresh_tool);
    let resolved = session.resolve_manifest(ui, &manifest.host.packages, specs)?;

    // A new spec nothing was resolved for is in the image already. It stays
    // in the manifest all the same (design invariant 8): intent, so it comes
    // back if the image drops it.
    let in_image: Vec<&str> = fresh_host
        .iter()
        .filter(|s| !resolved.all.iter().any(|p| p.requested.contains(s)))
        .map(String::as_str)
        .collect();

    let mut tool_added = Vec::new();
    let mut tool_download = 0;
    if let Some(t) = tier.as_ref().filter(|_| !fresh_tool.is_empty()) {
        let system = tool::system(session.universe.arch());
        let locked = t.resolve(ui, &manifest, session.tool.as_ref(), &system)?;
        let plan = t.plan(&locked, &manifest.caches)?;
        tool_download = plan.sum.download;
        tool_added = fresh_tool
            .iter()
            .filter_map(|a| locked.package(a))
            .map(|p| ToolAdded {
                attr: p.attr.clone(),
                version: p.version.clone(),
            })
            .collect();
        if !opts.dry_run {
            t.realise(ui, &locked, &plan, &manifest, &system)?;
        }
        session.tool = Some(locked);
        session.fresh_tools.clone_from(&fresh_tool);
    }

    if opts.dry_run {
        render_plan(
            &mut ui.out,
            &in_image,
            &resolved.new,
            &tool_added,
            tool_download,
        );
        return Ok(());
    }
    if fresh_host.is_empty() && fresh_tool.is_empty() {
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
        &Added {
            packages: &resolved.new,
            tools: &tool_added,
            tool_download,
            in_image: &in_image,
        },
        built.generation.number,
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

/// `--dry-run`: what adding would install, in columns.
fn render_plan(
    out: &mut Stream,
    in_image: &[&str],
    packages: &[Package],
    tools: &[ToolAdded],
    tool_download: u64,
) {
    let style = out.style;
    if packages.is_empty() && tools.is_empty() {
        render_nothing(out, &[], in_image);
        return;
    }
    let rows: Vec<[String; 4]> = packages
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                p.evr.clone(),
                p.repo.clone(),
                ui::size(p.download_size),
            ]
        })
        .chain(tools.iter().map(|t| {
            [
                t.attr.clone(),
                t.version.clone(),
                "nixpkgs".to_owned(),
                String::new(),
            ]
        }))
        .collect();
    let column = |i: usize| rows.iter().map(|r| r[i].len()).max().unwrap_or(0) + 2;
    let (name_w, evr_w, repo_w) = (column(0), column(1), column(2));
    out.blank();
    for r in &rows {
        // A tool package's size is in the total: the closure is shared.
        let tail = if r[3].is_empty() {
            style.dim(&r[2])
        } else {
            style.dim(&ui::pad(&r[2], repo_w)) + &style.dim(&r[3])
        };
        out.line(format_args!(
            "{} {}{}{tail}",
            style.green(ui::SYM_ADDED),
            ui::pad(&r[0], name_w),
            style.dim(&ui::pad(&r[1], evr_w)),
        ));
    }
    out.blank();
    let download: u64 = packages.iter().map(|p| p.download_size).sum::<u64>() + tool_download;
    let installed: u64 = packages.iter().map(|p| p.install_size).sum();
    let mut summary = format!(
        "{} · {} download",
        ui::plural(rows.len(), "package"),
        ui::size(download)
    );
    if !packages.is_empty() {
        summary.push_str(&format!(", {} installed", ui::size(installed)));
    }
    summary.push_str(" · dry run, nothing changed");
    out.line(style.dim(&summary));
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

/// What an install added.
struct Added<'a> {
    packages: &'a [Package],
    tools: &'a [ToolAdded],
    tool_download: u64,
    in_image: &'a [&'a str],
}

/// After an install: one `+` line per new package, a line for specs the
/// image already provides, then the total.
fn render_added(out: &mut Stream, added: &Added, generation: u32, secs: f64) {
    let style = out.style;
    let size: u64 =
        added.packages.iter().map(|p| p.download_size).sum::<u64>() + added.tool_download;
    for p in added.packages {
        out.line(format_args!(
            "{} {} {}",
            style.green(ui::SYM_ADDED),
            p.name,
            style.dim(&p.evr)
        ));
    }
    for t in added.tools {
        out.line(format_args!(
            "{} {} {}",
            style.green(ui::SYM_ADDED),
            t.attr,
            style.dim(&format!("{} · nixpkgs", t.version))
        ));
    }
    if !added.in_image.is_empty() {
        out.line(format_args!(
            "{} {} {}",
            style.green(ui::SYM_DONE),
            added.in_image.join(", "),
            style.dim("already in the image · kept in the manifest in case the image drops it")
        ));
    }
    let count = added.packages.len() + added.tools.len();
    if count == 0 {
        out.line(format_args!(
            "{} manifest updated · generation {generation} in {secs:.1}s",
            style.green(ui::SYM_DONE)
        ));
        return;
    }
    out.line(format_args!(
        "{} {} · generation {generation} · {} in {secs:.1}s",
        style.green(ui::SYM_DONE),
        ui::plural(count, "package") + " added",
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
            text(|out| render_plan(out, &[], &plan, &[], 0)),
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
            text(|out| render_plan(out, &["glibc"], &[], &[], 0)),
            "  ✓ glibc already in the image\n"
        );
        let specs = ["glibc".to_owned(), "wayland-devel".to_owned()];
        assert_eq!(
            text(|out| render_nothing(out, &specs, &["glibc"])),
            "  ✓ glibc already in the image\n  ✓ wayland-devel already added\n"
        );
    }

    fn with<'a>(
        packages: &'a [Package],
        tools: &'a [ToolAdded],
        in_image: &'a [&'a str],
    ) -> Added<'a> {
        Added {
            packages,
            tools,
            tool_download: 0,
            in_image,
        }
    }

    #[test]
    fn tool_packages_join_the_plan_and_the_report() {
        let rg = [ToolAdded {
            attr: "ripgrep".into(),
            version: "15.2.0".into(),
        }];
        let plan = [package("pkgconf", "2.4-1.fc45", "fedora", 40)];
        assert_eq!(
            text(|out| render_plan(out, &[], &plan, &rg, 2 << 20)),
            "\n\
             \x20 + pkgconf  2.4-1.fc45  fedora   40 KiB\n\
             \x20 + ripgrep  15.2.0      nixpkgs\n\
             \n\
             \x20 2 packages · 2.0 MiB download, 160 KiB installed · dry run, nothing changed\n"
        );
        assert_eq!(
            text(|out| render_added(
                out,
                &Added {
                    tool_download: 2 << 20,
                    ..with(&[], &rg, &[])
                },
                5,
                3.0
            )),
            "  + ripgrep 15.2.0 · nixpkgs\n\
             \x20 ✓ 1 package added · generation 5 · 2.0 MiB in 3.0s\n"
        );
    }

    #[test]
    fn reports_what_was_added() {
        let added = [package("libxkbcommon-devel", "1.13.1-4.fc45", "koji", 74)];
        assert_eq!(
            text(|out| render_added(out, &with(&added, &[], &[]), 3, 1.24)),
            "  + libxkbcommon-devel 1.13.1-4.fc45\n\
             \x20 ✓ 1 package added · generation 3 · 74 KiB in 1.2s\n"
        );
        assert_eq!(
            text(|out| render_added(out, &with(&[], &[], &["glibc"]), 4, 0.5)),
            "  ✓ glibc already in the image · kept in the manifest in case the image drops it\n\
             \x20 ✓ manifest updated · generation 4 in 0.5s\n"
        );
    }
}
