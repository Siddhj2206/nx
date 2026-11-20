//! `nxd`: the root half of nx.
//!
//! systemd runs one `nxd` per connection (`nxd.socket` with `Accept=yes`,
//! the connection on stdin), so a request is handled in a fresh process
//! that exits when it is answered. `nxd --serve <socket>` listens itself,
//! for a VM test or development without systemd.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use nx_core::State;
use nxd::{Context, serve};

const USAGE: &str = "usage: nxd [--state DIR] [--serve SOCKET]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nxd: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut state = PathBuf::from(nx_core::state::DEFAULT_ROOT);
    let mut socket = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state" => state = args.next().context(USAGE)?.into(),
            "--serve" => socket = Some(PathBuf::from(args.next().context(USAGE)?)),
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    let ctx = Context::system(State::new(state));
    match socket {
        Some(path) => serve::listen(&path, &ctx).context("serving"),
        None => {
            // The connection systemd accepted for us is stdin.
            let fd = std::io::stdin()
                .as_fd()
                .try_clone_to_owned()
                .context("duplicating stdin")?;
            serve::handle(UnixStream::from(fd), &ctx);
            Ok(())
        }
    }
}
