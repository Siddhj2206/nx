//! HTTP for the tool tier: the channel, the package list and narinfos.

use std::io::Read;
use std::time::Duration;

use ureq::ResponseExt;

use crate::{Error, Result};

/// Attempts per request: one reset connection shouldn't fail a command.
const ATTEMPTS: u32 = 3;

/// HTTP over ureq, with the system's trust store.
#[derive(Clone)]
pub struct Web(ureq::Agent);

impl Default for Web {
    fn default() -> Self {
        Self::new()
    }
}

impl Web {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("nx/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        Self(config.into())
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
                Ok(r)
                    if attempt < ATTEMPTS
                        && (r.status().is_server_error() || r.status() == 429) => {}
                Ok(r) => return Err(fail(format!("HTTP {}", r.status()))),
                Err(e) if attempt < ATTEMPTS && transient(&e) => {}
                Err(e) => return Err(fail(e.to_string())),
            }
            std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
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

fn transient(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed
    )
}
