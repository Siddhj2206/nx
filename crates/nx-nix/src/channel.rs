//! The nixpkgs channel release the tool tier is pinned to (ADR-0004). A
//! channel only advances once Hydra has built the release, so its store
//! paths are in cache.nixos.org, and its `packages.json.br` matches it.

use serde::{Deserialize, Serialize};

use crate::web::Web;
use crate::{Error, Result};

/// Where releases come from.
pub const CHANNEL: &str = "https://channels.nixos.org/nixpkgs-unstable";

/// One channel release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// The nixpkgs git revision.
    pub rev: String,
    /// `nixpkgs-26.11pre1122334.e21fec321d73`.
    pub name: String,
    /// The release directory: `packages.json.br` and `nixexprs.tar.xz`
    /// are under it.
    pub url: String,
}

impl Release {
    /// The first 12 characters of the revision, as the release name has it.
    pub fn short(&self) -> &str {
        self.rev.get(..12).unwrap_or(&self.rev)
    }

    /// The nixpkgs source of this release, which nx evaluates.
    pub fn nixexprs(&self) -> String {
        format!("{}/nixexprs.tar.xz", self.url)
    }

    pub fn packages_json(&self) -> String {
        format!("{}/packages.json.br", self.url)
    }

    /// `nixpkgs-26.11pre1122334.e21fec321d73` → `26.11pre1122334`.
    pub fn version(&self) -> &str {
        let v = self.name.strip_prefix("nixpkgs-").unwrap_or(&self.name);
        v.rsplit_once('.').map_or(v, |(head, _)| head)
    }
}

/// Asks the channel for its current release: one small request, which
/// redirects to the release directory.
pub fn latest(web: &Web) -> Result<Release> {
    let url = format!("{CHANNEL}/git-revision");
    let (rev, at) = web.text(&url)?.ok_or_else(|| Error::Fetch {
        url: url.clone(),
        reason: "not found".to_owned(),
    })?;
    from_answer(rev.trim(), &at)
}

fn from_answer(rev: &str, at: &str) -> Result<Release> {
    let dir = at.strip_suffix("/git-revision").unwrap_or(at);
    let name = dir.rsplit('/').next().unwrap_or_default();
    if rev.len() != 40
        || !rev.bytes().all(|b| b.is_ascii_hexdigit())
        || !name.starts_with("nixpkgs-")
    {
        return Err(Error::Invalid(format!(
            "the nixpkgs channel answered something unexpected ({rev:?} from {at})"
        )));
    }
    Ok(Release {
        rev: rev.to_owned(),
        name: name.to_owned(),
        url: dir.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_redirected_answer() -> Result<()> {
        let r = from_answer(
            "e21fec321d73511849707a7585e4ea155ee4bf40",
            "https://releases.nixos.org/nixpkgs/nixpkgs-26.11pre1122334.e21fec321d73/git-revision",
        )?;
        assert_eq!(r.name, "nixpkgs-26.11pre1122334.e21fec321d73");
        assert_eq!(r.short(), "e21fec321d73");
        assert_eq!(r.version(), "26.11pre1122334");
        assert_eq!(
            r.nixexprs(),
            "https://releases.nixos.org/nixpkgs/nixpkgs-26.11pre1122334.e21fec321d73/nixexprs.tar.xz"
        );
        assert!(from_answer("<html>", "https://example.org/x").is_err());
        Ok(())
    }
}
