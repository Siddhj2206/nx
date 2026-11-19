//! `nx search`: packages whose name or summary matches, from the host
//! tier's repos and then the nixpkgs catalog, laid out like nx 0.x's
//! search (name, version, mark; summary indented below).

use nx_nix::Catalog;
use nx_nix::catalog::Package as Nixpkg;
use nx_rpm::{Package, Universe};

use crate::ui::{self, Stream, Ui};

/// How many matches of each source to show before asking for a narrower
/// term.
const LIMIT: usize = 25;

pub fn run(ui: &mut Ui, universe: &Universe, catalog: Option<&Catalog>, terms: &[String]) {
    let matches = universe.search(&terms.join(" "));
    let nix = catalog.map(|c| c.search(terms)).unwrap_or_default();
    render(&mut ui.out, &matches, &nix, ui::width());
}

/// Lists `matches`, then `nix`, for a terminal `width` columns wide.
fn render(out: &mut Stream, matches: &[Package], nix: &[&Nixpkg], width: usize) {
    let style = out.style;
    if matches.is_empty() && nix.is_empty() {
        out.line("No packages match.");
        return;
    }
    // Two columns of indent on top of the stream's own two, and two to
    // spare.
    let summary = |out: &mut Stream, text: &str| {
        if !text.is_empty() {
            out.line(format_args!(
                "  {}",
                ui::truncate(text, width.saturating_sub(6))
            ));
        }
    };
    for p in matches.iter().take(LIMIT) {
        let mark = if p.installed {
            ui::in_image(style)
        } else {
            style.dim(&p.repo)
        };
        out.line(format_args!(
            "{} {}  {mark}",
            style.bold(&p.name),
            style.dim(&p.evr)
        ));
        summary(out, &p.summary);
    }
    for p in nix.iter().take(LIMIT) {
        let mut mark = style.dim("nixpkgs");
        if p.unfree {
            mark.push_str(&style.yellow(" · unfree"));
        }
        out.line(format_args!(
            "{} {}  {mark}",
            style.bold(&p.attr),
            style.dim(&p.version)
        ));
        summary(out, &p.description);
    }
    out.blank();
    let total = matches.len() + nix.len();
    let shown = matches.len().min(LIMIT) + nix.len().min(LIMIT);
    let mut foot = if shown < total {
        format!("{shown} of {total} matches; add a word to narrow it")
    } else {
        ui::plural(total, "match")
    };
    foot.push_str(" · install with nx add <name>");
    out.line(style.dim(&foot));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    fn text(matches: &[Package], width: usize) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, matches, &[], width);
        buf.text()
    }

    fn package(name: &str, repo: &str, installed: bool, summary: &str) -> Package {
        Package {
            name: name.into(),
            evr: "1.24.0-1.fc45".into(),
            arch: "x86_64".into(),
            repo: repo.into(),
            installed,
            summary: summary.into(),
            ..Package::default()
        }
    }

    #[test]
    fn lists_marks_and_counts() {
        let matches = [
            package("wayland", "@image", true, "Wayland libraries"),
            package(
                "wayland-devel",
                "updates",
                false,
                "Development files for Wayland",
            ),
        ];
        assert_eq!(
            text(&matches, 30),
            "  wayland 1.24.0-1.fc45  ✓ in the image\n\
             \x20   Wayland libraries\n\
             \x20 wayland-devel 1.24.0-1.fc45  updates\n\
             \x20   Development files for W…\n\
             \n\
             \x20 2 matches · install with nx add <name>\n"
        );
    }

    #[test]
    fn nixpkgs_follows_fedora() {
        let rg = Nixpkg {
            attr: "ripgrep".into(),
            version: "15.2.0".into(),
            description: "Fast grep".into(),
            ..Nixpkg::default()
        };
        let ngrok = Nixpkg {
            attr: "ngrok".into(),
            version: "3.30.0".into(),
            unfree: true,
            ..Nixpkg::default()
        };
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(
            &mut ui.out,
            &[package("ripgrep", "fedora", false, "")],
            &[&rg, &ngrok],
            100,
        );
        assert_eq!(
            buf.text(),
            "  ripgrep 1.24.0-1.fc45  fedora\n\
             \x20 ripgrep 15.2.0  nixpkgs\n\
             \x20   Fast grep\n\
             \x20 ngrok 3.30.0  nixpkgs · unfree\n\
             \n\
             \x20 3 matches · install with nx add <name>\n"
        );
    }

    #[test]
    fn caps_long_lists() {
        let matches: Vec<_> = (0..LIMIT + 1)
            .map(|i| package(&format!("p{i}"), "fedora", false, ""))
            .collect();
        let out = text(&matches, 100);
        assert!(!out.contains("p25 "), "{out}");
        assert!(out.ends_with(
            "  25 of 26 matches; add a word to narrow it · install with nx add <name>\n"
        ));
    }

    #[test]
    fn says_when_nothing_matches() {
        assert_eq!(text(&[], 100), "  No packages match.\n");
    }
}
