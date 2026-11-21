//! What a change did to the packages, as every command that changes them
//! reports it (plan 1.1, *Output* rules 2 and 4): an aligned table, one
//! row per package the manifest names, its dependencies folded under it
//! and counted (`-v` lists them), then one summary line.

use std::collections::BTreeMap;

use nx_core::{LockedPackage, LockedTool};
use nx_rpm::Planned;

use crate::host::Role;
use crate::ui::{self, Stream};

/// What happened to a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Added,
    Upgraded,
    Removed,
}

/// What changed for one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: Kind,
    pub name: String,
    /// The version before: upgraded and removed.
    pub from: Option<String>,
    /// The version after: added and upgraded.
    pub to: Option<String>,
    /// A nixpkgs package (the tool tier).
    pub tool: bool,
    /// The manifest entry that pulled it in, when that isn't itself.
    pub parent: Option<String>,
}

impl Change {
    #[cfg(test)]
    pub fn added(name: &str, version: &str) -> Self {
        Self {
            kind: Kind::Added,
            name: name.into(),
            from: None,
            to: Some(version.into()),
            tool: false,
            parent: None,
        }
    }

    #[cfg(test)]
    pub fn for_(mut self, parent: &str) -> Self {
        self.parent = Some(parent.into());
        self
    }

    #[cfg(test)]
    pub fn tool(mut self) -> Self {
        self.tool = true;
        self
    }
}

/// A package's version and the entry it is there for.
type Side = BTreeMap<String, (String, Option<String>)>;

/// The host tier from the current generation's packages to a plan's.
pub fn host(before: &[LockedPackage], after: &[Planned]) -> Vec<Change> {
    let before: Side = before
        .iter()
        .map(|p| {
            let parent = parent(&p.name, &p.requested);
            (p.name.clone(), (p.evr().to_owned(), parent))
        })
        .collect();
    let after: Side = after
        .iter()
        .map(|p| {
            let parent = parent(&p.package.name, &p.requested);
            (p.package.name.clone(), (p.package.evr.clone(), parent))
        })
        .collect();
    diff(&before, &after, false)
}

/// The host tier from one lock to another (`nx rollback`).
pub fn host_locks(before: &[LockedPackage], after: &[LockedPackage]) -> Vec<Change> {
    let side = |ps: &[LockedPackage]| -> Side {
        ps.iter()
            .map(|p| {
                let parent = parent(&p.name, &p.requested);
                (p.name.clone(), (p.evr().to_owned(), parent))
            })
            .collect()
    };
    diff(&side(before), &side(after), false)
}

/// The tool tier from one lock to another.
pub fn tools(before: Option<&LockedTool>, after: Option<&LockedTool>) -> Vec<Change> {
    let versions = |t: Option<&LockedTool>| -> Side {
        t.iter()
            .flat_map(|t| t.packages.iter())
            .map(|p| (p.attr.clone(), (p.version.clone(), None)))
            .collect()
    };
    diff(&versions(before), &versions(after), true)
}

/// The entry `name` is there for: none when an entry names it.
fn parent(name: &str, requested: &[String]) -> Option<String> {
    let named = requested.iter().any(|r| r.split('@').next() == Some(name));
    if named {
        None
    } else {
        requested.first().cloned()
    }
}

/// Package by package, from `before` to `after`.
fn diff(before: &Side, after: &Side, tool: bool) -> Vec<Change> {
    let mut out = Vec::new();
    for (name, (to, parent)) in after {
        let change = |kind, from: Option<&String>| Change {
            kind,
            name: name.clone(),
            from: from.cloned(),
            to: Some(to.clone()),
            tool,
            parent: parent.clone(),
        };
        match before.get(name) {
            Some((from, _)) if from != to => out.push(change(Kind::Upgraded, Some(from))),
            Some(_) => {}
            None => out.push(change(Kind::Added, None)),
        }
    }
    for (name, (from, parent)) in before {
        if !after.contains_key(name) {
            out.push(Change {
                kind: Kind::Removed,
                name: name.clone(),
                from: Some(from.clone()),
                to: None,
                tool,
                parent: parent.clone(),
            });
        }
    }
    out
}

/// One row of the table.
struct Row {
    sym: String,
    name: String,
    version: String,
    note: String,
}

