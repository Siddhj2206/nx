//! `nx info`: one package in the `info` layout, from the tier `nx add`
//! would put it in, with the reason (design, *Tier routing*).

use anyhow::Result;
use nx_core::{Generation, State};
use nx_nix::catalog::Package as Nixpkg;
use nx_nix::{Catalog, Nix};
use nx_rpm::{Package, Universe};
use nx_scriptlet::Leftover;

use crate::route::{self, Found, Route, Rpm};
use crate::ui::{self, Problem, Stream, Ui};

/// What nx knows about the package beyond its metadata.
#[derive(Debug, Default)]
struct Local {
    /// The version installed and the generation that added it.
    installed: Option<(String, u32)>,
    /// A host package's scriptlets nx doesn't translate, from its record in
    /// the store; whether the manifest allows skipping them.
    scriptlets: Option<(bool, Vec<Leftover>)>,
    /// An installed tool's size and closure.
    size: Option<nx_nix::nix::Size>,
}

pub fn run(
    ui: &mut Ui,
    universe: &Universe,
    catalog: Option<&Catalog>,
    current: Option<&Generation>,
    state: &State,
    nix: &dyn Nix,
    spec: &str,
) -> Result<()> {
    let manifest = current.map(|g| g.manifest.clone()).unwrap_or_default();
    let rpm = universe.info(spec)?;
    let nix_package = catalog
        .filter(|_| !route::host_only(spec))
        .and_then(|c| c.find(spec));
    let toolchains = route::toolchains(&manifest.routing);
    let found = Found {
        rpm: rpm.as_ref().map(|p| Rpm {
            name: p.name.clone(),
            installed: p.installed,
            plugs: universe.plugs_in(p, &toolchains),
        }),
        nix: nix_package.map(|p| route::Nixpkg::new(p, &toolchains)),
        similar: Vec::new(),
    };
    let route = route::decide(spec, None, manifest.routing.cli_tools, &found);
    let release = catalog.map(|c| c.release.clone()).unwrap_or_default();
    let installed = |name: &str| current.and_then(|g| crate::why::installed(state, g, name));
    match (route, rpm, nix_package) {
        (Route::Tool { reason, .. }, _, Some(n)) => {
            let local = tool_local(current, nix, &n.attr, installed(&n.attr));
            render_nix(&mut ui.out, spec, n, &release, Ok(&reason), &local);
        }
        (Route::Host(reason), Some(p), _) => {
            let local = host_local(current, state, &p, installed(&p.name));
            render(
                &mut ui.out,
                spec,
                &p,
                &universe.description(&p),
                Some(&reason),
                &local,
            );
        }
        (Route::Refuse(why), _, Some(n)) => {
            render_nix(&mut ui.out, spec, n, &release, Err(&why), &Local::default());
        }
        _ => {
            return Err(Problem::new(format!("no package matches {spec}"))
                .fix("Search for it", format!("nx search {spec}"))
                .into());
        }
    }
    Ok(())
}

/// A host package's install and scriptlets. The scriptlets come from the
/// store's record of the build installed, else of the build the repos
/// have, which a refused `nx add` left there.
fn host_local(
    current: Option<&Generation>,
    state: &State,
    p: &Package,
    installed: Option<(String, u32)>,
) -> Local {
    let sha = current
        .and_then(|g| g.lock.package(&p.name))
        .map_or(p.sha256.as_str(), |l| l.sha256.as_str());
    let leftovers = state
        .store()
        .meta(sha)
        .ok()
        .flatten()
        .map(|meta| {
            let ctx = nx_scriptlet::Context {
                present: &|_| true,
                removable: &|_| false,
            };
            nx_scriptlet::translate(&meta.scripts, &ctx).leftovers
        })
        .filter(|l| !l.is_empty());
    let allowed =
        current.is_some_and(|g| g.manifest.scriptlets.iter().any(|a| a.package == p.name));
    Local {
        installed,
        scriptlets: leftovers.map(|l| (allowed, l)),
        size: None,
    }
}

/// An installed tool's size, from the local store.
fn tool_local(
    current: Option<&Generation>,
    nix: &dyn Nix,
    attr: &str,
    installed: Option<(String, u32)>,
) -> Local {
    let paths: Vec<String> = current
        .and_then(|g| g.lock.tool.as_ref()?.package(attr))
        .map(|p| p.paths().map(str::to_owned).collect())
        .unwrap_or_default();
    let size = (!paths.is_empty() && paths.iter().all(|p| nix.has(p)))
        .then(|| nx_nix::nix::size(nix, &paths).ok())
        .flatten();
    Local {
        installed,
        scriptlets: None,
        size,
    }
}

