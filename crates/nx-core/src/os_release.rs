//! `os-release(5)`: the release a root filesystem belongs to.

use std::collections::BTreeMap;
use std::path::Path;

use crate::{Error, Result};

/// The fields of an os-release file. nx keys the host tier on `ID` and
/// `VERSION_ID`, so those two are required; the rest are kept for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsRelease {
    pub id: String,
    pub version_id: String,
    fields: BTreeMap<String, String>,
}

impl OsRelease {
    /// Reads `usr/lib/os-release` under `root`, falling back to
    /// `etc/os-release` as the spec says. `root` is `/` for the running
    /// system, or a deployment's directory for another one.
    pub fn read(root: &Path) -> Result<Self> {
        let primary = root.join("usr/lib/os-release");
        let text = match std::fs::read_to_string(&primary) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let fallback = root.join("etc/os-release");
                std::fs::read_to_string(&fallback).map_err(|source| Error::Read {
                    path: fallback,
                    source,
                })?
            }
            Err(source) => {
                return Err(Error::Read {
                    path: primary,
                    source,
                });
            }
        };
        Self::parse(&text)
    }

    /// Parses os-release text: `KEY=value` lines, values optionally quoted
    /// with shell-style escapes, `#` comments and blank lines ignored.
    pub fn parse(text: &str) -> Result<Self> {
        let mut fields = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            fields.insert(key.trim().to_owned(), unquote(value.trim()));
        }
        let field = |key: &str| {
            fields
                .get(key)
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| Error::Invalid(format!("os-release has no {key}")))
        };
        Ok(Self {
            id: field("ID")?,
            version_id: field("VERSION_ID")?,
            fields,
        })
    }

    /// Any other field, e.g. `PRETTY_NAME` or `VERSION`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
}

fn unquote(value: &str) -> String {
    let inner = match value.as_bytes() {
        [b'"', .., b'"'] | [b'\'', .., b'\''] if value.len() >= 2 => &value[1..value.len() - 1],
        _ => return value.to_owned(),
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
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

    const NEPTUNO: &str = r#"NAME="Neptuno"
VERSION="45.20261118 (base-atomic)"
ID=fedora
VERSION_ID=45
VERSION_CODENAME=""
PRETTY_NAME="Neptuno 45.20261118"
# a comment
"#;

    #[test]
    fn parses_quoted_and_bare_values() -> Result<()> {
        let os = OsRelease::parse(NEPTUNO)?;
        assert_eq!(os.id, "fedora");
        assert_eq!(os.version_id, "45");
        assert_eq!(os.get("PRETTY_NAME"), Some("Neptuno 45.20261118"));
        assert_eq!(os.get("VERSION_CODENAME"), Some(""));
        Ok(())
    }

    #[test]
    fn handles_escapes() -> Result<()> {
        let os = OsRelease::parse("ID=fedora\nVERSION_ID=45\nNAME=\"a \\\"b\\\" \\\\c\"\n")?;
        assert_eq!(os.get("NAME"), Some(r#"a "b" \c"#));
        Ok(())
    }

    #[test]
    fn requires_id_and_version_id() {
        assert!(OsRelease::parse("ID=fedora\n").is_err());
        assert!(OsRelease::parse("ID=fedora\nVERSION_ID=\"\"\n").is_err());
    }

    #[test]
    fn reads_from_a_root_with_etc_fallback() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-os-release-{}", std::process::id()));
        let etc = dir.join("etc");
        std::fs::create_dir_all(&etc).map_err(|source| Error::Read {
            path: etc.clone(),
            source,
        })?;
        std::fs::write(etc.join("os-release"), NEPTUNO).map_err(|source| Error::Read {
            path: etc.clone(),
            source,
        })?;
        let os = OsRelease::read(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(os?.version_id, "45");
        Ok(())
    }
}