/// The table: what the manifest names, each with what came or went with
/// it; dependencies whose entry didn't change in the same way are grouped
/// by that entry. `-v` lists every package.
pub fn table(out: &mut Stream, changes: &[Change]) {
    let (style, verbose) = (out.style, out.verbose);
    let mut rows: Vec<Row> = Vec::new();
    for kind in [Kind::Added, Kind::Upgraded, Kind::Removed] {
        let of_kind: Vec<&Change> = changes.iter().filter(|c| c.kind == kind).collect();
        let sym = match kind {
            Kind::Added => style.green(ui::SYM_ADDED),
            Kind::Upgraded => style.cyan(ui::SYM_UPGRADED),
            Kind::Removed => style.red(ui::SYM_REMOVED),
        };
        let heads: Vec<&Change> = of_kind
            .iter()
            .copied()
            .filter(|c| c.parent.is_none())
            .collect();
        let deps_of = |name: &str| -> Vec<&Change> {
            of_kind
                .iter()
                .copied()
                .filter(|c| c.parent.as_deref() == Some(name))
                .collect()
        };
        for head in &heads {
            let deps = deps_of(&head.name);
            let mut note = if head.tool {
                "nixpkgs".to_owned()
            } else {
                String::new()
            };
            if !deps.is_empty() && !verbose {
                let with = format!("with {}", dependencies(deps.len()));
                note = if note.is_empty() {
                    with
                } else {
                    format!("{note} · {with}")
                };
            }
            rows.push(row(&sym, &head.name, head, note));
            if verbose {
                for d in deps {
                    rows.push(row(&sym, &format!("  {}", d.name), d, String::new()));
                }
            }
        }
        // Dependencies of an entry that itself didn't change this way.
        let mut orphans: Vec<(&str, Vec<&Change>)> = Vec::new();
        for c in &of_kind {
            let Some(p) = c.parent.as_deref() else {
                continue;
            };
            if heads.iter().any(|h| h.name == p) {
                continue;
            }
            match orphans.iter_mut().find(|(q, _)| *q == p) {
                Some((_, v)) => v.push(c),
                None => orphans.push((p, vec![c])),
            }
        }
        for (p, group) in orphans {
            if verbose || group.len() <= 2 {
                for c in group {
                    rows.push(row(&sym, &c.name, c, format!("for {p}")));
                }
            } else {
                rows.push(Row {
                    sym: sym.clone(),
                    name: ui::plural(group.len(), "package"),
                    version: String::new(),
                    note: format!("for {p}"),
                });
            }
        }
    }
    let name_w = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    let version_w = rows
        .iter()
        .map(|r| r.version.chars().count())
        .max()
        .unwrap_or(0);
    for r in &rows {
        // Columns padded only when something follows them.
        let mut cols = vec![r.name.clone()];
        if version_w > 0 && (!r.version.is_empty() || !r.note.is_empty()) {
            cols.push(r.version.clone());
        }
        if !r.note.is_empty() {
            cols.push(r.note.clone());
        }
        let last = cols.len() - 1;
        let widths = [name_w, version_w];
        let line: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let c = if i < last {
                    ui::pad(c, widths[i])
                } else {
                    c.clone()
                };
                if i == 0 { c } else { style.dim(&c) }
            })
            .collect();
        out.line(format_args!("{} {}", r.sym, line.join("  ")));
    }
}

fn row(sym: &str, name: &str, c: &Change, note: String) -> Row {
    let version = match (&c.from, &c.to) {
        (Some(from), Some(to)) => format!("{from} → {to}"),
        (_, Some(v)) | (Some(v), None) => v.clone(),
        (None, None) => String::new(),
    };
    Row {
        sym: sym.to_owned(),
        name: name.to_owned(),
        version,
        note,
    }
}

