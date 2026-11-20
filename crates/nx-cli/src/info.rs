//! `nx info`: one package in the `info` layout, from the tier `nx add`
//! would put it in, with the reason (design, *Tier routing*).

use anyhow::{Result, bail};
use nx_core::Manifest;
use nx_nix::Catalog;
use nx_nix::catalog::Package as Nixpkg;
use nx_rpm::{Package, Universe};

use crate::route::{self, Found, Route, Rpm};
use crate::ui::{self, Stream, Ui};

pub fn run(
    ui: &mut Ui,
    universe: &Universe,
    catalog: Option<&Catalog>,
    manifest: &Manifest,
    spec: &str,
) -> Result<()> {
    let rpm = universe.info(spec)?;
    let nix = catalog
        .filter(|_| !route::host_only(spec))
        .and_then(|c| c.find(spec));
    let toolchains = route::toolchains(&manifest.routing);
    let found = Found {
        rpm: rpm.as_ref().map(|p| Rpm {
            name: p.name.clone(),
            installed: p.installed,
            plugs: universe.plugs_in(p, &toolchains),
        }),
        nix: nix.map(|p| route::Nixpkg::new(p, &toolchains)),
        similar: Vec::new(),
    };
    let route = route::decide(spec, None, manifest.routing.cli_tools, &found);
    let release = catalog.map(|c| c.release.clone()).unwrap_or_default();
    match (route, rpm, nix) {
        (Route::Tool { reason, .. }, _, Some(n)) => {
            render_nix(&mut ui.out, spec, n, &release, Ok(&reason));
        }
        (Route::Host(reason), Some(p), _) => {
            render(
                &mut ui.out,
                spec,
                &p,
                &universe.description(&p),
                Some(&reason),
            );
        }
        (Route::Refuse(why), _, Some(n)) => render_nix(&mut ui.out, spec, n, &release, Err(&why)),
        _ => bail!("no package matches {spec} (try nx search {spec})"),
    }
    Ok(())
}

/// Shows nixpkgs' `p`, which the user asked for as `spec`; `route` is why
/// it would go to the tool tier, or why it can't.
fn render_nix(out: &mut Stream, spec: &str, p: &Nixpkg, release: &str, route: Result<&str, &str>) {
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
    out.blank();
}

/// Shows `p`, which the user asked for as `spec`; `why` is the routing
/// reason.
fn render(out: &mut Stream, spec: &str, p: &Package, description: &str, why: Option<&str>) {
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
    if !description.is_empty() && description != p.summary {
        out.blank();
        for line in description.lines().take(8) {
            out.line(style.dim(line.trim_end()));
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
        render(&mut ui.out, spec, p, description, None);
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
        );
        assert_eq!(
            buf.text(),
            "\n\
             \x20 ripgrep 15.2.0\n\
             \x20 Fast grep\n\
             \x20 https://github.com/BurntSushi/ripgrep\n\
             \n\
             \x20 Matched     rg (the command of ripgrep)\n\
             \x20 Tier        tool · nixpkgs, sealed beside the image\n\
             \x20 Why         a standalone tool\n\
             \x20 Release     nixpkgs-26.11pre1122334.e21fec321d73\n\
             \x20 License     MIT\n\
             \x20 Runs        rg\n\
             \n"
        );
    }

    #[test]
    fn an_image_package_has_no_size_and_says_so() {
        let p = Package {
            installed: true,
            ..devel()
        };
        let out = text("libxkbcommon-devel", &p, "");
        assert!(
            out.contains("libxkbcommon-devel 1.11.0-1.fc45  ✓ in the image\n"),
            "{out}"
        );
        assert!(out.contains("  Repo        the image\n"), "{out}");
        assert!(!out.contains("Size") && !out.contains("Matched"), "{out}");
    }
}
