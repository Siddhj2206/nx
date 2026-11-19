//! `nx upgrade`: moves both tiers to what their sources have now (design,
//! *Default update policy*). The host tier re-resolves without preferring
//! the current builds, so packages unrelated to the image take the repos'
//! newer ones; image relatives stay the image's exact builds, which only
//! the image moves. The tool tier moves its pin to the channel's current
//! release and evaluates every package there.

use std::time::Instant;

use anyhow::Result;
use nx_rpm::Refresh;

use crate::host::Machine;
use crate::session::{self, How, Session};
use crate::ui::Ui;

pub fn run(
    ui: &mut Ui,
    machine: &Machine,
    (host, tool): (bool, bool),
    dry_run: bool,
    refresh: Refresh,
) -> Result<()> {
    let started = Instant::now();
    // Neither flag: both tiers.
    let both = !host && !tool;
    let mut session = Session::open(ui, machine, refresh, dry_run, |_| Ok(()))?;
    let outcome = session.plan(
        ui,
        &How {
            newer_host: host || both,
            newer_tools: tool || both,
            ..How::default()
        },
    )?;
    if outcome.changes.is_empty() && outcome.moved.is_none() {
        ui.done("everything is up to date");
        return Ok(());
    }
    session::finish(ui, &mut session, &outcome, started)
}
