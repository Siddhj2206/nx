//! `nx add`: routes each spec to a tier (design, *Tier routing*), resolves
//! the manifest plus the new packages, fetches what the stores lack,
//! builds the next generation and makes it live through nxd.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use nx_core::{CliTools, ScriptletAllow};
use nx_rpm::Refresh;

use crate::changes::{self, Change};
use crate::host::Machine;
use crate::route::{self, Forced, Found, Nixpkg, Route, Rpm};
use crate::session::{How, Session};
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

/// Adds `specs` to the manifest and builds the generation, or with
/// `--dry-run` shows what that would change.
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
    let refresh = opts.refresh.unwrap_or(Refresh::IfStale);
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
    if opts.forced == Some(Forced::Tool) && !session.tool_available() {
        bail!(
            "the tool tier needs Nix in the image, and {} is missing",
            nx_nix::STORE
        );
    }

    // Where each new spec goes.
    let routing = session.manifest().routing.clone();
    let use_tool = opts.forced != Some(Forced::Host)
        && session.tool_available()
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
            let catalog = session.catalog(ui)?;
            found.nix = catalog.find(spec).map(|p| Nixpkg::new(p, &toolchains));
            if found.rpm.is_none() && found.nix.is_none() {
                found.similar = catalog.similar(spec, 3);
            }
        }
        let route = route::decide(spec, opts.forced, routing.cli_tools, &found);
        match route {
            // Without the catalog, the solver has the last word on a name.
            Route::Refuse(_) if !use_tool => {
                let why = if session.tool_available() {
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
            Route::Refuse(why) => problems.push((spec.as_str(), why)),
        }
    }
    if !problems.is_empty() {
        let names: Vec<&str> = problems.iter().map(|(spec, _)| *spec).collect();
        return Err(ui::Problem::new(format!("can't add {}", names.join(", ")))
            .reasons(problems.into_iter().map(|(_, why)| why))
            .into());
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
    if opts.dry_run && outcome.changes.is_empty() {
        render_nothing(&mut ui.out, &[], &in_image);
        return Ok(());
    }
    let generation = if opts.dry_run {
        None
    } else {
        Some(session.install(ui, &outcome.all)?.generation.number)
    };
    render(
        &mut ui.out,
        &outcome.changes,
        &in_image,
        &changes::Done {
            generation,
            moved: None,
            download: outcome.download(),
            secs: started.elapsed().as_secs_f64(),
            others: session.others(),
            head: None,
        },
    );
    Ok(())
}

fn current_manifest(session: &Session) -> nx_core::Manifest {
    session
        .current()
        .map(|g| g.manifest.clone())
        .unwrap_or_default()
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

/// The change (or for a dry run, the plan): the table, a line for specs
/// the image already provides, then the total.
fn render(out: &mut Stream, changes: &[Change], in_image: &[&str], done: &changes::Done) {
    changes::table(out, changes);
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

    #[test]
    fn says_why_there_is_nothing_to_add() {
        let specs = ["glibc".to_owned(), "wayland-devel".to_owned()];
        assert_eq!(
            text(|out| render_nothing(out, &specs, &["glibc"])),
            "  ✓ glibc already in the image\n  ✓ wayland-devel already added\n"
        );
    }

    #[test]
    fn a_dry_run_is_the_same_table() {
        let changes = [
            Change::added("pkgconf", "2.4-1.fc45").for_("libxkbcommon-devel"),
            Change::added("libxkbcommon-devel", "1.11.0-1.fc45"),
        ];
        let done = changes::Done {
            download: 100 << 10,
            ..changes::Done::default()
        };
        assert_eq!(
            text(|out| render(out, &changes, &[], &done)),
            "  + libxkbcommon-devel  1.11.0-1.fc45  with 1 dependency\n\
             \x20 2 packages to add · 100 KiB to download · dry run, nothing changed\n"
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
            text(|out| render(out, &[], &["glibc"], &done)),
            "  ✓ glibc already in the image · kept in the manifest in case the image drops it\n\
             \x20 ✓ manifest updated · 0.5s\n"
        );
    }
}
