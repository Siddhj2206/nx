//! The seam both tiers fetch through: [`Http`], with [`Ureq`] for the
//! network and a map of canned answers in tests.

use std::io::{Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use ureq::ResponseExt;

use crate::Client;

/// A request that failed: the URL, why, and the HTTP status when there was
/// an answer.
#[derive(Debug, thiserror::Error)]
#[error("fetching {url}: {reason}")]
pub struct Error {
    pub url: String,
    pub reason: String,
    pub status: Option<u16>,
}

impl Error {
    pub fn new(url: &str, reason: impl Into<String>) -> Self {
        Self {
            url: url.to_owned(),
            reason: reason.into(),
            status: None,
        }
    }

    /// The server answered `status`.
    pub fn status(url: &str, status: u16) -> Self {
        Self {
            status: Some(status),
            ..Self::new(url, format!("http status: {status}"))
        }
    }

    /// The server has no such file: 404, or 403 (how a binary cache backed
    /// by S3 says it).
    pub fn not_found(&self) -> bool {
        matches!(self.status, Some(403 | 404))
    }

    fn from_ureq(url: &str, e: &ureq::Error) -> Self {
        match e {
            ureq::Error::StatusCode(code) => Self::status(url, *code),
            e => Self::new(url, e.to_string()),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Something that can GET a URL. [`Ureq`] is the real one; tests answer
/// from a map. Shared across download threads.
pub trait Http: Sync {
    /// The body, and the URL it came from after redirects.
    fn fetch(&self, url: &str) -> Result<(Vec<u8>, String)>;

    /// The body.
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.fetch(url).map(|(body, _)| body)
    }

    /// The body as a stream, for a large file read once. The default holds
    /// it in memory; [`Ureq`] streams it.
    fn reader(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::io::Cursor::new(self.get(url)?)))
    }

    /// GETs `url` into the file `to` (created or replaced) and returns the
    /// SHA-256 of what was written. The default holds the body in memory;
    /// [`Ureq`] streams it, so a large package never is.
    fn download(&self, url: &str, to: &Path) -> Result<String> {
        let body = self.get(url)?;
        std::fs::write(to, &body).map_err(|e| Error::new(url, format!("{}: {e}", to.display())))?;
        Ok(sha256_hex(&body))
    }
}

/// HTTP over ureq, with nx's agent and retry policy.
#[derive(Clone, Default)]
pub struct Ureq(Client);

impl Ureq {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The most nx reads into memory from one answer.
const MAX_BODY: u64 = 1 << 30;

impl Http for Ureq {
    fn fetch(&self, url: &str) -> Result<(Vec<u8>, String)> {
        self.0
            .get(url, |mut r| {
                let at = r.get_uri().to_string();
                let body = r.body_mut().with_config().limit(MAX_BODY).read_to_vec()?;
                Ok((body, at))
            })
            .map_err(|e| Error::from_ureq(url, &e))
    }

    fn reader(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        let r = self.0.get(url, Ok).map_err(|e| Error::from_ureq(url, &e))?;
        Ok(Box::new(r.into_body().into_reader()))
    }

    fn download(&self, url: &str, to: &Path) -> Result<String> {
        // Each attempt starts the file over.
        self.0
            .get(url, |r| {
                let file = std::fs::File::create(to)?;
                let mut out = Hashing::new(std::io::BufWriter::new(file));
                std::io::copy(&mut r.into_body().into_reader(), &mut out)?;
                Ok(out.finish()?)
            })
            .map_err(|e| Error::from_ureq(url, &e))
    }
}

/// SHA-256 as hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A writer that hashes what goes through it.
pub struct Hashing<W> {
    inner: W,
    sha: Sha256,
}

impl<W: Write> Hashing<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            sha: Sha256::new(),
        }
    }

    /// Flushes and returns the SHA-256 of everything written, as hex.
    pub fn finish(mut self) -> std::io::Result<String> {
        self.inner.flush()?;
        Ok(hex(&self.sha.finalize()))
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sha.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Canned answers by URL, for tests in any crate: a URL it lacks is a 404.
/// Records what was asked.
#[derive(Default)]
pub struct Canned {
    pub bodies: std::collections::HashMap<String, Vec<u8>>,
    /// URL → where it redirects to (the channel's answer).
    pub redirects: std::collections::HashMap<String, String>,
    hits: std::sync::Mutex<Vec<String>>,
}

impl Canned {
    pub fn new(bodies: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        Self {
            bodies: bodies.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Every URL asked for, in order.
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().map(|h| h.clone()).unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut h) = self.hits.lock() {
            h.clear();
        }
    }
}

impl Http for Canned {
    fn fetch(&self, url: &str) -> Result<(Vec<u8>, String)> {
        if let Ok(mut h) = self.hits.lock() {
            h.push(url.to_owned());
        }
        let at = self.redirects.get(url).map_or(url, String::as_str);
        self.bodies
            .get(at)
            .map(|b| (b.clone(), at.to_owned()))
            .ok_or_else(|| Error::status(url, 404))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canned_answers_follow_redirects_and_404_the_rest() -> Result<()> {
        let mut http = Canned::new([("https://b/x".to_owned(), b"body".to_vec())]);
        http.redirects
            .insert("https://a/x".into(), "https://b/x".into());
        assert_eq!(
            http.fetch("https://a/x")?,
            (b"body".to_vec(), "https://b/x".into())
        );
        let missing = http.get("https://c").err();
        assert!(missing.is_some_and(|e| e.not_found()));
        assert_eq!(http.hits(), ["https://a/x", "https://c"]);
        Ok(())
    }

    #[test]
    fn hashes_what_it_writes() -> std::io::Result<()> {
        let mut h = Hashing::new(Vec::new());
        h.write_all(b"abc")?;
        assert_eq!(
            h.finish()?,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        Ok(())
    }
}
