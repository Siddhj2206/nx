//! Fallback telemetry: every fallback nx takes writes one journal entry
//! with its own `MESSAGE_ID`, so `journalctl MESSAGE_ID=<id>` says how
//! often it runs (plan 1.1, 0.1). Entries go straight to journald's native
//! socket; writing is best effort and never fails a command.

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

/// journald's native protocol socket.
pub const SOCKET: &str = "/run/systemd/journal/socket";

/// A fallback nx took instead of the usual path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// An image relative's exact build came from Koji: no repo had it.
    Koji,
    /// The manifest didn't resolve as a whole, so each package was
    /// resolved alone and the ones that fail were left out.
    PartialResolve,
    /// A tool-tier package was built from its derivation: no binary cache
    /// had it.
    LocalBuild,
    /// Metadata came from a mirror after the first, or a package from a
    /// mirror other than the one its metadata did.
    OtherMirror,
    /// Every mirror of a repo failed, so its cached metadata was used.
    CachedMetadata,
    /// `systemd-sysext refresh` failed or merged nothing.
    MergeFailed,
}

impl Fallback {
    pub const ALL: [Self; 6] = [
        Self::Koji,
        Self::PartialResolve,
        Self::LocalBuild,
        Self::OtherMirror,
        Self::CachedMetadata,
        Self::MergeFailed,
    ];

    /// The `NX_FALLBACK` field and the entry's `MESSAGE_ID`. The ids are
    /// fixed: changing one loses its history.
    const fn fields(self) -> (&'static str, &'static str) {
        match self {
            Self::Koji => ("koji", "44f21298d15a4e8a92ad200912912dd7"),
            Self::PartialResolve => ("partial-resolve", "13b86f26dcdf40a68ab4002ee34da519"),
            Self::LocalBuild => ("local-build", "52cf727a75204b31aea91b293bded854"),
            Self::OtherMirror => ("other-mirror", "397e4099b3774e0c8aa3144b7392f21a"),
            Self::CachedMetadata => ("cached-metadata", "67553b5c473a4661952755ca9d659412"),
            Self::MergeFailed => ("merge-failed", "e7aa490fdaaf491a9c62c78cd738a59b"),
        }
    }

    /// The entry's `MESSAGE_ID`.
    pub const fn message_id(self) -> &'static str {
        self.fields().1
    }

    /// The `NX_FALLBACK` field, for reading without the ids.
    pub const fn name(self) -> &'static str {
        self.fields().0
    }
}

/// Where fallbacks are recorded: journald on a real system, nowhere (or a
/// test's socket) in tests.
#[derive(Debug, Clone)]
pub struct Journal {
    socket: Option<PathBuf>,
    identifier: &'static str,
}

impl Journal {
    /// journald, with `SYSLOG_IDENTIFIER=identifier` (`nx`, `nxd`).
    pub fn system(identifier: &'static str) -> Self {
        Self::at(Path::new(SOCKET), identifier)
    }

    /// A datagram socket at `path` that speaks journald's protocol.
    pub fn at(path: &Path, identifier: &'static str) -> Self {
        Self {
            socket: Some(path.to_owned()),
            identifier,
        }
    }

    /// Records nothing.
    pub fn off() -> Self {
        Self {
            socket: None,
            identifier: "nx",
        }
    }

    /// Records that `fallback` ran; `message` says what it was for. A
    /// journal that isn't there loses the entry, nothing more.
    pub fn fallback(&self, fallback: Fallback, message: &str) {
        let Some(path) = &self.socket else { return };
        let entry = entry(&[
            ("MESSAGE", message),
            ("MESSAGE_ID", fallback.message_id()),
            // LOG_NOTICE: normal, but worth counting.
            ("PRIORITY", "5"),
            ("SYSLOG_IDENTIFIER", self.identifier),
            ("NX_FALLBACK", fallback.name()),
        ]);
        if let Ok(socket) = UnixDatagram::unbound() {
            let _ = socket.send_to(&entry, path);
        }
    }
}

/// Longest message kept: one datagram, well under the socket's limit.
const MAX_VALUE: usize = 4096;

/// Fields in journald's native format: `KEY=value\n`, or for a value with
/// a newline `KEY\n`, its length as 64-bit little-endian, the value, `\n`.
pub fn entry(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in fields {
        let value = truncate(value, MAX_VALUE);
        out.extend_from_slice(key.as_bytes());
        if value.contains('\n') {
            out.push(b'\n');
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        } else {
            out.push(b'=');
        }
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_native_format() {
        let e = entry(&[("MESSAGE", "one line"), ("DETAIL", "two\nlines")]);
        let mut want = b"MESSAGE=one line\nDETAIL\n".to_vec();
        want.extend_from_slice(&9u64.to_le_bytes());
        want.extend_from_slice(b"two\nlines\n");
        assert_eq!(e, want);
    }

    #[test]
    fn message_ids_are_distinct_128_bit_ids() {
        let ids: std::collections::BTreeSet<_> =
            Fallback::ALL.iter().map(|f| f.message_id()).collect();
        assert_eq!(ids.len(), Fallback::ALL.len());
        for id in ids {
            assert_eq!(id.len(), 32, "{id}");
            assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        }
    }

    #[test]
    fn sends_one_datagram_per_fallback() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("socket");
        let _ = std::fs::remove_file(&path);
        let server = UnixDatagram::bind(&path)?;
        Journal::at(&path, "nx").fallback(Fallback::Koji, "libfoo 1.0-1.fc45 from Koji");
        let mut buf = [0u8; 1024];
        let n = server.recv(&mut buf)?;
        let got = String::from_utf8_lossy(&buf[..n]).into_owned();
        std::fs::remove_dir_all(&dir)?;
        assert!(
            got.contains("MESSAGE=libfoo 1.0-1.fc45 from Koji\n"),
            "{got}"
        );
        assert!(
            got.contains("MESSAGE_ID=44f21298d15a4e8a92ad200912912dd7\n"),
            "{got}"
        );
        assert!(got.contains("NX_FALLBACK=koji\n"), "{got}");
        assert!(got.contains("SYSLOG_IDENTIFIER=nx\n"), "{got}");
        Ok(())
    }

    #[test]
    fn a_missing_journal_is_not_an_error() {
        Journal::at(Path::new("/nonexistent/socket"), "nx").fallback(Fallback::Koji, "x");
        Journal::off().fallback(Fallback::Koji, "x");
    }

    #[test]
    fn long_messages_are_cut_on_a_char_boundary() {
        let s = "é".repeat(MAX_VALUE);
        let t = truncate(&s, MAX_VALUE + 1);
        assert!(t.len() <= MAX_VALUE + 1);
        assert!(s.starts_with(t));
    }
}
