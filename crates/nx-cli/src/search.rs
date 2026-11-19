//! `nx search`: host-tier packages whose name or summary matches, laid out
//! like nx 0.x's search (name, version, mark; summary indented below).

use nx_rpm::{Package, Universe};

use crate::ui::{self, Stream, Ui};

/// How many matches to show before asking for a narrower term.
const LIMIT: usize = 25;

pub fn run(ui: &mut Ui, universe: &Universe, terms: &[String]) {
    let matches = universe.search(&terms.join(" "));
    render(&mut ui.out, &matches, ui::width());
}

/// Lists `matches` for a terminal `width` columns wide.
fn render(out: &mut Stream, matches: &[Package], width: usize) {
    let style = out.style;
    if matches.is_empty() {
        out.line("No packages match.");
        return;
    }
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
        if !p.summary.is_empty() {
            // Two columns of indent on top of the stream's own two, and two
            // to spare.
            out.line(format_args!(
                "  {}",
                ui::truncate(&p.summary, width.saturating_sub(6))
            ));
        }
    }
    out.blank();
    let mut foot = if matches.len() > LIMIT {
        format!(
            "{LIMIT} of {} matches; add a word to narrow it",
            matches.len()
        )
    } else {
        ui::plural(matches.len(), "match")
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
        render(&mut ui.out, matches, width);
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
