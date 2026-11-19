//! nxd's logic, apart from the process around it: who is asking
//! ([`Peer`]), whether they may ([`Authorizer`], polkit in production), and
//! what a request does ([`serve`]).
//!
//! nxd never downloads, resolves or parses packages. It validates a
//! generation the unprivileged CLI wrote and makes it live.

pub mod serve;

use std::path::PathBuf;

use nx_activate::{Image, Layout, Runner, System};
use nx_core::State;
use nx_core::{DeploymentSource, RpmOstree};

/// Why a request was not carried out.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// polkit (or nxd, unable to ask it) said no.
    #[error("{0}")]
    Denied(String),
    #[error("can't identify the caller: {0}")]
    Peer(std::io::Error),
    #[error(transparent)]
    Request(#[from] nx_proto::Error),
    #[error(transparent)]
    Activate(#[from] nx_activate::Error),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The polkit action for making a generation live.
pub const ACTION_ACTIVATE: &str = "org.nx.activate";

/// Who is on the other end of a connection, from `SO_PEERCRED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub pid: i32,
    pub uid: u32,
}

/// Decides whether a peer may do an action.
pub trait Authorizer {
    /// `Ok` to allow; `Err(Error::Denied)` says what to tell the client.
    fn check(&self, peer: Peer, action: &str) -> Result<()>;
}

/// Asks polkit through `pkcheck`, which lets the session's agent prompt
/// when the rule doesn't already allow it. Root is always allowed.
pub struct Polkit;

impl Authorizer for Polkit {
    fn check(&self, peer: Peer, action: &str) -> Result<()> {
        if peer.uid == 0 {
            return Ok(());
        }
        // pid alone can be reused; the start time pins the process.
        let start = start_time(peer.pid)
            .ok_or_else(|| Error::Denied("can't identify the calling process".into()))?;
        let subject = format!("{},{start},{}", peer.pid, peer.uid);
        let out = std::process::Command::new("pkcheck")
            .args(["--action-id", action, "--process", &subject])
            .arg("--allow-user-interaction")
            .output()
            .map_err(|e| Error::Denied(format!("can't run pkcheck: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.trim();
        Err(Error::Denied(if said.is_empty() {
            "not authorized".to_owned()
        } else {
            format!("not authorized: {said}")
        }))
    }
}

/// Field 22 of `/proc/<pid>/stat`: when the process started, in ticks.
fn start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_start_time(&stat)
}

fn parse_start_time(stat: &str) -> Option<u64> {
    // The command name is in parentheses and may hold spaces and
    // parentheses; everything after the last `)` is plain fields, the
    // first of which (state) is field 3.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

/// The booted deployment's tree under `/sysroot`, which no extension
/// overlays; `Image::None` off a bootc system (ostree's marker).
fn booted_image() -> Image {
    if !std::path::Path::new("/run/ostree-booted").exists() {
        return Image::None;
    }
    let root = RpmOstree
        .deployments()
        .map_err(|e| e.to_string())
        .and_then(|d| {
            d.booted()
                .map(|b| b.root(std::path::Path::new("/sysroot")))
                .ok_or_else(|| "rpm-ostree reports no booted deployment".to_owned())
        });
    match root {
        Ok(root) if root.join("usr").is_dir() => Image::Root(root),
        Ok(root) => Image::Unreadable(format!("{} has no /usr", root.display())),
        Err(e) => Image::Unreadable(e),
    }
}

/// What a request runs against.
pub struct Context {
    pub layout: Layout,
    pub runner: Box<dyn Runner>,
    pub authorizer: Box<dyn Authorizer>,
    /// Finds the booted image each time a request needs it (an image
    /// update changes it under a long-running `--serve`).
    pub image: Box<dyn Fn() -> Image>,
    /// Held while a request runs: with one process per connection, two
    /// activations must not interleave.
    pub lock: PathBuf,
}

impl Context {
    /// The real machine around `state`.
    pub fn system(state: State) -> Self {
        Self {
            layout: Layout::system(state),
            runner: Box::new(System),
            authorizer: Box::new(Polkit),
            image: Box::new(booted_image),
            lock: PathBuf::from("/run/nxd.lock"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_start_time_past_odd_command_names() {
        let stat = "123 (my (odd) cmd) S 1 123 123 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 987654 \
                    1000 100 18446744073709551615";
        assert_eq!(parse_start_time(stat), Some(987_654));
        assert_eq!(parse_start_time("garbage"), None);
    }
}
