//! nx 0.x's state, read for `nx migrate` and `nx doctor`: the user's Nix
//! profile (`nix profile` packages, on `PATH` through `~/.nix-profile`) and
//! the user flake registry that pinned nixpkgs for it. nx 1.0 uses neither.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Nix, Result};

/// One installed package of the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// What `nix profile remove` takes.
    pub name: String,
    /// The nixpkgs attribute (`legacyPackages.<system>.` dropped), or the
    /// whole attribute path for something that isn't from nixpkgs.
    pub attr: String,
    /// Whether it came from nixpkgs (`flake:nixpkgs` or a nixpkgs URL).
    pub nixpkgs: bool,
}

/// Where nx 0.x kept things, under one home directory.
#[derive(Debug, Clone)]
pub struct Legacy {
    /// `$XDG_STATE_HOME/nix/profiles/profile`.
    pub profile: PathBuf,
    /// `~/.nix-profile`, a link to `profile`.
    pub link: PathBuf,
    /// `$XDG_CONFIG_HOME/nix/registry.json`.
    pub registry: PathBuf,
}

impl Legacy {
    pub fn new(home: &Path, state_home: Option<&Path>, config_home: Option<&Path>) -> Self {
        let state = state_home.map_or_else(|| home.join(".local/state"), Path::to_path_buf);
        let config = config_home.map_or_else(|| home.join(".config"), Path::to_path_buf);
        Self {
            profile: state.join("nix/profiles/profile"),
            link: home.join(".nix-profile"),
            registry: config.join("nix/registry.json"),
        }
    }

    /// The profile's active packages, sorted by name. No profile: none.
    pub fn elements(&self) -> Result<Vec<Element>> {
        read_manifest(&self.profile.join("manifest.json"))
    }

    /// Whether the user registry pins nixpkgs, as nx 0.x did.
    pub fn pins_nixpkgs(&self) -> bool {
        std::fs::read_to_string(&self.registry)
            .ok()
            .and_then(|t| serde_json::from_str::<Registry>(&t).ok())
            .is_some_and(|r| r.flakes.iter().any(|f| f.from.id == "nixpkgs"))
    }
}

/// `nix profile remove`: the profile keeps a generation from before, so
/// `nix profile rollback` undoes it.
pub fn remove(nix: &dyn Nix, names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut args = vec!["profile".to_owned(), "remove".to_owned()];
    args.extend(names.iter().cloned());
    nix.run(&args, &mut |_| {}).map(|_| ())
}

/// `nix registry remove nixpkgs`: drops nx 0.x's pin from the user registry.
pub fn unpin(nix: &dyn Nix) -> Result<()> {
    let args = ["registry", "remove", "nixpkgs"].map(str::to_owned);
    nix.run(&args, &mut |_| {}).map(|_| ())
}

#[derive(Deserialize)]
struct Manifest {
    version: u32,
    #[serde(default)]
    elements: std::collections::BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(default)]
    active: bool,
    #[serde(default)]
    attr_path: String,
    #[serde(default)]
    original_url: String,
}

#[derive(Deserialize)]
struct Registry {
    #[serde(default)]
    flakes: Vec<Flake>,
}

#[derive(Deserialize)]
struct Flake {
    from: From,
}

#[derive(Deserialize)]
struct From {
    #[serde(default)]
    id: String,
}

fn read_manifest(path: &Path) -> Result<Vec<Element>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let m: Manifest = serde_json::from_str(&text)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    if m.version < 3 {
        return Err(Error::Invalid(format!(
            "{}: profile version {} is older than nx reads; run nix profile list once to upgrade it",
            path.display(),
            m.version
        )));
    }
    Ok(m.elements
        .into_iter()
        .filter(|(_, e)| e.active)
        .map(|(name, e)| {
            let parts: Vec<&str> = e.attr_path.splitn(3, '.').collect();
            let attr = match parts.as_slice() {
                ["legacyPackages", _, attr] => (*attr).to_owned(),
                _ => e.attr_path.clone(),
            };
            let nixpkgs = e.original_url == "flake:nixpkgs"
                || e.original_url.contains("NixOS/nixpkgs")
                || e.original_url.starts_with("nixpkgs");
            Element {
                name,
                attr,
                nixpkgs,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nx_0x_profile() -> Result<()> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/profile/p");
        let got = read_manifest(&dir.join("manifest.json"))?;
        assert_eq!(
            got,
            [
                Element {
                    name: "neovim".into(),
                    attr: "neovim".into(),
                    nixpkgs: true,
                },
                Element {
                    name: "ripgrep".into(),
                    attr: "ripgrep".into(),
                    nixpkgs: true,
                },
            ]
        );
        assert!(read_manifest(&dir.join("absent.json"))?.is_empty());
        Ok(())
    }

    #[test]
    fn removes_through_nix() -> Result<()> {
        let nix = crate::nix::tests::Fake::answering("");
        remove(&nix, &[])?;
        remove(&nix, &["ripgrep".into(), "neovim".into()])?;
        unpin(&nix)?;
        assert_eq!(
            nix.calls(),
            [
                vec!["profile", "remove", "ripgrep", "neovim"],
                vec!["registry", "remove", "nixpkgs"],
            ]
        );
        Ok(())
    }

    #[test]
    fn paths_follow_xdg() {
        let l = Legacy::new(Path::new("/h"), Some(Path::new("/s")), None);
        assert_eq!(l.profile, Path::new("/s/nix/profiles/profile"));
        assert_eq!(l.link, Path::new("/h/.nix-profile"));
        assert_eq!(l.registry, Path::new("/h/.config/nix/registry.json"));
    }
}
