//! `nx info`: one host-tier package in the `info` layout.

use anyhow::{Result, bail};
use nx_rpm::{Package, Universe};

use crate::ui::{self, Stream, Ui};

pub fn run(ui: &mut Ui, universe: &Universe, spec: &str) -> Result<()> {
    let Some(p) = universe.info(spec)? else {
        bail!("no package matches {spec} (try nx search {spec})");
    };
    render(&mut ui.out, spec, &p, &universe.description(&p));
    Ok(())
}

/// Shows `p`, which the user asked for as `spec`.
fn render(out: &mut Stream, spec: &str, p: &Package, description: &str) {
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
    // Only host-tier packages exist until the tool tier (M6).
    out.row(
        "Tier",
        &format!("host {}", style.dim("· Fedora RPM on the image's /usr")),
    );
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
        render(&mut ui.out, spec, p, description);
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
