//! The protocol between the unprivileged `nx` and the root `nxd`: one JSON
//! request per connection, one JSON response, each a single line.
//!
//! nxd never takes a path from the client. A request names a generation by
//! number, and nxd looks it up in its own state directory.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where nxd's socket is (`nxd.socket`).
pub const SOCKET: &str = "/run/nxd.sock";

/// The most a request or response may hold; both are a few dozen bytes.
const MAX_LINE: u64 = 64 << 10;

/// How long a client waits for nxd. A merge takes milliseconds; the polkit
/// prompt, which waits on a person, is the long part.
const IO_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Makes generation `generation` the current one and live.
    Activate { generation: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Activated {
        /// What was done to units, users and files ("started foo.service").
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        events: Vec<String>,
        /// Service steps that failed or were skipped; the merge stands.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Nothing listens on the socket: nxd isn't installed or its socket
    /// isn't enabled.
    #[error("can't reach nxd at {path}: {source}")]
    Unavailable {
        path: String,
        source: std::io::Error,
    },
    #[error("talking to nxd: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("message longer than {MAX_LINE} bytes, or cut short")]
    BadFraming,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Sends `request` to the nxd listening on `socket` and returns its answer.
pub fn call(socket: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).map_err(|source| Error::Unavailable {
        path: socket.display().to_string(),
        source,
    })?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    write_line(&mut stream, request)?;
    read_line(&mut BufReader::new(&stream))
}

/// Reads one request from a connection.
pub fn read_request(stream: impl Read) -> Result<Request> {
    read_line(&mut BufReader::new(stream))
}

/// Writes `message` and its newline.
pub fn write_line(out: &mut impl Write, message: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()?;
    Ok(())
}

fn read_line<T: for<'de> Deserialize<'de>>(input: &mut impl BufRead) -> Result<T> {
    let mut line = String::new();
    let n = input.take(MAX_LINE + 1).read_line(&mut line)?;
    if n == 0 || n as u64 > MAX_LINE || !line.ends_with('\n') {
        return Err(Error::BadFraming);
    }
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_one_line() -> Result<()> {
        let request = Request::Activate { generation: 7 };
        let mut wire = Vec::new();
        write_line(&mut wire, &request)?;
        assert_eq!(
            String::from_utf8_lossy(&wire),
            "{\"request\":\"activate\",\"generation\":7}\n"
        );
        assert_eq!(read_request(&wire[..])?, request);
        // nx 1.0 sent a backend; it is ignored.
        let old: Request = serde_json::from_str(
            "{\"request\":\"activate\",\"generation\":2,\"backend\":\"prefix\"}",
        )?;
        assert_eq!(old, Request::Activate { generation: 2 });
        Ok(())
    }

    #[test]
    fn refuses_unterminated_and_oversized_lines() {
        assert!(matches!(read_request(&b""[..]), Err(Error::BadFraming)));
        assert!(matches!(
            read_request(&b"{\"request\":\"activate\",\"generation\":1}"[..]),
            Err(Error::BadFraming)
        ));
        let mut big = vec![b'x'; MAX_LINE as usize + 10];
        big.push(b'\n');
        assert!(matches!(read_request(&big[..]), Err(Error::BadFraming)));
        assert!(matches!(
            read_request(&b"{\"request\":\"format_disk\"}\n"[..]),
            Err(Error::Json(_))
        ));
    }

    #[test]
    fn an_absent_socket_is_unavailable() {
        let r = call(
            Path::new("/nonexistent/nxd.sock"),
            &Request::Activate { generation: 1 },
        );
        assert!(matches!(r, Err(Error::Unavailable { .. })), "{r:?}");
    }
}
