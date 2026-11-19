//! HTTP for the tool tier: the channel, the package list and narinfos.

use std::io::Read;

use nx_http::{ATTEMPTS, ureq};
use ureq::ResponseExt;

use crate::{Error, Result};

/// HTTP over ureq, with nx's agent and retry policy ([`nx_http`]).
#[derive(Clone)]
pub struct Web(ureq::Agent);

impl Default for Web {
    fn default() -> Self {
        Self::new()
    }
}

impl Web {
    pub fn new() -> Self {
        Self(nx_http::agent(false))
    }

    /// GETs `url`, retrying a transient failure. `Ok(None)` for a 404 or
    /// 403 (a binary cache's "don't have it").
    fn call(&self, url: &str) -> Result<Option<ureq::http::Response<ureq::Body>>> {
        let fail = |reason: String| Error::Fetch {
            url: url.to_owned(),
            reason,
        };
        let mut attempt = 1;
        loop {
            match self.0.get(url).call() {
                Ok(r) if r.status().is_success() => return Ok(Some(r)),
                Ok(r) if matches!(r.status().as_u16(), 403 | 404) => return Ok(None),
                Ok(r) if attempt < ATTEMPTS && nx_http::transient_status(r.status().as_u16()) => {}
                Ok(r) => return Err(fail(format!("HTTP {}", r.status()))),
                Err(e) if attempt < ATTEMPTS && nx_http::transient(&e) => {}
                Err(e) => return Err(fail(e.to_string())),
            }
            nx_http::back_off(attempt);
            attempt += 1;
        }
    }

    /// The body as text, and the URL it came from after redirects.
    pub fn text(&self, url: &str) -> Result<Option<(String, String)>> {
        let Some(mut r) = self.call(url)? else {
            return Ok(None);
        };
        let at = r.get_uri().to_string();
        let body = r
            .body_mut()
            .with_config()
            .limit(16 << 20)
            .read_to_string()
            .map_err(|e| Error::Fetch {
                url: url.to_owned(),
                reason: e.to_string(),
            })?;
        Ok(Some((body, at)))
    }

    /// The body as a stream, for large files.
    pub fn reader(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        let r = self.call(url)?.ok_or_else(|| Error::Fetch {
            url: url.to_owned(),
            reason: "not found".to_owned(),
        })?;
        Ok(Box::new(r.into_body().into_reader()))
    }
}
