//! One connection: read a request, authorize it, run it, answer.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use nx_core::Fallback;
use nx_proto::{Request, Response};

use crate::{ACTION_ACTIVATE, Context, Error, Peer, Result};

/// A client that connects and says nothing must not hold nxd forever.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Listens on `path` and handles connections one at a time, forever.
pub fn listen(path: &Path, ctx: &Context) -> std::io::Result<()> {
    let listener = bind(path)?;
    loop {
        let (stream, _) = listener.accept()?;
        handle(stream, ctx);
    }
}

/// Binds the socket, replacing a stale one. Anyone may connect: polkit
/// decides who may ask for what.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Answers one connection. Failures go to the client, and to the journal.
pub fn handle(stream: UnixStream, ctx: &Context) {
    let response = match respond(&stream, ctx) {
        Ok(r) => r,
        Err(e) => Response::Failed {
            message: e.to_string(),
        },
    };
    if let Response::Failed { message } = &response {
        eprintln!("nxd: refused or failed: {message}");
    }
    let mut out = &stream;
    if nx_proto::write_line(&mut out, &response).is_err() {
        eprintln!("nxd: the client went away before the answer");
    }
    let _ = out.flush();
}

fn respond(stream: &UnixStream, ctx: &Context) -> Result<Response> {
    let peer = peer(stream).map_err(Error::Peer)?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(Error::Peer)?;
    let request = nx_proto::read_request(stream)?;
    eprintln!("nxd: uid {} pid {}: {request:?}", peer.uid, peer.pid);
    match request {
        Request::Activate { generation } => {
            ctx.authorizer.check(peer, ACTION_ACTIVATE)?;
            let _guard = lock(&ctx.lock)?;
            // Root, the owner of the state directory (nx), the caller and
            // the other administrators may own what they hand over.
            let state = ctx.layout.state.root();
            let nx = std::fs::metadata(state)
                .map_err(|source| Error::Io {
                    path: state.display().to_string(),
                    source,
                })?
                .uid();
            let wheel = nx_activate::Wheel::new(&*ctx.runner, &ctx.layout.root);
            let allowed = |uid: u32| uid == nx || uid == peer.uid || wheel.is_member(uid);
            let layout = nx_activate::Layout {
                image: (ctx.image)(),
                ..ctx.layout.clone()
            };
            let done = nx_activate::activate(&layout, &*ctx.runner, generation, &allowed)
                .inspect_err(|e| {
                    if let nx_activate::Error::Command {
                        program: "systemd-sysext",
                        detail,
                    } = e
                    {
                        ctx.journal.fallback(
                            Fallback::MergeFailed,
                            &format!("generation {generation} didn't merge: {detail}"),
                        );
                    }
                })?;
            Ok(Response::Activated {
                events: done.services.events,
                warnings: done.services.warnings,
            })
        }
    }
}

fn peer(stream: &UnixStream) -> std::io::Result<Peer> {
    let cred = rustix::net::sockopt::socket_peercred(stream)?;
    Ok(Peer {
        pid: cred.pid.as_raw_nonzero().get(),
        uid: cred.uid.as_raw(),
    })
}

/// How long a request waits for another to finish before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(60);

