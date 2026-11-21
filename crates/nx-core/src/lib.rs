//! nx's view of the system it runs on.
//!
//! Everything nx installs is resolved against the booted bootc image, so the
//! first thing any command needs is which image that is, which deployments
//! exist beside it, and which Fedora release each one is. Every read here
//! goes through a root path or a [`DeploymentSource`] so tests run against
//! fixtures, without root and without a bootc system.

pub mod deployment;
pub mod generation;
pub mod journal;
pub mod lockfile;
pub mod manifest;
pub mod os_release;
pub mod select;
pub mod state;

pub use deployment::{
    BootedOstree, Deployment, DeploymentSource, Deployments, Ostree, OstreeCli, RpmOstree,
};
pub use generation::{
    Activation, ActivationPackage, Built, Generation, Overlap, Readable, Refusal,
};
pub use journal::{Fallback, Journal};
pub use lockfile::{Bin, Lock, LockedPackage, LockedRepo, LockedTool, ToolOutput, ToolPackage};
pub use manifest::{CliTools, Https, Manifest, RepoEntry, ScriptletAllow};
pub use os_release::OsRelease;
pub use state::{State, Store};

/// The version of the state format (manifest, lockfile, generation layout)
/// this nx writes. An older nx refuses newer state rather than misread it
/// (design invariant 9).
pub const FORMAT: u32 = 1;

/// This nx's version, recorded in each lockfile it writes
/// ([`Lock::builder`]).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Errors from reading the system's state.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("running {command}: {source}")]
    Spawn {
        command: &'static str,
        source: std::io::Error,
    },
    #[error("{command} failed: {stderr}")]
    Command {
        command: &'static str,
        stderr: String,
    },
    #[error("parsing {what}: {source}")]
    Json {
        what: &'static str,
        source: serde_json::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {message}")]
    Toml {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("{path} is format {version}, newer than this nx understands")]
    Newer {
        path: std::path::PathBuf,
        version: u32,
    },
    /// Two tool-tier packages expose one command.
    #[error("two tool-tier packages expose {command}")]
    Conflict { command: String },
    /// Files the extension would carry that something else has: the image,
    /// or another package of the same generation. One entry per pair.
    #[error("{}", overlap_summary(.0))]
    Overlap(Vec<generation::Overlap>),
    /// Packages whose scriptlets nx can't translate.
    #[error("{}", refusal_summary(.0))]
    Scriptlets(Vec<generation::Refusal>),
    #[error("serializing {what}: {message}")]
    Serialize { what: &'static str, message: String },
    #[error("{0}")]
    Invalid(String),
}

fn refusal_summary(r: &[generation::Refusal]) -> String {
    let names: Vec<&str> = r.iter().map(|r| r.nevra.as_str()).collect();
    format!(
        "scriptlets nx doesn't run or translate: {}",
        names.join(", ")
    )
}

fn overlap_summary(o: &[generation::Overlap]) -> String {
    o.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
