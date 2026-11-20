//! What a change did to the packages, as every command that changes them
//! reports it: one `↑`, `+` or `-` line per package of either tier, then
//! one summary line.

use std::collections::BTreeMap;

use nx_core::{LockedPackage, LockedTool};
use nx_rpm::Planned;

use crate::ui::{self, Stream};

/// What changed for one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Upgraded {
        name: String,
        from: String,
        to: String,
    },
    Added {
        name: String,
        version: String,
    },
    Removed {
        name: String,
        version: String,
    },
}

/// The host tier from the current generation's packages to a plan's.
pub fn host(before: &[LockedPackage], after: &[Planned]) -> Vec<Change> {
    let before: BTreeMap<String, String> = before
        .iter()
        .map(|p| (p.name.clone(), p.evr().to_owned()))
        .collect();
    let after: BTreeMap<String, String> = after
        .iter()
        .map(|p| (p.package.name.clone(), p.package.evr.clone()))
        .collect();
    diff(&before, &after, "")
}

/// The tool tier from one lock to another.
pub fn tools(before: Option<&LockedTool>, after: Option<&LockedTool>) -> Vec<Change> {
    let versions = |t: Option<&LockedTool>| -> BTreeMap<String, String> {
        t.iter()
            .flat_map(|t| t.packages.iter())
            .map(|p| (p.attr.clone(), p.version.clone()))
            .collect()
    };
    diff(&versions(before), &versions(after), " · nixpkgs")
}

/// Package by package, from `before` to `after` (name → version); `tag`
/// follows the version.
fn diff(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    tag: &str,
) -> Vec<Change> {
    let mut out = Vec::new();
    for (name, to) in after {
        match before.get(name) {
            Some(from) if from != to => out.push(Change::Upgraded {
                name: name.clone(),
                from: from.clone(),
                to: format!("{to}{tag}"),
            }),
            Some(_) => {}
            None => out.push(Change::Added {
                name: name.clone(),
                version: format!("{to}{tag}"),
            }),
        }
    }
    for (name, from) in before {
        if !after.contains_key(name) {
            out.push(Change::Removed {
                name: name.clone(),
                version: format!("{from}{tag}"),
            });
        }
    }
    out
}

/// One `↑`, `+` or `-` line per change.
pub fn lines(out: &mut Stream, changes: &[Change]) {
    let style = out.style;
    for c in changes {
        match c {
            Change::Upgraded { name, from, to } => out.line(format_args!(
                "{} {name} {}",
                style.cyan(ui::SYM_UPGRADED),
                style.dim(&format!("{from} → {to}"))
            )),
            Change::Added { name, version } => out.line(format_args!(
                "{} {name} {}",
                style.green(ui::SYM_ADDED),
                style.dim(version)
            )),
            Change::Removed { name, version } => out.line(format_args!(
                "{} {name} {}",
                style.red(ui::SYM_REMOVED),
                style.dim(version)
            )),
        }
    }
}

/// How a change ended, for [`summary`].
#[derive(Debug, Default)]
pub struct Done<'a> {
    /// The generation made live; `None` for a dry run.
    pub generation: Option<u32>,
    /// The nixpkgs release the tool tier moved to.
    pub moved: Option<&'a str>,
    /// Bytes downloaded.
    pub download: u64,
    pub secs: f64,
}

/// `✓ 2 packages added · generation 5 · 1.2 MiB in 3.0s`, or for a dry
/// run `2 packages added · dry run, nothing changed`.
pub fn summary(out: &mut Stream, changes: &[Change], done: &Done) {
    let style = out.style;
    let all = |f: fn(&Change) -> bool| changes.iter().all(f);
    let verb = if all(|c| matches!(c, Change::Added { .. })) {
        "added"
    } else if all(|c| matches!(c, Change::Removed { .. })) {
        "removed"
    } else if all(|c| matches!(c, Change::Upgraded { .. })) {
        "upgraded"
    } else {
        "changed"
    };
    let mut what = if changes.is_empty() {
        "manifest updated".to_owned()
    } else {
        format!("{} {verb}", ui::plural(changes.len(), "package"))
    };
    if let Some(v) = done.moved {
        what.push_str(&format!(" · nixpkgs {v}"));
    }
    match done.generation {
        Some(n) => {
            let size = if done.download > 0 {
                format!(" · {}", ui::size(done.download))
            } else {
                String::new()
            };
            out.line(format_args!(
                "{} {what} · generation {n}{size} in {:.1}s",
                style.green(ui::SYM_DONE),
                done.secs
            ));
        }
        None => out.line(style.dim(&format!("{what} · dry run, nothing changed"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{SharedBuf, Ui};

    fn map(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    fn text(changes: &[Change], done: &Done) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        lines(&mut ui.out, changes);
        summary(&mut ui.out, changes, done);
        buf.text()
    }

    #[test]
    fn lists_what_moved_then_one_summary() {
        let changes = diff(
            &map(&[("ripgrep", "15.1.0"), ("jq", "1.8.2"), ("old", "1")]),
            &map(&[("ripgrep", "15.2.0"), ("jq", "1.8.2"), ("new", "2")]),
            " · nixpkgs",
        );
        assert_eq!(
            text(
                &changes,
                &Done {
                    generation: Some(4),
                    moved: Some("26.11pre1122334"),
                    secs: 2.0,
                    ..Done::default()
                }
            ),
            "  + new 2 · nixpkgs\n\
             \x20 ↑ ripgrep 15.1.0 → 15.2.0 · nixpkgs\n\
             \x20 - old 1 · nixpkgs\n\
             \x20 ✓ 3 packages changed · nixpkgs 26.11pre1122334 · generation 4 in 2.0s\n"
        );
    }

    #[test]
    fn says_what_kind_of_change_it_was() {
        let added = [Change::Added {
            name: "libxkbcommon-devel".into(),
            version: "1.13.1-4.fc45".into(),
        }];
        assert_eq!(
            text(
                &added,
                &Done {
                    generation: Some(3),
                    download: 74 << 10,
                    secs: 1.24,
                    ..Done::default()
                }
            ),
            "  + libxkbcommon-devel 1.13.1-4.fc45\n\
             \x20 ✓ 1 package added · generation 3 · 74 KiB in 1.2s\n"
        );
        let removed = [
            Change::Removed {
                name: "wayland-devel".into(),
                version: "1-1.fc45".into(),
            },
            Change::Removed {
                name: "libffi-devel".into(),
                version: "1-1.fc45".into(),
            },
        ];
        assert_eq!(
            text(&removed, &Done::default()),
            "  - wayland-devel 1-1.fc45\n\
             \x20 - libffi-devel 1-1.fc45\n\
             \x20 2 packages removed · dry run, nothing changed\n"
        );
        assert_eq!(
            text(
                &[],
                &Done {
                    generation: Some(6),
                    secs: 0.25,
                    ..Done::default()
                }
            ),
            "  ✓ manifest updated · generation 6 in 0.2s\n"
        );
    }
}
