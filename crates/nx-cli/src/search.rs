//! `nx search`: packages whose name or summary matches, from the host
//! tier's repos and then the nixpkgs catalog, laid out like nx 0.x's
//! search (name, version, mark; summary indented below).

use nx_nix::Catalog;
use nx_nix::catalog::Package as Nixpkg;
use nx_rpm::{Package, Universe};

use crate::ui::{self, Stream, Ui};

/// How many matches of each source to show before asking for a narrower
/// term.
const LIMIT: usize = 20;

/// What the current generation has: host package names and tool
/// attributes, each marked only in its own source.
#[derive(Debug, Default)]
pub struct Added {
    pub host: std::collections::HashSet<String>,
    pub tools: std::collections::HashSet<String>,
}

pub fn run(
    ui: &mut Ui,
    universe: &Universe,
    catalog: Option<&Catalog>,
    added: &Added,
    terms: &[String],
) {
    let matches = universe.search(&terms.join(" "));
    let nix = catalog.map(|c| c.search(terms)).unwrap_or_default();
    let hits = merge(&terms.join(" "), &matches, &nix, added);
    render(&mut ui.out, &hits, ui::width());
}

/// One result, from either source.
#[derive(Debug, PartialEq, Eq)]
struct Hit {
    name: String,
    version: String,
    /// The repo, or `nixpkgs`.
    source: String,
    summary: String,
    mark: Mark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    None,
    InImage,
    Added,
}

/// Both sources in one order: the exact name, then names that start with
/// the query, then names that contain it, then the rest (a summary
/// match). Each source keeps its own order within a rank, the repos first.
fn merge(query: &str, rpms: &[Package], nix: &[&Nixpkg], added: &Added) -> Vec<Hit> {
    let q = query.to_lowercase();
    let rank = |name: &str| {
        let name = name.to_lowercase();
        if name == q {
            0
        } else if name.starts_with(&q) {
            1
        } else if name.contains(&q) {
            2
        } else {
            3
        }
    };
    let mut hits: Vec<(u8, Hit)> = rpms
        .iter()
        .map(|p| {
            let mark = if p.installed {
                Mark::InImage
            } else if added.host.contains(&p.name) {
                Mark::Added
            } else {
                Mark::None
            };
            (
                rank(&p.name),
                Hit {
                    name: p.name.clone(),
                    version: p.evr.clone(),
                    source: p.repo.clone(),
                    summary: p.summary.clone(),
                    mark,
                },
            )
        })
        .chain(nix.iter().map(|p| {
            (
                rank(&p.attr),
                Hit {
                    name: p.attr.clone(),
                    version: p.version.clone(),
                    source: if p.unfree {
                        "nixpkgs · unfree"
                    } else {
                        "nixpkgs"
                    }
                    .to_owned(),
                    summary: p.description.clone(),
                    mark: if added.tools.contains(&p.attr) {
                        Mark::Added
                    } else {
                        Mark::None
                    },
                },
            )
        }))
        .collect();
    hits.sort_by_key(|(r, _)| *r);
    hits.into_iter().map(|(_, h)| h).collect()
}

fn render(out: &mut Stream, hits: &[Hit], width: usize) {
    let style = out.style;
    if hits.is_empty() {
        out.line("No packages match.");
        return;
    }
    let shown = &hits[..hits.len().min(LIMIT)];
    let w = |f: fn(&Hit) -> usize| shown.iter().map(f).max().unwrap_or(0);
    let name_w = w(|h| h.name.chars().count());
    // A long version (a date-stamped snapshot) is cut, not the summary.
    let version_w = w(|h| h.version.chars().count()).min(16);
    let source_w = w(|h| match h.mark {
        Mark::None => h.source.chars().count(),
        Mark::InImage => "✓ in the image".chars().count(),
        Mark::Added => "✓ added".chars().count(),
    });
    // The stream's indent, three gaps, and the columns before the summary.
    let used = 2 + name_w + version_w + source_w + 6;
    for h in shown {
        let source = match h.mark {
            Mark::None => style.dim(&h.source),
            Mark::InImage => ui::in_image(style),
            Mark::Added => style.green(&format!("{} added", ui::SYM_DONE)),
        };
        let mut line = format!(
            "{}  {}  {}",
            ui::pad(&style.bold(&h.name), name_w),
            ui::pad(&style.dim(&ui::truncate(&h.version, version_w)), version_w),
            ui::pad(&source, source_w),
        );
        let room = width.saturating_sub(used);
        if !h.summary.is_empty() && room > 10 {
            line.push_str("  ");
            line.push_str(&style.dim(&ui::truncate(&h.summary, room)));
        }
        out.line(line.trim_end());
    }
    out.blank();
    let mut foot = if shown.len() < hits.len() {
        format!(
            "{} of {} matches; add a word to narrow it",
            shown.len(),
            hits.len()
        )
    } else {
        ui::plural(hits.len(), "match")
    };
    foot.push_str(" · install with nx add <name>");
    out.line(style.dim(&foot));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    fn text(hits: &[Hit], width: usize) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, hits, width);
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
    fn ranks_names_across_both_sources_and_marks_what_is_installed() {
        let rpms = [
            package(
                "wayland-devel",
                "updates",
                false,
                "Development files for Wayland",
            ),
            package("wayland", "@image", true, "Wayland libraries"),
            package(
                "libdecor",
                "fedora",
                false,
                "Decorations for wayland clients",
            ),
        ];
        let wl = Nixpkg {
            attr: "wayland".into(),
            version: "1.24.0".into(),
            description: "Core Wayland window system code".into(),
            ..Nixpkg::default()
        };
        let added = Added {
            host: ["wayland-devel".to_owned()].into(),
            ..Added::default()
        };
        let hits = merge("wayland", &rpms, &[&wl], &added);
        let names: Vec<(&str, &str)> = hits
            .iter()
            .map(|h| (h.name.as_str(), h.source.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("wayland", "@image"),
                ("wayland", "nixpkgs"),
                ("wayland-devel", "updates"),
                ("libdecor", "fedora"),
            ]
        );
        assert_eq!(
            text(&hits, 80),
            "  wayland        1.24.0-1.fc45  ✓ in the image  Wayland libraries\n\
             \x20 wayland        1.24.0         nixpkgs         Core Wayland window system code\n\
             \x20 wayland-devel  1.24.0-1.fc45  ✓ added         Development files for Wayland\n\
             \x20 libdecor       1.24.0-1.fc45  fedora          Decorations for wayland clients\n\
             \n\
             \x20 4 matches · install with nx add <name>\n"
        );
        // The summary is cut to the terminal.
        assert!(text(&hits, 70).contains("  Core Wayland window s…\n"));
    }

    #[test]
    fn caps_long_lists() {
        let rpms: Vec<_> = (0..LIMIT + 1)
            .map(|i| package(&format!("p{i}"), "fedora", false, ""))
            .collect();
        let out = text(&merge("p", &rpms, &[], &Added::default()), 100);
        assert!(!out.contains("p20 "), "{out}");
        assert!(out.ends_with(
            "  20 of 21 matches; add a word to narrow it · install with nx add <name>\n"
        ));
    }

    #[test]
    fn says_when_nothing_matches() {
        assert_eq!(text(&[], 100), "  No packages match.\n");
    }
}
