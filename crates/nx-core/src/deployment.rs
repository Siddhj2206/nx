//! The image deployments on this machine: booted, staged and rollback.
//!
//! The host tier is resolved per image digest, and every deployed digest
//! keeps a generation (design invariant 5), so nx needs all of them, not just
//! the booted one. `bootc status` needs root and the CLI runs unprivileged;
//! `rpm-ostree status --json` gives the same facts without it.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::{Error, Result};

/// One deployment as rpm-ostree reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub osname: String,
    /// The ostree commit; with `serial`, names the deployment's directory.
    pub checksum: String,
    #[serde(default)]
    pub serial: u32,
    #[serde(default)]
    pub booted: bool,
    #[serde(default)]
    pub staged: bool,
    #[serde(default)]
    pub pinned: bool,
    /// e.g. `ostree-unverified-registry:ghcr.io/siddhj2206/neptuno:latest`.
    #[serde(rename = "container-image-reference")]
    pub image_reference: Option<String>,
    /// The image's manifest digest, `sha256:…`. What generations are keyed by.
    #[serde(rename = "container-image-reference-digest")]
    pub image_digest: Option<String>,
    /// The image's version label, e.g. `45.20261118`.
    pub version: Option<String>,
}

impl Deployment {
    /// The deployment's root filesystem under `sysroot` (normally
    /// `/sysroot`). Its rpmdb and os-release are readable unprivileged.
    pub fn root(&self, sysroot: &Path) -> PathBuf {
        sysroot
            .join("ostree/deploy")
            .join(&self.osname)
            .join("deploy")
            .join(format!("{}.{}", self.checksum, self.serial))
    }

    /// The image reference without its ostree transport prefix, e.g.
    /// `ghcr.io/siddhj2206/neptuno:latest`.
    pub fn image(&self) -> Option<&str> {
        let reference = self.image_reference.as_deref()?;
        Some(
            reference
                .rsplit_once("registry:")
                .map_or(reference, |(_, image)| image),
        )
    }
}

/// Every deployment, in rpm-ostree's order.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Deployments {
    deployments: Vec<Deployment>,
}

impl Deployments {
    pub fn from_json(json: &str) -> Result<Self> {
        let parsed: Self = serde_json::from_str(json).map_err(|source| Error::Json {
            what: "rpm-ostree status",
            source,
        })?;
        if parsed.booted().is_none() {
            return Err(Error::Invalid(
                "rpm-ostree reports no booted deployment".into(),
            ));
        }
        Ok(parsed)
    }

    pub fn all(&self) -> &[Deployment] {
        &self.deployments
    }

    /// The running deployment. Present by construction.
    pub fn booted(&self) -> Option<&Deployment> {
        self.deployments.iter().find(|d| d.booted)
    }

    /// The deployment the next boot will use, if an update is staged.
    pub fn staged(&self) -> Option<&Deployment> {
        self.deployments.iter().find(|d| d.staged)
    }

    /// The deployment `bootc rollback` would boot: the first that is neither
    /// booted nor staged.
    pub fn rollback(&self) -> Option<&Deployment> {
        self.deployments.iter().find(|d| !d.booted && !d.staged)
    }
}

/// Where deployment state comes from. The real one runs rpm-ostree; tests
/// pass fixtures.
pub trait DeploymentSource {
    fn deployments(&self) -> Result<Deployments>;
}

/// `rpm-ostree status --json`, unprivileged.
#[derive(Debug, Default)]
pub struct RpmOstree;

impl DeploymentSource for RpmOstree {
    fn deployments(&self) -> Result<Deployments> {
        const COMMAND: &str = "rpm-ostree status --json";
        let out = Command::new("rpm-ostree")
            .args(["status", "--json"])
            .output()
            .map_err(|source| Error::Spawn {
                command: COMMAND,
                source,
            })?;
        if !out.status.success() {
            return Err(Error::Command {
                command: COMMAND,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            });
        }
        Deployments::from_json(&String::from_utf8_lossy(&out.stdout))
    }
}

impl DeploymentSource for Deployments {
    fn deployments(&self) -> Result<Deployments> {
        Ok(self.clone())
    }
}
