//! A tool-tier command (design, *Exposing binaries*). Every exposed name in
//! a generation's `bin/` is a hard link to this one binary; what it runs is
//! in `shims/<name>` beside `bin/`, one store path per file. It finds its
//! own name from `/proc/self/exe`, which is the path it was started through
//! (`…/generations/7/bin/rg`), whatever `argv[0]` says.
//!
//! It sets one variable, `TERMINFO_DIRS`: Nix's ncurses doesn't know
//! terminals only the host has (`xterm-ghostty`). Nothing else: Nix's glibc
//! and OpenSSL already find the host's locale archive and certificates.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// The host's terminfo, searched after the caller's own.
const HOST_TERMINFO: &str = "/usr/share/terminfo";

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    let exe = match std::fs::read_link("/proc/self/exe") {
        Ok(p) => p,
        Err(e) => return fail(&argv0, &format!("can't find itself: {e}")),
    };
    let target = match target(&exe) {
        Ok(t) => t,
        Err(e) => return fail(&argv0, &e),
    };
    let err = Command::new(&target)
        .arg0(&argv0)
        .args(args)
        .env(
            "TERMINFO_DIRS",
            terminfo_dirs(std::env::var_os("TERMINFO_DIRS")),
        )
        .exec();
    fail(&argv0, &format!("can't run {}: {err}", target.display()))
}

/// `…/bin/rg` → the store path in `…/shims/rg`.
fn target(exe: &Path) -> Result<PathBuf, String> {
    let name = exe.file_name().ok_or("has no name")?;
    let generation = exe
        .parent()
        .and_then(Path::parent)
        .ok_or("is not in a generation")?;
    let record = generation.join("shims").join(name);
    let text = std::fs::read(&record).map_err(|e| format!("{}: {e}", record.display()))?;
    let line = text.split(|&b| b == b'\n').next().unwrap_or_default();
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(line));
    if !path.starts_with("/nix/store/") {
        return Err(format!("{} doesn't name a store path", record.display()));
    }
    Ok(path)
}

/// The caller's directories, then the host's, then an empty entry: ncurses'
/// own default.
fn terminfo_dirs(caller: Option<OsString>) -> OsString {
    let mut dirs = OsString::new();
    if let Some(c) = caller.filter(|c| !c.is_empty()) {
        dirs.push(c);
        dirs.push(":");
    }
    dirs.push(HOST_TERMINFO);
    dirs.push(":");
    dirs
}

fn fail(argv0: &OsString, msg: &str) -> ExitCode {
    let name = Path::new(argv0).file_name().unwrap_or(argv0.as_os_str());
    eprintln!("nx: {}: {msg}", name.to_string_lossy());
    ExitCode::from(127)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_store_path_beside_bin() -> std::io::Result<()> {
        let g = std::env::temp_dir().join(format!("nx-shim-{}", std::process::id()));
        std::fs::create_dir_all(g.join("bin"))?;
        std::fs::create_dir_all(g.join("shims"))?;
        std::fs::write(
            g.join("shims/rg"),
            "/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0/bin/rg\n",
        )?;
        std::fs::write(g.join("shims/bad"), "/etc/passwd\n")?;
        assert_eq!(
            target(&g.join("bin/rg")),
            Ok(PathBuf::from(
                "/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0/bin/rg"
            ))
        );
        assert!(target(&g.join("bin/bad")).is_err());
        assert!(target(&g.join("bin/none")).is_err());
        std::fs::remove_dir_all(&g)
    }

    #[test]
    fn terminfo_keeps_the_callers_first() {
        assert_eq!(terminfo_dirs(None), "/usr/share/terminfo:");
        assert_eq!(
            terminfo_dirs(Some("/home/u/.terminfo".into())),
            "/home/u/.terminfo:/usr/share/terminfo:"
        );
    }
}