fn dependencies(n: usize) -> String {
    match n {
        1 => "1 dependency".to_owned(),
        n => format!("{n} dependencies"),
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
    /// The other images the change was carried to: role and generation.
    pub others: &'a [(Role, u32)],
    /// What the change was, when it wasn't packages ("1 repo added", "1
    /// repo to add"): it heads the line in place of the count.
    pub head: Option<&'a str>,
}

/// `✓ 2 packages added · 1.2 MiB in 3.0s`, or for a dry run `2 packages
/// to add · 1.2 MiB to download · dry run, nothing changed`. `-v` adds the
/// generation numbers.
pub fn summary(out: &mut Stream, changes: &[Change], done: &Done) {
    let (style, verbose) = (out.style, out.verbose);
    let dry = done.generation.is_none();
    let count = |k: Kind| changes.iter().filter(|c| c.kind == k).count();
    let kinds: Vec<(usize, &str, &str)> = [
        (count(Kind::Added), "added", "to add"),
        (count(Kind::Upgraded), "upgraded", "to upgrade"),
        (count(Kind::Removed), "removed", "to remove"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .collect();
    let mut parts: Vec<String> = Vec::new();
    parts.push(match kinds.as_slice() {
        _ if done.head.is_some() => done.head.unwrap_or_default().to_owned(),
        [] => "manifest updated".to_owned(),
        [(n, done_verb, dry_verb)] => {
            format!(
                "{} {}",
                ui::plural(*n, "package"),
                if dry { dry_verb } else { done_verb }
            )
        }
        many => many
            .iter()
            .map(|(n, done_verb, dry_verb)| {
                format!("{n} {}", if dry { dry_verb } else { done_verb })
            })
            .collect::<Vec<_>>()
            .join(", "),
    });
    if let Some(v) = done.moved {
        parts.push(format!("nixpkgs {v}"));
    }
    match done.generation {
        Some(n) => {
            let mut took = format!("{:.1}s", done.secs);
            if done.download > 0 {
                took = format!("{} in {took}", ui::size(done.download));
            }
            parts.push(took);
            if verbose {
                parts.push(format!("generation {n}"));
            }
            if !done.others.is_empty() {
                parts.push(others(done.others, verbose));
            }
            out.line(format_args!(
                "{} {}",
                style.green(ui::SYM_DONE),
                parts.join(" · ")
            ));
        }
        None => {
            if done.download > 0 {
                parts.push(format!("{} to download", ui::size(done.download)));
            }
            parts.push("dry run, nothing changed".to_owned());
            out.line(style.dim(&parts.join(" · ")));
        }
    }
}

/// `the staged image too`, `the staged and rollback images too`; with
/// `-v`, their generations.
fn others(others: &[(Role, u32)], verbose: bool) -> String {
    if verbose {
        let each: Vec<String> = others
            .iter()
            .map(|(role, n)| format!("{role} image: generation {n}"))
            .collect();
        return each.join(", ");
    }
    let roles: Vec<String> = others.iter().map(|(r, _)| r.to_string()).collect();
    match roles.as_slice() {
        [one] => format!("the {one} image too"),
        [rest @ .., last] => format!("the {} and {last} images too", rest.join(", ")),
        [] => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{SharedBuf, Ui};

    fn side(items: &[(&str, &str)]) -> Side {
        items
            .iter()
            .map(|(a, b)| ((*a).to_owned(), ((*b).to_owned(), None)))
            .collect()
    }

    fn text(changes: &[Change], done: &Done, verbose: bool) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        ui.set_verbose(verbose);
        table(&mut ui.out, changes);
        summary(&mut ui.out, changes, done);
        buf.text()
    }

    #[test]
    fn lists_what_moved_then_one_summary() {
        let changes = diff(
            &side(&[("ripgrep", "15.1.0"), ("jq", "1.8.2"), ("old", "1")]),
            &side(&[("ripgrep", "15.2.0"), ("jq", "1.8.2"), ("new", "2")]),
            true,
        );
        let done = Done {
            generation: Some(4),
            moved: Some("26.11pre1122334"),
            secs: 2.0,
            ..Done::default()
        };
        assert_eq!(
            text(&changes, &done, false),
            "  + new      2                nixpkgs\n\
             \x20 ↑ ripgrep  15.1.0 → 15.2.0  nixpkgs\n\
             \x20 - old      1                nixpkgs\n\
             \x20 ✓ 1 added, 1 upgraded, 1 removed · nixpkgs 26.11pre1122334 · 2.0s\n"
        );
    }

    #[test]
    fn folds_dependencies_under_what_pulled_them_in() {
        let changes = [
            Change::added("libffi-devel", "3.5.2-3.fc45").for_("wayland-devel"),
            Change::added("wayland-devel", "1.26.0-3.fc45"),
            Change::added("libxml2-devel", "2.14.6-1.fc45").for_("wayland-devel"),
            Change::added("ripgrep", "15.2.0").tool(),
        ];
        let done = Done {
            generation: Some(3),
            download: 190 << 10,
            secs: 1.24,
            others: &[(Role::Staged, 4)],
            ..Done::default()
        };
        assert_eq!(
            text(&changes, &done, false),
            "  + wayland-devel  1.26.0-3.fc45  with 2 dependencies\n\
             \x20 + ripgrep        15.2.0         nixpkgs\n\
             \x20 ✓ 4 packages added · 190 KiB in 1.2s · the staged image too\n"
        );
        assert_eq!(
            text(&changes, &done, true),
            "  + wayland-devel    1.26.0-3.fc45\n\
             \x20 +   libffi-devel   3.5.2-3.fc45\n\
             \x20 +   libxml2-devel  2.14.6-1.fc45\n\
             \x20 + ripgrep          15.2.0         nixpkgs\n\
             \x20 ✓ 4 packages added · 190 KiB in 1.2s · generation 3 · staged image: generation 4\n"
        );
    }

    #[test]
    fn groups_dependencies_whose_entry_stayed() {
        let up = |name: &str| Change {
            kind: Kind::Upgraded,
            from: Some("1".into()),
            to: Some("2".into()),
            ..Change::added(name, "").for_("wayland-devel")
        };
        let changes = [up("a"), up("b"), up("c"), up("d")];
        assert_eq!(
            text(&changes, &Done::default(), false),
            "  ↑ 4 packages  for wayland-devel\n\
             \x20 4 packages to upgrade · dry run, nothing changed\n"
        );
        assert_eq!(
            text(&changes[..2], &Done::default(), false),
            "  ↑ a  1 → 2  for wayland-devel\n\
             \x20 ↑ b  1 → 2  for wayland-devel\n\
             \x20 2 packages to upgrade · dry run, nothing changed\n"
        );
    }

    #[test]
    fn an_entry_the_package_itself_answers_is_no_parent() {
        assert_eq!(
            parent("wayland-devel", &["wayland-devel@fedora".into()]),
            None
        );
        assert_eq!(
            parent("libffi-devel", &["wayland-devel".into()]).as_deref(),
            Some("wayland-devel")
        );
        assert_eq!(parent("x", &[]), None);
    }

    #[test]
    fn a_manifest_only_change_says_so() {
        let done = Done {
            generation: Some(6),
            secs: 0.25,
            ..Done::default()
        };
        assert_eq!(text(&[], &done, false), "  ✓ manifest updated · 0.2s\n");
    }
}
