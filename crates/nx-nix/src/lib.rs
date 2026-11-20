//! The tool tier's Nix side (design, *Tool tier*).
//!
//! nx never writes `/nix/store` itself (ADR-0001): it decides what to
//! realise and runs `nix`, which asks the daemon. What lives here:
//!
//! - [`channel`]: the nixpkgs channel release the tool tier is pinned to.
//! - [`catalog`]: every package of a release, from its `packages.json.br`,
//!   for search, info and routing without evaluating anything.
//! - [`eval`]: store paths for the requested attributes, by one evaluation
//!   (ADR-0009).
//! - [`narinfo`]: closures and sizes from the binary caches, for the plan.
//! - [`log`]: Nix's `--log-format internal-json` stream, as notes to draw.
//! - [`nix`]: running `nix` itself, behind a trait so tests need no Nix.

pub mod catalog;
pub mod channel;
pub mod eval;
pub mod log;
pub mod narinfo;
pub mod nix;

pub use catalog::Catalog;
pub use channel::Release;
pub use nix::{Nix, System};
pub use nx_http::Http;

/// Where every store path lives.
pub const STORE: &str = "/nix/store/";

/// The binary cache nixpkgs builds go to.
pub const CACHE: &str = "https://cache.nixos.org";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fetching {url}: {reason}")]
    Fetch { url: String, reason: String },
    #[error("{path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("reading the nixpkgs package list: {0}")]
    Catalog(String),
    #[error("running nix: {0}")]
    Spawn(std::io::Error),
    /// `nix` ran and failed; the message is Nix's innermost error.
    #[error("{0}")]
    Nix(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<nx_http::FetchError> for Error {
    fn from(e: nx_http::FetchError) -> Self {
        Self::Fetch {
            url: e.url,
            reason: e.reason,
        }
    }
}

/// GETs `url` as text, with the URL it came from after redirects; `None`
/// when the server has no such file (a binary cache's "don't have it").
pub fn text(http: &dyn Http, url: &str) -> Result<Option<(String, String)>> {
    match http.fetch(url) {
        Ok((body, at)) => {
            String::from_utf8(body)
                .map(|t| Some((t, at)))
                .map_err(|_| Error::Fetch {
                    url: url.to_owned(),
                    reason: "the answer is not text".to_owned(),
                })
        }
        Err(e) if e.not_found() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `/nix/store/<hash>-ripgrep-15.2.0` → `ripgrep-15.2.0`; the input when it
/// isn't a store path.
pub fn store_name(path: &str) -> &str {
    let base = path.strip_prefix(STORE).unwrap_or(path);
    match base.split_once('-') {
        Some((hash, name)) if hash.len() == 32 => name,
        _ => base,
    }
}

/// `/nix/store/<hash>-…` → `<hash>`, or `None` for anything else.
pub fn store_hash(path: &str) -> Option<&str> {
    let base = path.strip_prefix(STORE)?;
    let (hash, _) = base.split_once('-')?;
    (hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(hash)
}

/// Whether `path` is a store path itself (not something inside one).
pub fn is_store_path(path: &str) -> bool {
    store_hash(path).is_some()
        && path
            .strip_prefix(STORE)
            .is_some_and(|b| !b.contains('/') && !b.contains(".."))
}

/// Strips the colour codes Nix puts in its messages.
pub fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_parts() {
        let p = "/nix/store/76bbldfx3g88jxaqjakksgc5mvl1c3kb-ripgrep-15.2.0";
        assert_eq!(store_name(p), "ripgrep-15.2.0");
        assert_eq!(store_hash(p), Some("76bbldfx3g88jxaqjakksgc5mvl1c3kb"));
        assert!(is_store_path(p));
        assert!(!is_store_path(&format!("{p}/bin/rg")));
        assert!(!is_store_path("/usr/bin/rg"));
        assert_eq!(store_hash("/nix/store/short-x"), None);
    }

    #[test]
    fn strips_colour() {
        assert_eq!(
            strip_ansi("\u{1b}[31;1merror:\u{1b}[0m path '\u{1b}[35;1m/x\u{1b}[0m'"),
            "error: path '/x'"
        );
    }
}