/// Shows nixpkgs' `p`, which the user asked for as `spec`; `route` is why
/// it would go to the tool tier, or why it can't.
fn render_nix(
    out: &mut Stream,
    spec: &str,
    p: &Nixpkg,
    release: &str,
    route: Result<&str, &str>,
    local: &Local,
) {
    let style = out.style;
    out.blank();
    out.line(format_args!(
        "{} {}",
        style.bold(&p.attr),
        style.dim(&p.version)
    ));
    if !p.description.is_empty() {
        out.line(&p.description);
    }
    if !p.homepage.is_empty() {
        out.line(style.cyan(&p.homepage));
    }
    out.blank();
    if p.attr != spec {
        out.row(
            "Matched",
            &format!(
                "{spec} {}",
                style.dim(&format!("(the command of {})", p.attr))
            ),
        );
    }
    installed_row(out, local);
    match route {
        Ok(why) => {
            out.row(
                "Tier",
                &format!("tool {}", style.dim("· nixpkgs, sealed beside the image")),
            );
            out.row("Why", why);
        }
        Err(why) => out.row("Tier", &style.yellow(why)),
    }
    if !release.is_empty() {
        out.row("Release", release);
    }
    if !p.license.is_empty() {
        out.row("License", &p.license);
    }
    if !p.main_program.is_empty() {
        out.row("Runs", &p.main_program);
    }
    if let Some(size) = &local.size {
        out.row(
            "Size",
            &format!(
                "{} {}",
                ui::size(size.own),
                style.dim(&format!(
                    "· closure {} in {}",
                    ui::size(size.closure),
                    ui::plural(size.closure_paths, "store path")
                ))
            ),
        );
    }
    out.blank();
}

/// `Installed  1.26.0-3.fc45 · added in generation 63`.
fn installed_row(out: &mut Stream, local: &Local) {
    if let Some((version, added)) = &local.installed {
        let style = out.style;
        out.row(
            "Installed",
            &format!(
                "{version} {}",
                style.dim(&format!("· added in generation {added}"))
            ),
        );
    }
}

