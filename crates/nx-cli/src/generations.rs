//! `nx generations`: every generation on disk, which one is current, and
//! what each changed compared with the one before it.

use std::collections::BTreeSet;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use nx_core::{Generation, State};

use crate::host;
use crate::ui::{self, Stream, Ui};

/// One generation as `nx generations` shows it.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    number: u32,
    /// Seconds since the epoch; `None` when the file's time is unreadable.
    made: Option<u64>,
    packages: usize,
    added: Vec<String>,
    removed: Vec<String>,
    current: bool,
}

pub fn run(ui: &mut Ui) -> Result<()> {
    let state = host::state();
    let rows = rows(ui, &state)?;
    render(&mut ui.out, &rows);
    Ok(())
}

fn rows(ui: &mut Ui, state: &State) -> Result<Vec<Row>> {
    let current = state.current_number().context("reading `current`")?;
    let mut rows = Vec::new();
    let mut before: BTreeSet<String> = BTreeSet::new();
    for number in state.generations()? {
        let g = match Generation::read(state, number) {
            Ok(g) => g,
            Err(e) => {
                ui.warn(format_args!("generation {number}: {e}"));
                continue;
            }
        };
        let names: BTreeSet<String> = g.lock.host.iter().map(|p| p.name.clone()).collect();
        rows.push(Row {
            number,
            made: std::fs::metadata(g.dir.join("lock.toml"))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            packages: names.len(),
            added: names.difference(&before).cloned().collect(),
            removed: before.difference(&names).cloned().collect(),
            current: current == Some(number),
        });
        before = names;
    }
    Ok(rows)
}

/// Newest first, like a log.
fn render(out: &mut Stream, rows: &[Row]) {
    let style = out.style;
    if rows.is_empty() {
        out.line(style.dim("No generations yet · nx add <name> makes the first"));
        return;
    }
    for r in rows.iter().rev() {
        let date = r.made.map_or_else(|| "unknown".to_owned(), ui::date);
        let mut change: Vec<String> = r
            .added
            .iter()
            .map(|n| style.green(&format!("+{n}")))
            .collect();
        change.extend(r.removed.iter().map(|n| style.red(&format!("-{n}"))));
        let change = if change.is_empty() {
            style.dim("no change")
        } else {
            summarize(&change)
        };
        let mark = if r.current {
            style.green("current")
        } else {
            String::new()
        };
        out.line(format_args!(
            "{}{}{}{}  {mark}",
            ui::pad(&style.bold(&r.number.to_string()), 6),
            ui::pad(&style.dim(&date), 19),
            ui::pad(&style.dim(&ui::plural(r.packages, "package")), 15),
            change
        ));
    }
}

/// The first few changes, then how many more.
fn summarize(change: &[String]) -> String {
    const SHOWN: usize = 4;
    let mut text = change[..change.len().min(SHOWN)].join(" ");
    if change.len() > SHOWN {
        text.push_str(&format!(" and {} more", change.len() - SHOWN));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    fn row(number: u32, packages: usize, added: &[&str], removed: &[&str], current: bool) -> Row {
        Row {
            number,
            made: Some(1_795_076_040),
            packages,
            added: added.iter().map(|s| (*s).to_owned()).collect(),
            removed: removed.iter().map(|s| (*s).to_owned()).collect(),
            current,
        }
    }

    fn text(rows: &[Row]) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, rows);
        buf.text()
    }

    #[test]
    fn shows_newest_first_with_the_diff_and_the_current_one() {
        let rows = [
            row(1, 1, &["wayland-devel"], &[], false),
            row(2, 2, &["libxkbcommon-devel"], &[], false),
            row(3, 1, &[], &["libxkbcommon-devel"], true),
        ];
        assert_eq!(
            text(&rows),
            "  3     2026-11-19 08:14   1 package      -libxkbcommon-devel  current\n\
             \x20 2     2026-11-19 08:14   2 packages     +libxkbcommon-devel  \n\
             \x20 1     2026-11-19 08:14   1 package      +wayland-devel  \n"
        );
        assert_eq!(
            text(&[]),
            "  No generations yet · nx add <name> makes the first\n"
        );
    }

    #[test]
    fn long_changes_are_cut() {
        let change: Vec<String> = (1..=6).map(|n| format!("+p{n}")).collect();
        assert_eq!(summarize(&change), "+p1 +p2 +p3 +p4 and 2 more");
    }
}
