//! One connection: read a request, authorize it, run it, answer.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use nx_proto::{Request, Response};

use crate::{ACTION_ACTIVATE, Context, Peer};

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
        Err(message) => Response::Failed { message },
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

fn respond(stream: &UnixStream, ctx: &Context) -> Result<Response, String> {
    let peer = peer(stream).map_err(|e| format!("can't identify the caller: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let request = nx_proto::read_request(stream).map_err(|e| e.to_string())?;
    eprintln!("nxd: uid {} pid {}: {request:?}", peer.uid, peer.pid);
    match request {
        Request::Activate {
            generation,
            backend,
        } => {
            ctx.authorizer.check(peer, ACTION_ACTIVATE)?;
            let _guard = lock(&ctx.lock)?;
            // Root, the owner of the state directory (nx) and the caller
            // may own what they hand over.
            let nx = std::fs::metadata(ctx.layout.state.root())
                .map_err(|e| format!("can't read the state directory: {e}"))?
                .uid();
            let done = nx_activate::activate(
                &ctx.layout,
                &*ctx.runner,
                generation,
                backend,
                &[nx, peer.uid],
            )
            .map_err(|e| e.to_string())?;
            Ok(Response::Activated {
                backend: done.backend,
                fallback: done.fallback,
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

/// Takes the exclusive lock on `path`; dropped with the file.
fn lock(path: &Path) -> Result<File, String> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    file.lock()
        .map_err(|e| format!("locking {}: {e}", path.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use nx_activate::{Layout, Output, Runner};
    use nx_core::generation::EXTENSION_RELEASE;
    use nx_core::{Backend, Lock, Manifest, State};

    use super::*;
    use crate::Authorizer;

    struct Allow(bool);
    impl Authorizer for Allow {
        fn check(&self, _: Peer, action: &str) -> Result<(), String> {
            assert_eq!(action, ACTION_ACTIVATE);
            if self.0 {
                Ok(())
            } else {
                Err("not authorized: no".into())
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
            root,
            file_contexts: None,
        };
        Ok((tmp, layout))
    }

    /// Serves one connection on a temp socket while `client` talks to it.
    fn exchange<T>(
        tag: &str,
        allow: bool,
        client: impl FnOnce(&Path) -> T,
    ) -> std::io::Result<(T, Layout)> {
        let (tmp, layout) = fixture(tag)?;
        let socket = tmp.0.join("nxd.sock");
        let listener = bind(&socket)?;
        let served = layout.clone();
        let lock = tmp.0.join("nxd.lock");
        let result = std::thread::scope(|s| {
            let server = s.spawn(move || {
                let ctx = Context {
                    layout: served,
                    runner: Box::new(Merges),
                    authorizer: Box::new(Allow(allow)),
                    lock,
                };
                if let Ok((stream, _)) = listener.accept() {
                    handle(stream, &ctx);
                }
            });
            let out = client(&socket);
            let _ = server.join();
            out
        });
        drop(tmp);
        Ok((result, layout))
    }

    /// Asks to activate generation 1; returns the answer and whether
    /// `current` exists afterwards (read while the fixture is still there).
    fn activate(tag: &str, allow: bool) -> std::io::Result<(nx_proto::Result<Response>, bool)> {
        let (out, _) = exchange(tag, allow, |socket| {
            let response = nx_proto::call(
                socket,
                &Request::Activate {
                    generation: 1,
                    backend: None,
                },
            );
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
            matches!(
                response,
                Ok(Response::Activated {
                    backend: Backend::Sysext,
                    fallback: None
                })
            ),
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

    #[test]
    fn garbage_is_answered_not_obeyed() -> std::io::Result<()> {
        let (reply, _) = exchange("garbage", true, |socket| {
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
