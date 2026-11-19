//! `manifest.toml`: what the user asked for. Intent, safe to edit by hand
//! and to keep in dotfiles; nx rewrites it only to add or remove what a
//! command was asked to.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Backend, Error, FORMAT, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default = "format")]
    pub version: u32,
    #[serde(default)]
    pub host: Host,
    /// Repos added with `nx repo add`; the image's own repos are not listed.
    #[serde(default, rename = "repo", skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoEntry>,
    /// Packages whose leftover scriptlet text the user reviewed and chose to
    /// ignore (design, *Scriptlet translation*).
    #[serde(default, rename = "scriptlets", skip_serializing_if = "Vec::is_empty")]
    pub scriptlets: Vec<ScriptletAllow>,
    /// Sections this nx doesn't know yet (`[tool]` before M6), kept as
    /// written.
    #[serde(flatten)]
    pub rest: toml::Table,
}

fn format() -> u32 {
    FORMAT
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: FORMAT,
            host: Host::default(),
            repos: Vec::new(),
            scriptlets: Vec::new(),
            rest: toml::Table::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// What the user typed: names, provides or paths.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Forces the activation backend; unset, nxd probes and keeps the one
    /// that works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
}

/// A package's scriptlets, reviewed. `digest` fingerprints the text nx
/// couldn't translate: when the package's scriptlets change, so does it,
/// and the entry stops applying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptletAllow {
    pub package: String,
    pub digest: String,
}

/// Hex digits of a leftover text's fingerprint that name it in
/// `--allow-scriptlets`: enough that two texts don't collide.
pub const ALLOW_DIGEST_LEN: usize = 16;

impl ScriptletAllow {
    /// Whether this entry covers leftover text with fingerprint `digest`.
    /// [`ALLOW_DIGEST_LEN`] hex digits are enough to name it.
    pub fn matches(&self, digest: &str) -> bool {
        self.digest.len() >= ALLOW_DIGEST_LEN && digest.starts_with(&self.digest)
    }

    /// Parses `package=digest`.
    pub fn parse(text: &str) -> Option<Self> {
        let (package, digest) = text.split_once('=')?;
        let ok = digest.len() >= ALLOW_DIGEST_LEN && digest.bytes().all(|b| b.is_ascii_hexdigit());
        (ok && !package.is_empty()).then(|| Self {
            package: package.to_owned(),
            digest: digest.to_ascii_lowercase(),
        })
    }
}

/// One added repo. `copr:owner/project` needs nothing else; any other repo
/// records where it lives and its key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseurl: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metalink: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpgkey: Option<String>,
    /// dnf semantics: lower wins; Fedora's repos are 99, added ones 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

impl Manifest {
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let m: Self = toml::from_str(text).map_err(|e| Error::Toml {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        if m.version > FORMAT {
            return Err(Error::Newer {
                path: path.to_owned(),
                version: m.version,
            });
        }
        Ok(m)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).map_err(|e| Error::Serialize {
            what: "manifest.toml",
            message: e.to_string(),
        })
    }

    /// Adds `specs` not already listed.
    pub fn add_host(&mut self, specs: &[String]) {
        for spec in specs {
            if !self.host.packages.contains(spec) {
                self.host.packages.push(spec.clone());
            }
        }
    }

    /// Drops `specs`; returns the ones that were not listed.
    pub fn remove_host(&mut self, specs: &[String]) -> Vec<String> {
        let missing = specs
            .iter()
            .filter(|s| !self.host.packages.contains(s))
            .cloned()
            .collect();
        self.host.packages.retain(|p| !specs.contains(p));
        missing
    }

    pub fn repo(&self, name: &str) -> Option<&RepoEntry> {
        self.repos.iter().find(|r| r.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_what_it_does_not_know() -> Result<()> {
        let text = r#"
[host]
packages = ["wayland-devel"]
backend = "prefix"

[tool]
packages = ["ripgrep"]

[[repo]]
name = "copr:atim/lazygit"
priority = 50
"#;
        let mut m = Manifest::parse(text, Path::new("m"))?;
        assert_eq!(m.version, FORMAT);
        assert_eq!(m.host.backend, Some(Backend::Prefix));
        m.add_host(&["libxkbcommon-devel".into(), "wayland-devel".into()]);
        assert_eq!(m.host.packages, ["wayland-devel", "libxkbcommon-devel"]);
        assert_eq!(
            m.remove_host(&["wayland-devel".into(), "nope".into()]),
            ["nope"]
        );
        assert_eq!(m.host.packages, ["libxkbcommon-devel"]);
        m.add_host(&["wayland-devel".into()]);
        let text = m.to_toml()?;
        assert_eq!(Manifest::parse(&text, Path::new("m"))?, m);
        assert!(text.contains("[tool]"), "{text}");
        assert_eq!(
            m.repo("copr:atim/lazygit").and_then(|r| r.priority),
            Some(50)
        );
        Ok(())
    }

    #[test]
    fn refuses_a_newer_format() {
        let err = Manifest::parse("version = 99", Path::new("m"));
        assert!(
            matches!(err, Err(Error::Newer { version: 99, .. })),
            "{err:?}"
        );
    }
}
