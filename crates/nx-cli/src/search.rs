//! `nx search`: host-tier packages whose name or summary matches, laid out
//! like nx 0.x's search (name, version, mark; summary indented below).

use anyhow::Result;
use nx_rpm::Universe;

use crate::ui::{self, Ui};

/// How many matches to show before asking for a narrower term.
const LIMIT: usize = 25;

pub fn run(ui: &mut Ui, universe: &Universe, terms: &[String]) -> Result<()> {
    let term = terms.join(" ");
    let matches = universe.search(&term);
    let s = &mut ui.out;
    let st = s.style;
    if matches.is_empty() {
        s.line("No packages match.");
        return Ok(());
    }
    let width = ui::width();
    for p in matches.iter().take(LIMIT) {
        let mark = if p.installed {
            format!("  {}", st.green(&format!("{} in the image", ui::SYM_DONE)))
        } else {
            format!("  {}", st.dim(&p.repo))
        };
        s.line(format_args!(
            "{} {}{mark}",
            st.bold(&p.name),
            st.dim(&p.evr)
        ));
        if !p.summary.is_empty() {
            s.line(format_args!(
                "  {}",
                ui::truncate(&p.summary, width.saturating_sub(6))
            ));
        }
    }
    s.blank();
    let mut foot = if matches.len() > LIMIT {
        format!(
            "{LIMIT} of {} matches; add a word to narrow it",
            matches.len()
        )
    } else {
        ui::plural(matches.len(), "match").replace("matchs", "matches")
    };
    foot.push_str(" · install with nx add <name>");
    s.line(st.dim(&foot));
    Ok(())
}
