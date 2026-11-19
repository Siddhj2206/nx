//! HTTP for the tool tier: the channel, the package list and narinfos.

use std::io::Read;

use nx_http::ureq;
use ureq::ResponseExt;

use crate::{Error, Result};

/// HTTP over ureq, with nx's agent and retry policy ([`nx_http`]).
#[derive(Clone, Default)]
pub struct Web(nx_http::Client);

impl Web {
    pub fn new() -> Self {
        Self::default()
    }

    /// GETs `url` and reads the answer with `read`. `Ok(None)` for a 404
    /// or 403 (a binary cache's "don't have it").
    fn call<T>(
        &self,
        url: &str,
        read: impl FnMut(ureq::http::Response<ureq::Body>) -> std::result::Result<T, ureq::Error>,
    ) -> Result<Option<T>> {
        match self.0.get(url, read) {
            Ok(t) => Ok(Some(t)),
            Err(ureq::Error::StatusCode(403 | 404)) => Ok(None),
            Err(e) => Err(Error::Fetch {
                url: url.to_owned(),
                reason: e.to_string(),
            }),
        }
    }

    /// The body as text, and the URL it came from after redirects.
    pub fn text(&self, url: &str) -> Result<Option<(String, String)>> {
        self.call(url, |mut r| {
            let at = r.get_uri().to_string();
            let body = r
                .body_mut()
                .with_config()
                .limit(16 << 20)
                .read_to_string()?;
            Ok((body, at))
        })
    }

    /// The body as a stream, for large files.
    pub fn reader(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        let r = self.call(url, Ok)?.ok_or_else(|| Error::Fetch {
            url: url.to_owned(),
            reason: "not found".to_owned(),
        })?;
        Ok(Box::new(r.into_body().into_reader()))
    }
}