/// Shows `p`, which the user asked for as `spec`; `why` is the routing
/// reason.
fn render(
    out: &mut Stream,
    spec: &str,
    p: &Package,
    description: &str,
    why: Option<&str>,
    local: &Local,
) {
    let style = out.style;
    out.blank();
    let mut head = format!("{} {}", style.bold(&p.name), style.dim(&p.evr));
    if p.installed {
        head.push_str("  ");
        head.push_str(&ui::in_image(style));
    }
    out.line(head);
    if !p.summary.is_empty() {
        out.line(&p.summary);
    }
    if !p.url.is_empty() {
        out.line(style.cyan(&p.url));
    }
    out.blank();
    if p.name != spec {
        out.row(
            "Matched",
            &format!("{spec} {}", style.dim(&format!("(provided by {})", p.name))),
        );
    }
    installed_row(out, local);
    out.row(
        "Tier",
        &format!("host {}", style.dim("· Fedora RPM on the image's /usr")),
    );
    if let Some(why) = why {
        out.row("Why", why);
    }
    out.row("Repo", if p.installed { "the image" } else { &p.repo });
    out.row("Source", &p.source);
    out.row("License", &p.license);
    out.row("Arch", &p.arch);
    if !p.installed {
        out.row(
            "Size",
            &format!(
                "{} {}",
                ui::size(p.download_size),
                style.dim(&format!("· {} installed", ui::size(p.install_size)))
            ),
        );
    }
    if let Some((allowed, leftovers)) = &local.scriptlets {
        let does: Vec<String> = leftovers
            .iter()
            .map(|l| {
                let what = nx_scriptlet::describe(l);
                if what.is_empty() {
                    l.source.clone()
                } else {
                    format!("{} {}", l.source, what.join(", "))
                }
            })
            .collect();
        let state = if *allowed {
            style.dim("· skipped, as you allowed")
        } else {
            style.yellow("· nx refuses it until you allow them")
        };
        out.row("Scriptlets", &format!("{} {state}", does.join("; ")));
    }
    if !description.is_empty() && description != p.summary {
        out.blank();
        for line in description.lines().take(8) {
            out.line(style.dim(line.trim_end()));
        }
    }
    // The scripts in full: what --allow-scriptlets would skip.
    if let Some((_, leftovers)) = &local.scriptlets {
        for l in leftovers {
            out.blank();
            out.line(style.bold(&l.source));
            for line in &l.lines {
                out.line(format_args!("  {}", style.dim(line)));
            }
        }
    }
    out.blank();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    fn text(spec: &str, p: &Package, description: &str) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, spec, p, description, None, &Local::default());
        buf.text()
    }

    fn devel() -> Package {
        Package {
            name: "libxkbcommon-devel".into(),
            evr: "1.11.0-1.fc45".into(),
            arch: "x86_64".into(),
            repo: "fedora".into(),
            summary: "Development files for libxkbcommon".into(),
            source: "libxkbcommon-1.11.0-1.fc45.src.rpm".into(),
            license: "MIT".into(),
            url: "https://xkbcommon.org".into(),
            download_size: 60 * 1024,
            install_size: 300 * 1024,
            ..Package::default()
        }
    }

    #[test]
    fn renders_an_available_package_matched_by_provide() {
        assert_eq!(
            text(
                "pkgconfig(xkbcommon)",
                &devel(),
                "Headers.\nAnd pkg-config files."
            ),
            "\n\
             \x20 libxkbcommon-devel 1.11.0-1.fc45\n\
             \x20 Development files for libxkbcommon\n\
             \x20 https://xkbcommon.org\n\
             \n\
             \x20 Matched     pkgconfig(xkbcommon) (provided by libxkbcommon-devel)\n\
             \x20 Tier        host · Fedora RPM on the image's /usr\n\
             \x20 Repo        fedora\n\
             \x20 Source      libxkbcommon-1.11.0-1.fc45.src.rpm\n\
             \x20 License     MIT\n\
             \x20 Arch        x86_64\n\
             \x20 Size        60 KiB · 300 KiB installed\n\
             \n\
             \x20 Headers.\n\
             \x20 And pkg-config files.\n\
             \n"
        );
    }

    #[test]
    fn a_tool_says_where_it_would_go() {
        let rg = Nixpkg {
            attr: "ripgrep".into(),
            version: "15.2.0".into(),
            description: "Fast grep".into(),
            homepage: "https://github.com/BurntSushi/ripgrep".into(),
            license: "MIT".into(),
            main_program: "rg".into(),
            ..Nixpkg::default()
        };
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render_nix(
            &mut ui.out,
            "rg",
            &rg,
            "nixpkgs-26.11pre1122334.e21fec321d73",
            Ok("a standalone tool"),
            &Local {
                installed: Some(("15.2.0".into(), 63)),
                size: Some(nx_nix::nix::Size {
                    own: 7_040_576,
                    closure_paths: 8,
                    closure: 58_410_560,
                }),
                ..Local::default()
            },
        );
        assert_eq!(
            buf.text(),
            "\n\
             \x20 ripgrep 15.2.0\n\
             \x20 Fast grep\n\
             \x20 https://github.com/BurntSushi/ripgrep\n\
             \n\
             \x20 Matched     rg (the command of ripgrep)\n\
             \x20 Installed   15.2.0 · added in generation 63\n\
             \x20 Tier        tool · nixpkgs, sealed beside the image\n\
             \x20 Why         a standalone tool\n\
             \x20 Release     nixpkgs-26.11pre1122334.e21fec321d73\n\
             \x20 License     MIT\n\
             \x20 Runs        rg\n\
             \x20 Size        6.7 MiB · closure 55.7 MiB in 8 store paths\n\
             \n"
        );
    }

    #[test]
    fn shows_the_scriptlets_nx_would_skip() {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        let local = Local {
            scriptlets: Some((
                false,
                vec![Leftover {
                    source: "%post".into(),
                    lines: vec![
                        "update-alternatives --install /usr/bin/ld ld /usr/bin/ld.mold 1".into(),
                    ],
                }],
            )),
            ..Local::default()
        };
        render(&mut ui.out, "mold", &devel(), "", None, &local);
        let out = buf.text();
        assert!(
            out.contains(
                "  Scriptlets  %post makes ld.mold the system linker · nx refuses it until you allow them\n"
            ),
            "{out}"
        );
        assert!(
            out.ends_with(
                "\n  %post\n    update-alternatives --install /usr/bin/ld ld /usr/bin/ld.mold 1\n\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn an_image_package_has_no_size_and_says_so() {
        let p = Package {
            installed: true,
            ..devel()
        };
        assert_eq!(
            text("libxkbcommon-devel", &p, ""),
            "\n\
             \x20 libxkbcommon-devel 1.11.0-1.fc45  ✓ in the image\n\
             \x20 Development files for libxkbcommon\n\
             \x20 https://xkbcommon.org\n\
             \n\
             \x20 Tier        host · Fedora RPM on the image's /usr\n\
             \x20 Repo        the image\n\
             \x20 Source      libxkbcommon-1.11.0-1.fc45.src.rpm\n\
             \x20 License     MIT\n\
             \x20 Arch        x86_64\n\
             \n"
        );
    }

    #[test]
    fn a_host_package_nx_added_has_an_installed_row_and_no_scriptlets() {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        let local = Local {
            installed: Some(("1.11.0-1.fc45".into(), 12)),
            ..Local::default()
        };
        render(
            &mut ui.out,
            "libxkbcommon-devel",
            &devel(),
            "",
            Some("you asked for an RPM"),
            &local,
        );
        assert_eq!(
            buf.text(),
            "\n\
             \x20 libxkbcommon-devel 1.11.0-1.fc45\n\
             \x20 Development files for libxkbcommon\n\
             \x20 https://xkbcommon.org\n\
             \n\
             \x20 Installed   1.11.0-1.fc45 · added in generation 12\n\
             \x20 Tier        host · Fedora RPM on the image's /usr\n\
             \x20 Why         you asked for an RPM\n\
             \x20 Repo        fedora\n\
             \x20 Source      libxkbcommon-1.11.0-1.fc45.src.rpm\n\
             \x20 License     MIT\n\
             \x20 Arch        x86_64\n\
             \x20 Size        60 KiB · 300 KiB installed\n\
             \n"
        );
    }
}
