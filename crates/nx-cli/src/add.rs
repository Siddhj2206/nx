//! `nx add`: routes each spec to a tier (design, *Tier routing*), resolves
//! the manifest plus the new packages, fetches what the stores lack,
//! builds the next generation and makes it live through nxd.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use nx_core::{CliTools, ScriptletAllow};
use nx_rpm::{Package, Refresh};

use crate::changes::{self, Change};
use crate::host::Machine;
use crate::route::{self, Forced, Found, Nixpkg, Route, Rpm};
use crate::session::{How, Session};
use crate::tool;
use crate::ui::{self, Stream, Ui};

/// What `nx add` was told besides the packages.
#[derive(Debug, Default)]
pub struct Options {
    pub dry_run: bool,
    pub allow_scriptlets: Vec<String>,
    pub refresh: Option<Refresh>,
    pub forced: Option<Forced>,
    pub overrides: Vec<String>,
    pub without: Vec<String>,
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
    let mut session = Session::open(ui, machine, refresh, opts.dry_run, |m| {
        for a in allow {
            if !m.scriptlets.contains(&a) {
                m.scriptlets.push(a);
            }
        }
        for o in &opts.overrides {
            if !m.overrides.names.contains(o) {
                m.overrides.names.push(o.clone());
            }
        }
        for w in &opts.without {
            if !m.overrides.without.contains(w) {
                m.overrides.without.push(w.clone());
            }
        }
        Ok(())
    })?;

    // Where each new spec goes.
    let routing = session.manifest().routing.clone();
    let use_tool = opts.forced != Some(Forced::Host)
        && tool::available()
        && (routing.cli_tools == CliTools::Nix || opts.forced == Some(Forced::Tool));
    let toolchains = route::toolchains(&routing);
    let (mut fresh_host, mut fresh_tool, mut problems) = (Vec::new(), Vec::new(), Vec::new());
    for spec in specs {
        if session.manifest().has(spec) {
            continue;
        }
        let mut found = Found::default();
        if !route::host_only(spec) {
            let universe = session.universe();
            found.rpm = universe.info(spec)?.map(|p| Rpm {
                plugs: universe.plugs_in(&p, &toolchains),
                name: p.name,
                installed: p.installed,
            });
        }
        if use_tool && !route::host_only(spec) {
            let catalog = session.tier(ui)?.catalog(ui)?;
            found.nix = catalog.find(spec).map(|p| Nixpkg::new(p, &toolchains));
            if found.rpm.is_none() && found.nix.is_none() {
                found.similar = catalog.similar(spec, 3);
            }
        }
        let route = route::decide(spec, opts.forced, routing.cli_tools, &found);
        match route {
            // Without the catalog, the solver has the last word on a name.
            Route::Refuse(_) if !use_tool => {
                let why = if tool::available() {
                    "routing.cli_tools is fedora"
                } else {
                    "the image has no Nix"
                };
                session.route(spec, why.into());
                fresh_host.push(spec.clone());
            }
            Route::Host(why) => {
                session.route(spec, why);
                fresh_host.push(spec.clone());
            }
            Route::Tool { attr, reason } => {
                session.route(&attr, reason);
                if !session.manifest().tool.packages.contains(&attr) && !fresh_tool.contains(&attr)
                {
                    fresh_tool.push(attr);
                }
            }
            Route::Refuse(why) => problems.push(why),
        }
    }
    if !problems.is_empty() {
        bail!(problems.join("\n"));
    }
    if fresh_host.is_empty()
        && fresh_tool.is_empty()
        && *session.manifest() == current_manifest(&session)
    {
        render_nothing(&mut ui.out, specs, &[]);
        return Ok(());
    }
    session.edit(|m| {
        m.add_host(&fresh_host);
        m.add_tool(&fresh_tool);
    })?;
    let outcome = session.plan(
        ui,
        &How {
            asked: specs,
            ..How::default()
        },
    )?;

    // A new spec nothing was resolved for is in the image already. It stays
    // in the manifest all the same (design invariant 8): intent, so it comes
    // back if the image drops it.
    let in_image: Vec<&str> = fresh_host
        .iter()
        .filter(|s| !outcome.all.iter().any(|p| p.requested.contains(s)))
        .map(String::as_str)
        .collect();
    if opts.dry_run {
        let tools: Vec<ToolAdded> = fresh_tool
            .iter()
            .filter_map(|a| session.tool()?.package(a))
            .map(|p| ToolAdded {
                attr: p.attr.clone(),
                version: p.version.clone(),
            })
            .collect();
        render_plan(
            &mut ui.out,
            &in_image,
            &outcome.new_host,
            &tools,
            outcome.tool_download,
        );
        return Ok(());
    }
    let built = session.install(ui, &outcome.all)?;
    render_added(
        &mut ui.out,
        &outcome.changes,
        &in_image,
        &changes::Done {
            generation: Some(built.generation.number),
            download: outcome.download(),
            secs: started.elapsed().as_secs_f64(),
            ..changes::Done::default()
        },
    );
    Ok(())
}

/// The manifest the current generation has, to tell an allowance or an
/// override from nothing at all.
fn current_manifest(session: &Session) -> nx_core::Manifest {
    session
        .current()
        .map(|g| g.manifest.clone())
        .unwrap_or_default()
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

/// After an install: one line per package that changed, a line for specs
/// the image already provides, then the total.
fn render_added(out: &mut Stream, changes: &[Change], in_image: &[&str], done: &changes::Done) {
    changes::lines(out, changes);
    if !in_image.is_empty() {
        let style = out.style;
        out.line(format_args!(
            "{} {} {}",
            style.green(ui::SYM_DONE),
            in_image.join(", "),
            style.dim("already in the image · kept in the manifest in case the image drops it")
        ));
    }
    changes::summary(out, changes, done);
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

    #[test]
    fn tool_packages_join_the_plan() {
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
    }

    #[test]
    fn says_what_is_in_the_image_before_the_total() {
        let done = changes::Done {
            generation: Some(4),
            secs: 0.5,
            ..changes::Done::default()
        };
        assert_eq!(
            text(|out| render_added(out, &[], &["glibc"], &done)),
            "  ✓ glibc already in the image · kept in the manifest in case the image drops it\n\
             \x20 ✓ manifest updated · generation 4 in 0.5s\n"
        );
    }
}
