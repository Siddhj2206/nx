//! Making a built generation live: ask nxd, which validates it, labels it
//! and merges it. With `NX_STATE_DIR` (a VM test, development) there is no
//! nxd for that directory, so `current` moves and nothing is merged.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use nx_core::{Backend, State};
use nx_proto::{Request, Response};

use crate::host;
use crate::ui::Ui;

/// nxd's socket: `NXD_SOCKET` (for `nxd --serve`), else the system one.
fn socket() -> PathBuf {
    std::env::var_os("NXD_SOCKET")
        .filter(|s| !s.is_empty())
        .map_or_else(|| PathBuf::from(nx_proto::SOCKET), PathBuf::from)
}

/// Makes generation `number` current and live, telling the user how.
pub fn activate(ui: &mut Ui, state: &State, number: u32, forced: Option<Backend>) -> Result<()> {
    if host::state_overridden() {
        state.set_current(number)?;
        ui.warn(format_args!(
            "NX_STATE_DIR is set: generation {number} is current but not merged"
        ));
        return Ok(());
    }
    let started = Instant::now();
    let request = Request::Activate {
        generation: number,
        backend: forced,
    };
    let response = nx_proto::call(&socket(), &request).map_err(|e| match e {
        nx_proto::Error::Unavailable { .. } => anyhow::Error::new(e).context(
            "generation is built but not live; nxd is not running (systemctl enable --now nxd.socket)",
        ),
        e => anyhow::Error::new(e).context("asking nxd to activate the generation"),
    })?;
    let live = match response {
        Response::Activated {
            backend,
            fallback,
            events,
            warnings,
        } => {
            if let Some(why) = fallback {
                ui.warn(format_args!(
                    "sysext could not merge ({why}); using the prefix backend, `nx shell` exports it"
                ));
            }
            ui.step(format_args!(
                "generation {number} is live · {} · {} ms",
                backend.name(),
                started.elapsed().as_millis()
            ));
            for event in events {
                ui.service(event);
            }
            for warning in warnings {
                ui.warn(warning);
            }
            Ok(())
        }
        Response::Failed { message } => Err(anyhow::anyhow!(message)),
    };
    live.with_context(|| format!("nxd did not activate generation {number}"))
}
