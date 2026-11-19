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
    /// nixpkgs packages, sealed beside the image (design, *Tool tier*).
    #[serde(default, skip_serializing_if = "Tool::is_empty")]
    pub tool: Tool,
    /// Extra Nix binary caches, tried with cache.nixos.org.
    #[serde(default, rename = "cache", skip_serializing_if = "Vec::is_empty")]
    pub caches: Vec<CacheEntry>,
    #[serde(default, skip_serializing_if = "Routing::is_default")]
    pub routing: Routing,
    #[serde(default, skip_serializing_if = "Overrides::is_empty")]
    pub overrides: Overrides,
    /// Sections this nx doesn't know yet, kept as written.
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
            tool: Tool::default(),
            caches: Vec::new(),
            routing: Routing::default(),
            overrides: Overrides::default(),
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

/// The tool tier's intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    /// nixpkgs attribute paths (`ripgrep`, `python3Packages.black`).
    #[serde(default)]
    pub packages: Vec<String>,
    /// Package names (`lib.getName`) allowed despite an unfree license.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unfree: Vec<String>,
}

impl Tool {
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.unfree.is_empty()
    }
}

/// A trusted extra Nix binary cache: its URL and the key its paths are
/// signed with (`name:base64`). nxd lists it in the daemon's
/// `trusted-substituters` when the generation goes live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub url: String,
    pub key: String,
}

/// Where a standalone command-line tool goes when both tiers have it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CliTools {
    /// nixpkgs, which moves with its own pin, not the image (the default).
    #[default]
    Nix,
    /// Fedora's RPM, in the host tier.
    Fedora,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routing {
    #[serde(default)]
    pub cli_tools: CliTools,
    /// Commands to treat as compilers, linkers or interpreters besides
    /// nx's own list (`nx_rpm::plugs::TOOLCHAINS`); `*` ends a prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub toolchains: Vec<String>,
    /// Commands from that list to treat as standalone tools instead
    /// (`zig`, say, for nixpkgs' copy).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standalone: Vec<String>,
}

impl Routing {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Command names a tool-tier package may expose although something else
/// has them (`nx add --override`): the image and the host tier keep theirs
/// (they come first on `PATH`), another tool package's is replaced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overrides {
    #[serde(default)]
    pub names: Vec<String>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
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

    /// Adds tool-tier `attrs` not already listed.
    pub fn add_tool(&mut self, attrs: &[String]) {
        for a in attrs {
            if !self.tool.packages.contains(a) {
                self.tool.packages.push(a.clone());
            }
        }
    }

    /// Whether `spec` is in either tier.
    pub fn has(&self, spec: &str) -> bool {
        self.host.packages.iter().any(|p| p == spec) || self.tool.packages.iter().any(|p| p == spec)
    }

    /// Whether nothing is asked of either tier.
    pub fn is_empty(&self) -> bool {
        self.host.packages.is_empty() && self.tool.packages.is_empty()
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

[future]
setting = "kept"

[tool]
packages = ["ripgrep"]
unfree = ["terraform"]

[[cache]]
url = "https://nix-community.cachix.org"
key = "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs="

[routing]
cli_tools = "fedora"
toolchains = ["dmd"]
standalone = ["zig"]

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
        assert!(text.contains("[future]"), "{text}");
        assert_eq!(m.tool.packages, ["ripgrep"]);
        assert_eq!(m.tool.unfree, ["terraform"]);
        assert_eq!(m.caches.len(), 1);
        assert_eq!(m.routing.cli_tools, CliTools::Fedora);
        assert_eq!(m.routing.toolchains, ["dmd"]);
        assert_eq!(m.routing.standalone, ["zig"]);
        assert!(m.has("ripgrep") && m.has("wayland-devel") && !m.has("nope"));
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
