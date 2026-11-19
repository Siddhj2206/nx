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
    /// Deployments built by hand, for a source that knows only some of them.
    pub fn new(deployments: Vec<Deployment>) -> Self {
        Self { deployments }
    }

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

/// Just the booted deployment, read without rpm-ostree's daemon, which is
/// not up early in boot (`nx select` runs before `systemd-sysext`). The boot
/// loader's `ostree=` argument names the deployment through a symlink, and
/// the image's digest is in its commit's metadata.
#[derive(Debug)]
pub struct BootedOstree {
    /// `/`, or a fixture.
    pub root: PathBuf,
    /// The kernel command line.
    pub cmdline: String,
}

impl BootedOstree {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let path = root.join("proc/cmdline");
        let cmdline =
            std::fs::read_to_string(&path).map_err(|source| Error::Read { path, source })?;
        Ok(Self { root, cmdline })
    }

    /// `(osname, checksum, serial)` of the deployment a boot-loader path
    /// resolves to: `…/deploy/<osname>/deploy/<checksum>.<serial>`.
    pub fn parse(resolved: &Path) -> Option<(String, String, u32)> {
        let leaf = resolved.file_name()?.to_str()?;
        let (checksum, serial) = leaf.rsplit_once('.')?;
        let osname = resolved.parent()?.parent()?.file_name()?.to_str()?;
        Some((osname.to_owned(), checksum.to_owned(), serial.parse().ok()?))
    }

    /// The `ostree=` argument's path, e.g. `/ostree/boot.1/default/<bootcsum>/0`.
    pub fn boot_path(cmdline: &str) -> Option<&str> {
        cmdline
            .split_whitespace()
            .find_map(|arg| arg.strip_prefix("ostree="))
    }
}

impl DeploymentSource for BootedOstree {
    fn deployments(&self) -> Result<Deployments> {
        let invalid = |what: &str| Error::Invalid(format!("the booted deployment: {what}"));
        let boot = Self::boot_path(&self.cmdline).ok_or_else(|| invalid("no ostree= argument"))?;
        let link = self.root.join(boot.trim_start_matches('/'));
        let resolved = std::fs::canonicalize(&link).map_err(|source| Error::Read {
            path: link.clone(),
            source,
        })?;
        let resolved = resolved
            .strip_prefix(&self.root)
            .map_or(resolved.clone(), |p| Path::new("/").join(p));
        let (osname, checksum, serial) =
            Self::parse(&resolved).ok_or_else(|| invalid("an unexpected path"))?;
        const COMMAND: &str = "ostree show --print-metadata-key=ostree.manifest-digest";
        let repo = self.root.join("sysroot/ostree/repo");
        let out = Command::new("ostree")
            .arg(format!("--repo={}", repo.display()))
            .args([
                "show",
                "--print-metadata-key=ostree.manifest-digest",
                &checksum,
            ])
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
        // A GVariant string: 'sha256:…'.
        let digest = String::from_utf8_lossy(&out.stdout)
            .trim()
            .trim_matches('\'')
            .to_owned();
        Ok(Deployments::new(vec![Deployment {
            id: format!("{osname}-{checksum}.{serial}"),
            osname,
            checksum,
            serial,
            booted: true,
            staged: false,
            pinned: false,
            image_reference: None,
            image_digest: Some(digest).filter(|d| d.starts_with("sha256:")),
            version: None,
        }]))
    }
}

impl DeploymentSource for Deployments {
    fn deployments(&self) -> Result<Deployments> {
        Ok(self.clone())
    }
}

#[cfg(test)]
mod booted_tests {
    use super::*;

    #[test]
    fn reads_the_boot_path_and_the_deployment_it_names() {
        let cmdline = "BOOT_IMAGE=(hd0,gpt3)/boot/ostree/default-8218/vmlinuz root=UUID=x \
                       ostree=/ostree/boot.1/default/8218/0 rw";
        assert_eq!(
            BootedOstree::boot_path(cmdline),
            Some("/ostree/boot.1/default/8218/0")
        );
        assert_eq!(BootedOstree::boot_path("ro quiet"), None);
        let deployment = Path::new("/sysroot/ostree/deploy/default/deploy/159c70.0");
        assert_eq!(
            BootedOstree::parse(deployment),
            Some(("default".to_owned(), "159c70".to_owned(), 0))
        );
        assert_eq!(BootedOstree::parse(Path::new("/nonsense")), None);
    }
}
