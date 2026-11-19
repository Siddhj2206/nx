//! `lock.toml`: what nx resolved the manifest to, for one image digest.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, FORMAT, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    /// The image digest it was resolved for; `None` off a bootc system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The image's `VERSION_ID`.
    pub release: String,
    pub arch: String,
    #[serde(default, rename = "repo")]
    pub repos: Vec<LockedRepo>,
    #[serde(default, rename = "host")]
    pub host: Vec<LockedPackage>,
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
            ..Self::default()
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

    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    pub fn package(&self, name: &str) -> Option<&LockedPackage> {
        self.host.iter().find(|p| p.name == name)
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
        let text = lock.to_toml();
        assert!(text.contains("[[host]]"), "{text}");
        assert_eq!(Lock::parse(&text, Path::new("l"))?, lock);
        assert_eq!(
            lock.package("pkgconf").map(|p| p.sha256.as_str()),
            Some("ef")
        );
        Ok(())
    }
}
