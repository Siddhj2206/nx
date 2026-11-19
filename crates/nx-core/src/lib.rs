//! nx's view of the system it runs on.
//!
//! Everything nx installs is resolved against the booted bootc image, so the
//! first thing any command needs is which image that is, which deployments
//! exist beside it, and which Fedora release each one is. Every read here
//! goes through a root path or a [`DeploymentSource`] so tests run against
//! fixtures, without root and without a bootc system.

pub mod deployment;
pub mod os_release;

pub use deployment::{Deployment, DeploymentSource, Deployments, RpmOstree};
pub use os_release::OsRelease;

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
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
