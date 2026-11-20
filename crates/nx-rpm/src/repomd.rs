//! The two small XML files in front of a repo's metadata: the metalink that
//! names mirrors and the expected checksum of `repomd.xml`, and `repomd.xml`
//! itself, which names and checksums every other metadata file.

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::{Error, Result};

/// A checksum as metadata states it: algorithm and lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub kind: String,
    pub hex: String,
}

/// What a metalink says about `repomd.xml`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Metalink {
    /// Accepted checksums of `repomd.xml`. A metalink lists the current one
    /// and a few recent ones, since mirrors lag.
    pub repomd_checksums: Vec<Checksum>,
    /// Mirror URLs of `repomd.xml`, in the metalink's preference order.
    pub urls: Vec<String>,
}

impl Metalink {
    /// Mirror base URLs: each `…/repodata/repomd.xml` with that suffix cut.
    pub fn bases(&self) -> impl Iterator<Item = &str> {
        self.urls
            .iter()
            .filter_map(|u| u.strip_suffix("repodata/repomd.xml"))
    }
}

/// One metadata file `repomd.xml` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFile {
    /// `primary`, `filelists`, `updateinfo`, …
    pub kind: String,
    /// Path relative to the repo base, e.g. `repodata/…-primary.xml.zst`.
    pub href: String,
    pub checksum: Checksum,
    pub size: Option<u64>,
}

/// A parsed `repomd.xml`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Repomd {
    pub revision: Option<String>,
    pub data: Vec<DataFile>,
}

impl Repomd {
    pub fn get(&self, kind: &str) -> Option<&DataFile> {
        self.data.iter().find(|d| d.kind == kind)
    }
}

fn xml_error(what: &'static str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("parsing {what}: {e}"))
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn attr(e: &quick_xml::events::BytesStart<'_>, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| local_name(a.key.as_ref()) == key)
        .and_then(|a| a.normalized_value(XmlVersion::Implicit1_0).ok())
        .map(|v| v.into_owned())
}

/// Element text with entities resolved; metadata text is checksums, URLs
/// and numbers.
fn text(t: &quick_xml::events::BytesText<'_>, what: &'static str) -> Result<String> {
    quick_xml::escape::unescape(&t.xml10_content())
        .map(|s| s.trim().to_owned())
        .map_err(|e| xml_error(what, e))
}

/// Parses a Fedora metalink (`mirrors.fedoraproject.org/metalink?…`).
pub fn parse_metalink(xml: &str) -> Result<Metalink> {
    let mut reader = Reader::from_str(xml);
    let mut out = Metalink::default();
    let mut in_repomd = false;
    let mut hash_kind: Option<String> = None;
    let mut in_url = false;
    // Alternates nest a second <verification> per older repomd; their
    // hashes are just as valid.
    loop {
        match reader.read_event().map_err(|e| xml_error("metalink", e))? {
            Event::Start(e) => match local_name(e.name().as_ref()) {
                "file" => in_repomd = attr(&e, "name").as_deref() == Some("repomd.xml"),
                "hash" if in_repomd => hash_kind = attr(&e, "type"),
                "url" if in_repomd => in_url = true,
                _ => {}
            },
            Event::Text(t) => {
                let text = text(&t, "metalink")?;
                let text = text.as_str();
                if let Some(kind) = hash_kind.take() {
                    out.repomd_checksums.push(Checksum {
                        kind,
                        hex: text.to_ascii_lowercase(),
                    });
                } else if in_url && text.starts_with("http") {
                    out.urls.push(text.to_owned());
                }
            }
            Event::End(e) => match local_name(e.name().as_ref()) {
                "file" => in_repomd = false,
                "hash" => hash_kind = None,
                "url" => in_url = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    if out.urls.is_empty() {
        return Err(Error::Invalid("metalink lists no mirrors".into()));
    }
    Ok(out)
}

/// Parses `repomd.xml`.
pub fn parse_repomd(xml: &str) -> Result<Repomd> {
    let mut reader = Reader::from_str(xml);
    let mut out = Repomd::default();
    // kind, href, checksum, size of the <data> being read.
    type Partial = (String, Option<String>, Option<Checksum>, Option<u64>);
    let mut current: Option<Partial> = None;
    let mut field: Option<String> = None;
    let mut checksum_kind = String::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| xml_error("repomd.xml", e))?
        {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref()).to_owned();
                match name.as_str() {
                    "data" => current = attr(&e, "type").map(|t| (t, None, None, None)),
                    "checksum" => checksum_kind = attr(&e, "type").unwrap_or_default(),
                    _ => {}
                }
                field = Some(name);
            }
            Event::Empty(e) => {
                if local_name(e.name().as_ref()) == "location"
                    && let Some(cur) = current.as_mut()
                {
                    cur.1 = attr(&e, "href");
                }
            }
            Event::Text(t) => {
                let text = text(&t, "repomd.xml")?;
                let text = text.as_str();
                match (field.as_deref(), current.as_mut()) {
                    (Some("revision"), None) => out.revision = Some(text.to_owned()),
                    (Some("checksum"), Some(cur)) => {
                        cur.2 = Some(Checksum {
                            kind: checksum_kind.clone(),
                            hex: text.to_ascii_lowercase(),
                        });
                    }
                    (Some("size"), Some(cur)) => cur.3 = text.parse().ok(),
                    _ => {}
                }
            }
            Event::End(e) => {
                field = None;
                if local_name(e.name().as_ref()) == "data"
                    && let Some((kind, Some(href), Some(checksum), size)) = current.take()
                {
                    out.data.push(DataFile {
                        kind,
                        href,
                        checksum,
                        size,
                    });
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metalink_gives_mirrors_and_checksums() -> Result<()> {
        let m = parse_metalink(include_str!("../tests/fixtures/metalink.xml"))?;
        assert!(m.urls.len() > 3);
        assert!(m.bases().all(|b| b.starts_with("http") && b.ends_with('/')));
        assert!(
            m.repomd_checksums
                .iter()
                .any(|c| c.kind == "sha256" && c.hex.len() == 64)
        );
        Ok(())
    }

    #[test]
    fn repomd_lists_primary() -> Result<()> {
        let r = parse_repomd(include_str!("../tests/fixtures/repomd.xml"))?;
        let primary = r.get("primary");
        assert!(
            primary.is_some_and(|p| p.href.starts_with("repodata/") && p.checksum.kind == "sha256")
        );
        assert!(r.revision.is_some());
        Ok(())
    }
}
