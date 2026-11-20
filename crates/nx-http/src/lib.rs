//! How nx talks HTTP, in one place for both tiers: one agent setup (the
//! system's trust store, so a CA the image or the user adds applies to nx
//! too) and one retry policy. Mirrors and binary caches reset connections
//! now and then, and one reset shouldn't fail a command.

use std::time::Duration;

mod http;
pub use http::{Canned, Error as FetchError, Hashing, Http, Ureq, sha256_hex};
pub use ureq;
use ureq::http::Response;
use ureq::{Body, Error};

/// Attempts per request.
pub const ATTEMPTS: u32 = 3;

/// The longest a server's `Retry-After` makes nx wait.
const MAX_WAIT: Duration = Duration::from_secs(30);

/// The agent every nx download uses.
#[derive(Clone)]
pub struct Client(ureq::Agent);

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self(
            ureq::Agent::config_builder()
                .user_agent(concat!("nx/", env!("CARGO_PKG_VERSION")))
                .timeout_connect(Some(Duration::from_secs(10)))
                // A server that stops sending fails the attempt, however
                // long the whole download (a large RPM on a slow link) takes.
                .timeout_per_read(Some(Duration::from_secs(30)))
                .timeout_global(Some(Duration::from_secs(600)))
                // Answers come back to `get`, which reads `Retry-After`.
                .http_status_as_error(false)
                .tls_config(
                    ureq::tls::TlsConfig::builder()
                        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                        .build(),
                )
                .build()
                .into(),
        )
    }

    /// GETs `url` and hands a 2xx answer to `read`, trying again when the
    /// request or `read` fails for a transient reason. Any other answer is
    /// `Error::StatusCode`.
    pub fn get<T>(
        &self,
        url: &str,
        mut read: impl FnMut(Response<Body>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut attempt = 1;
        loop {
            let (result, asked) = match self.0.get(url).call() {
                Ok(r) if r.status().is_success() => (read(r), None),
                Ok(r) => (Err(Error::StatusCode(r.status().as_u16())), retry_after(&r)),
                Err(e) => (Err(e), None),
            };
            match result {
                Err(e) if attempt < ATTEMPTS && transient(&e) => {
                    std::thread::sleep(asked.unwrap_or_else(|| back_off(attempt)));
                    attempt += 1;
                }
                done => return done,
            }
        }
    }
}

/// Worth another try: the network, not the answer, failed.
fn transient(e: &Error) -> bool {
    match e {
        Error::StatusCode(code) => *code >= 500 || *code == 429,
        Error::Io(_) | Error::Timeout(_) | Error::ConnectionFailed => true,
        _ => false,
    }
}

/// How long to wait before attempt `attempt + 1`.
fn back_off(attempt: u32) -> Duration {
    Duration::from_millis(500 * u64::from(attempt))
}

/// A `Retry-After` in seconds, capped; an HTTP date is ignored.
fn retry_after<B>(r: &Response<B>) -> Option<Duration> {
    let secs: u64 = r
        .headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs).min(MAX_WAIT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_what_the_network_failed() {
        assert!(transient(&Error::StatusCode(503)));
        assert!(transient(&Error::StatusCode(429)));
        assert!(!transient(&Error::StatusCode(404)));
        assert!(transient(&Error::ConnectionFailed));
    }

    #[test]
    fn honours_retry_after_up_to_a_cap() {
        let answer = |v: &str| {
            Response::builder()
                .status(503)
                .header("retry-after", v)
                .body(())
                .map(|r| retry_after(&r))
        };
        assert_eq!(answer("2").ok().flatten(), Some(Duration::from_secs(2)));
        assert_eq!(answer("3600").ok().flatten(), Some(MAX_WAIT));
        assert_eq!(answer("Wed, 21 Oct 2015 07:28:00 GMT").ok().flatten(), None);
    }
}
