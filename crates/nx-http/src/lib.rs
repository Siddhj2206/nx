//! How nx talks HTTP, in one place for both tiers: one agent setup (the
//! system's trust store, so a CA the image or the user adds applies to nx
//! too) and one retry policy. Mirrors and binary caches reset connections
//! now and then, and one reset shouldn't fail a command.

use std::time::Duration;

pub use ureq;

/// Attempts per request.
pub const ATTEMPTS: u32 = 3;

/// The agent every nx download uses. `status_as_error`: whether a non-2xx
/// answer is an `Err` (ureq's default) or a response to look at (a binary
/// cache's 404 means "don't have it").
pub fn agent(status_as_error: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(concat!("nx/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(10)))
        // Per request; a large RPM or package list on a slow link.
        .timeout_global(Some(Duration::from_secs(600)))
        .http_status_as_error(status_as_error)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

/// Worth another try: the network, not the answer, failed.
pub fn transient(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::StatusCode(code) => transient_status(*code),
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed => true,
        _ => false,
    }
}

/// A server error or "slow down".
pub fn transient_status(code: u16) -> bool {
    code >= 500 || code == 429
}

/// Waits before attempt `attempt + 1`.
pub fn back_off(attempt: u32) {
    std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_what_the_network_failed() {
        assert!(transient(&ureq::Error::StatusCode(503)));
        assert!(transient(&ureq::Error::StatusCode(429)));
        assert!(!transient(&ureq::Error::StatusCode(404)));
        assert!(transient(&ureq::Error::ConnectionFailed));
    }
}