/// Takes the exclusive lock on `path`, waiting up to [`LOCK_WAIT`]; it is
/// released when the file is dropped.
fn lock(path: &Path) -> Result<File> {
    let io = |source| Error::Io {
        path: path.display().to_string(),
        source,
    };
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(io)?;
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io(std::io::Error::other(
                    "another nxd request is still running",
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use nx_activate::{Layout, Output, Runner};
    use nx_core::generation::EXTENSION_RELEASE;
    use nx_core::{Lock, Manifest, State};

    use super::*;
    use crate::Authorizer;

    struct Allow(bool);
    impl Authorizer for Allow {
        fn check(&self, _: Peer, action: &str) -> Result<()> {
            assert_eq!(action, ACTION_ACTIVATE);
            if self.0 {
                Ok(())
            } else {
                Err(Error::Denied("not authorized: no".into()))
            }
        }
    }

    /// `systemd-sysext` that says nx is merged.
    struct Merges;
    impl Runner for Merges {
        fn run(&self, _: &str, args: &[&OsStr]) -> std::io::Result<Output> {
            let status = args.first().is_some_and(|a| *a == "status");
            Ok(Output {
                success: true,
                stdout: if status {
                    r#"[{"hierarchy":"/usr","extensions":["nx"]}]"#.into()
                } else {
                    String::new()
                },
                stderr: String::new(),
            })
        }
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(tag: &str) -> std::io::Result<(Tmp, Layout)> {
        let dir = std::env::temp_dir().join(format!("nxd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tmp = Tmp(dir.clone());
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("usr/lib"))?;
        std::fs::write(
            root.join("usr/lib/os-release"),
            "ID=fedora\nVERSION_ID=45\n",
        )?;
        let state = State::new(dir.join("state"));
        let gen_dir = state.root().join("generations/1");
        let release = gen_dir.join("sysext").join(EXTENSION_RELEASE);
        std::fs::create_dir_all(release.parent().unwrap_or(&gen_dir))?;
        std::fs::write(release, "ID=fedora\nVERSION_ID=45\n")?;
        std::fs::write(
            gen_dir.join("manifest.toml"),
            Manifest::default().to_toml().unwrap_or_default(),
        )?;
        std::fs::write(
            gen_dir.join("lock.toml"),
            Lock::new(None, "45", "x86_64")
                .to_toml()
                .unwrap_or_default(),
        )?;
        let layout = Layout {
            state,
            extensions: dir.join("extensions"),
            merged: dir.join("merged"),
            root,
            file_contexts: None,
            image: nx_activate::Image::None,
            nix_conf: None,
        };
        Ok((tmp, layout))
    }

    /// Serves one connection on a temp socket while `client` talks to it.
    fn exchange<T>(tag: &str, allow: bool, client: impl FnOnce(&Path) -> T) -> std::io::Result<T> {
        Ok(exchange_with(tag, allow, Merges, client)?.0)
    }

    /// [`exchange`] with `runner` as `systemd-sysext`; also returns what
    /// nxd wrote to the journal.
    fn exchange_with<T>(
        tag: &str,
        allow: bool,
        runner: impl Runner + Send + 'static,
        client: impl FnOnce(&Path) -> T,
    ) -> std::io::Result<(T, Vec<String>)> {
        let (tmp, layout) = fixture(tag)?;
        let socket = tmp.0.join("nxd.sock");
        let listener = bind(&socket)?;
        let lock = tmp.0.join("nxd.lock");
        let journal_path = tmp.0.join("journal.sock");
        let journal = std::os::unix::net::UnixDatagram::bind(&journal_path)?;
        journal.set_nonblocking(true)?;
        let result = std::thread::scope(|s| {
            let server = s.spawn(move || {
                let ctx = Context {
                    layout,
                    runner: Box::new(runner),
                    authorizer: Box::new(Allow(allow)),
                    image: Box::new(|| nx_activate::Image::None),
                    lock,
                    journal: nx_core::Journal::at(&journal_path, "nxd"),
                };
                if let Ok((stream, _)) = listener.accept() {
                    handle(stream, &ctx);
                }
            });
            let out = client(&socket);
            let _ = server.join();
            out
        });
        let mut entries = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = journal.recv(&mut buf) {
            entries.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        drop(tmp);
        Ok((result, entries))
    }

    /// Asks to activate generation 1; returns the answer and whether
    /// `current` exists afterwards (read while the fixture is still there).
    fn activate(tag: &str, allow: bool) -> std::io::Result<(nx_proto::Result<Response>, bool)> {
        let out = exchange(tag, allow, |socket| {
            let response = nx_proto::call(socket, &Request::Activate { generation: 1 });
            let current = socket
                .parent()
                .is_some_and(|d| d.join("state/current").exists());
            (response, current)
        })?;
        Ok(out)
    }

    #[test]
    fn an_authorized_activation_goes_through() -> std::io::Result<()> {
        let (response, current) = activate("allow", true)?;
        assert!(
            matches!(response, Ok(Response::Activated { .. })),
            "{response:?}"
        );
        assert!(current);
        Ok(())
    }

    #[test]
    fn an_unauthorized_one_changes_nothing() -> std::io::Result<()> {
        let (response, current) = activate("deny", false)?;
        assert!(
            matches!(&response, Ok(Response::Failed { message }) if message.contains("not authorized")),
            "{response:?}"
        );
        assert!(!current);
        Ok(())
    }

    /// `systemd-sysext` whose refresh fails.
    struct RefreshFails;
    impl Runner for RefreshFails {
        fn run(&self, _: &str, args: &[&OsStr]) -> std::io::Result<Output> {
            let refresh = args.first().is_some_and(|a| *a == "refresh");
            Ok(Output {
                success: !refresh,
                stdout: String::new(),
                stderr: if refresh {
                    "no space".into()
                } else {
                    String::new()
                },
            })
        }
    }

    #[test]
    fn a_failed_merge_is_recorded_in_the_journal() -> std::io::Result<()> {
        let (response, entries) = exchange_with("merge-fails", true, RefreshFails, |socket| {
            nx_proto::call(socket, &Request::Activate { generation: 1 })
        })?;
        assert!(
            matches!(&response, Ok(Response::Failed { .. })),
            "{response:?}"
        );
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(
            entries[0].contains("NX_FALLBACK=merge-failed\n"),
            "{entries:?}"
        );
        assert!(
            entries[0].contains("MESSAGE=generation 1 didn't merge"),
            "{entries:?}"
        );
        Ok(())
    }

    #[test]
    fn an_authorized_activation_records_nothing() -> std::io::Result<()> {
        let (_, entries) = exchange_with("quiet", true, Merges, |socket| {
            nx_proto::call(socket, &Request::Activate { generation: 1 })
        })?;
        assert!(entries.is_empty(), "{entries:?}");
        Ok(())
    }

    #[test]
    fn garbage_is_answered_not_obeyed() -> std::io::Result<()> {
        let reply = exchange("garbage", true, |socket| {
            use std::io::{BufRead, BufReader};
            let mut stream = UnixStream::connect(socket)?;
            stream.write_all(b"{\"request\":\"rm_rf\"}\n")?;
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line)?;
            Ok::<_, std::io::Error>(line)
        })?;
        let reply = reply?;
        assert!(reply.contains("\"response\":\"failed\""), "{reply}");
        Ok(())
    }
}
