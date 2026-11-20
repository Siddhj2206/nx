//! What nx 1.0 left in Nix's configuration. 1.0 listed a generation's
//! extra binary caches (`[[cache]]`) in `/etc/nix/nx.conf`, included from
//! `nix.conf`, and restarted `nix-daemon` to trust them. 1.1 dropped extra
//! caches, so nxd removes both on activation and restarts the daemon once
//! if it changed anything. Goes in 1.2.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::{Runner, Services};

/// The file nx 1.0's nxd wrote, beside `nix.conf`.
pub const FILE: &str = "nx.conf";
const INCLUDE: &str = "!include nx.conf";
const COMMENT: &str = "# Extra caches nx trusts (nx cache add).";

/// Removes `dir/nx.conf` and `nix.conf`'s include of it, and restarts the
/// daemon if either was there. Problems are warnings: the generation is
/// live either way.
pub fn clean(dir: &Path, runner: &dyn Runner, services: &mut Services) {
    let removed = match remove(dir).and_then(|file| Ok(uninclude(dir)? || file)) {
        Ok(changed) => changed,
        Err(e) => {
            services
                .warnings
                .push(format!("removing nx 1.0's extra Nix caches failed: {e}"));
            return;
        }
    };
    if !removed {
        return;
    }
    let restarted = runner.run(
        "systemctl",
        &["try-restart".as_ref(), "nix-daemon.service".as_ref()],
    );
    match restarted {
        Ok(out) if out.success => services
            .events
            .push("restarted nix-daemon without nx 1.0's extra caches".to_owned()),
        Ok(out) => services.warnings.push(format!(
            "restarting nix-daemon failed: {}",
            out.stderr.trim()
        )),
        Err(e) => services
            .warnings
            .push(format!("restarting nix-daemon failed: {e}")),
    }
}

/// Removes `nx.conf`; whether it was there.
fn remove(dir: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(dir.join(FILE)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Takes the include (and the comment nx put above it) out of `nix.conf`;
/// whether it was there. The file is replaced, not edited in place: nxd
/// runs as root and never writes through a link.
fn uninclude(dir: &Path) -> std::io::Result<bool> {
    let conf = dir.join("nix.conf");
    let meta = match std::fs::symlink_metadata(&conf) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let text = std::fs::read_to_string(&conf)?;
    if !text.lines().any(|l| l.trim() == INCLUDE) {
        return Ok(false);
    }
    if !meta.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a plain file",
            conf.display()
        )));
    }
    let mut new = text
        .lines()
        .filter(|l| !matches!(l.trim(), INCLUDE | COMMENT))
        .collect::<Vec<_>>()
        .join("\n");
    // 1.0 put a blank line before its comment.
    new.truncate(new.trim_end().len());
    if !new.is_empty() {
        new.push('\n');
    }
    replace(&conf, &new, meta.permissions().mode() & 0o7777)?;
    Ok(true)
}

/// Writes `text` to a fresh name beside `file` (`create_new` never follows
/// a planted link), flushes it, and renames it over `file`, which replaces
/// a link rather than writing through it.
fn replace(file: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let tmp = file.with_file_name(format!(".{name}.nx.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.sync_all()?;
    std::fs::rename(&tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Output;
    use std::cell::RefCell;
    use std::ffi::OsStr;

    struct Calls(RefCell<Vec<String>>);
    impl Runner for Calls {
        fn run(&self, program: &str, args: &[&OsStr]) -> std::io::Result<Output> {
            let args: Vec<String> = args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            self.0
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            Ok(Output {
                success: true,
                ..Output::default()
            })
        }
    }

    fn dir(tag: &str) -> std::io::Result<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("nx-nixconf-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn removes_what_1_0_wrote_and_restarts_once() -> std::io::Result<()> {
        let dir = dir("clean")?;
        // What nx 1.0's nxd left after `nx cache add`.
        std::fs::write(
            dir.join("nix.conf"),
            "max-jobs = auto\n\n# Extra caches nx trusts (nx cache add).\n!include nx.conf\n",
        )?;
        std::fs::write(
            dir.join(FILE),
            "extra-trusted-substituters = https://nix-community.cachix.org\n",
        )?;
        let runner = Calls(RefCell::default());
        let mut s = Services::default();
        clean(&dir, &runner, &mut s);
        assert!(!dir.join(FILE).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("nix.conf"))?,
            "max-jobs = auto\n"
        );
        assert_eq!(
            *runner.0.borrow(),
            ["systemctl try-restart nix-daemon.service"]
        );
        assert_eq!(
            s.events,
            ["restarted nix-daemon without nx 1.0's extra caches"]
        );
        // Nothing left: nothing to do.
        clean(&dir, &runner, &mut s);
        assert_eq!(runner.0.borrow().len(), 1);
        std::fs::remove_dir_all(&dir)
    }

    #[test]
    fn never_writes_through_a_link() -> std::io::Result<()> {
        let dir = dir("link")?;
        let target = dir.join("elsewhere");
        std::fs::write(&target, "a = b\n!include nx.conf\n")?;
        std::os::unix::fs::symlink(&target, dir.join("nix.conf"))?;
        assert!(uninclude(&dir).is_err());
        assert_eq!(
            std::fs::read_to_string(&target)?,
            "a = b\n!include nx.conf\n"
        );
        std::fs::remove_dir_all(&dir)
    }
}
