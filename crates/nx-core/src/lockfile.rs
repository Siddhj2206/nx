//! `lock.toml`: what nx resolved the manifest to, for one image digest.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, FORMAT, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    /// The image digest it was resolved for; `None` off a bootc system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The image's `VERSION_ID`.
    pub release: String,
    pub arch: String,
    /// The generation this one was built from (the current one at the
    /// time), so `nx rollback` undoes a change even after earlier
    /// rollbacks made the numbers say otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    /// A placeholder `nx select` made at boot because no generation fit the
    /// booted image: empty, and still to be resolved by `nx sync`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    /// Manifest packages this generation leaves out because they don't
    /// resolve against its image (a repo that has nothing for the new
    /// release yet). `nx sync` tries them again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    #[serde(default, rename = "repo")]
    pub repos: Vec<LockedRepo>,
    #[serde(default, rename = "host")]
    pub host: Vec<LockedPackage>,
    /// The tool tier: the nixpkgs release and what was realised from it.
    /// The same in every image's generation: it doesn't depend on the image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<LockedTool>,
}

/// The tool tier as resolved: one pinned release (ADR-0004) and each
/// package's store paths, evaluated once (ADR-0009).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedTool {
    pub rev: String,
    /// The channel release's name (`nixpkgs-26.11pre1122334.e21fec321d73`).
    pub release: String,
    /// The release directory.
    pub url: String,
    #[serde(default, rename = "package", skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<ToolPackage>,
}

impl LockedTool {
    pub fn package(&self, attr: &str) -> Option<&ToolPackage> {
        self.packages.iter().find(|p| p.attr == attr)
    }

    /// Every exposed command, with the package that exposes it.
    pub fn bins(&self) -> impl Iterator<Item = (&ToolPackage, &Bin)> {
        self.packages
            .iter()
            .flat_map(|p| p.bins.iter().map(move |b| (p, b)))
    }
}

/// One tool-tier package.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPackage {
    /// The attribute path, as in the manifest.
    pub attr: String,
    /// The package's name and version, as nixpkgs has them.
    pub name: String,
    pub version: String,
    /// Its derivation, for building what no cache has.
    pub drv: String,
    /// `meta.outputsToInstall` and their store paths: the generation's GC
    /// roots.
    pub outputs: Vec<ToolOutput>,
    /// The commands it exposes; names something else owns are left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<Bin>,
    /// Built locally because it is unfree (no cache has it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unfree: bool,
}

impl ToolPackage {
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.outputs.iter().map(|o| o.path.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub name: String,
    pub path: String,
}

/// An exposed command and the store file it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bin {
    pub name: String,
    pub path: String,
}

/// The metadata a resolve used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedRepo {
    pub id: String,
    /// `primary` metadata checksum.
    pub primary: String,
    /// `repomd.xml`'s revision (a timestamp for Fedora).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Fingerprints of the keys its packages must verify against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

/// One RPM in the host tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    pub nevra: String,
    /// The repo it came from, or `koji` for an image relative's exact build.
    pub repo: String,
    /// The package file's SHA-256; its store directory's name.
    pub sha256: String,
    /// The manifest entries that pulled it in.
    pub requested: Vec<String>,
}

impl Lock {
    pub fn new(image: Option<String>, release: &str, arch: &str) -> Self {
        Self {
            version: FORMAT,
            image,
            release: release.to_owned(),
            arch: arch.to_owned(),
            parent: None,
            pending: false,
            skipped: Vec::new(),
            repos: Vec::new(),
            host: Vec::new(),
            tool: None,
        }
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let lock: Self = toml::from_str(text).map_err(|e| Error::Toml {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        if lock.version > FORMAT {
            return Err(Error::Newer {
                path: path.to_owned(),
                version: lock.version,
            });
        }
        Ok(lock)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).map_err(|e| Error::Serialize {
            what: "lock.toml",
            message: e.to_string(),
        })
    }

    pub fn package(&self, name: &str) -> Option<&LockedPackage> {
        self.host.iter().find(|p| p.name == name)
    }

    pub fn repo(&self, id: &str) -> Option<&LockedRepo> {
        self.repos.iter().find(|r| r.id == id)
    }
}

impl LockedPackage {
    /// `pkgconf-2.4-1.fc45.x86_64` → `2.4-1.fc45`.
    pub fn evr(&self) -> &str {
        let rest = self
            .nevra
            .strip_prefix(&self.name)
            .and_then(|r| r.strip_prefix('-'))
            .unwrap_or(&self.nevra);
        rest.rsplit_once('.').map_or(rest, |(evr, _arch)| evr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() -> Result<()> {
        let mut lock = Lock::new(Some("sha256:ab".into()), "45", "x86_64");
        lock.repos.push(LockedRepo {
            id: "fedora".into(),
            primary: "cd".into(),
            revision: Some("1731".into()),
            keys: vec![],
        });
        lock.host.push(LockedPackage {
            name: "pkgconf".into(),
            nevra: "pkgconf-2.4-1.fc45.x86_64".into(),
            repo: "fedora".into(),
            sha256: "ef".into(),
            requested: vec!["libxkbcommon-devel".into()],
        });
        lock.tool = Some(LockedTool {
            rev: "e21fec321d73511849707a7585e4ea155ee4bf40".into(),
            release: "nixpkgs-26.11pre1122334.e21fec321d73".into(),
            url: "https://releases.nixos.org/nixpkgs/nixpkgs-26.11pre1122334.e21fec321d73".into(),
            packages: vec![ToolPackage {
                attr: "jq".into(),
                name: "jq".into(),
                version: "1.8.2".into(),
                drv: "/nix/store/8ydjymrq0hlzxz7c1h1r1qsa19iw8js6-jq-1.8.2.drv".into(),
                outputs: vec![ToolOutput {
                    name: "bin".into(),
                    path: "/nix/store/nh0wly5a1h79bfjgw46sly70ljdy1xkl-jq-1.8.2-bin".into(),
                }],
                bins: vec![Bin {
                    name: "jq".into(),
                    path: "/nix/store/nh0wly5a1h79bfjgw46sly70ljdy1xkl-jq-1.8.2-bin/bin/jq".into(),
                }],
                unfree: false,
            }],
        });
        let text = lock.to_toml()?;
        assert!(text.contains("[[host]]"), "{text}");
        assert!(text.contains("[[tool.package]]"), "{text}");
        assert_eq!(Lock::parse(&text, Path::new("l"))?, lock);
        assert_eq!(
            lock.package("pkgconf").map(|p| p.sha256.as_str()),
            Some("ef")
        );
        assert_eq!(lock.host[0].evr(), "2.4-1.fc45");
        assert_eq!(lock.repo("fedora").map(|r| r.primary.as_str()), Some("cd"));
        Ok(())
    }
}
