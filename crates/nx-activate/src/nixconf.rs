//! Extra Nix binary caches (design, *Unfree and uncached packages*). The
//! daemon trusts a substituter for an unprivileged client only when its
//! own configuration lists it in `trusted-substituters`, and it reads that
//! only when it starts. So nxd, as root, writes the current generation's
//! `[[cache]]` entries into `/etc/nix/nx.conf`, makes sure `nix.conf`
//! includes it, and restarts `nix-daemon` when it changed (11 ms;
//! `KillMode=process` leaves running builds alone). The CLI then passes
//! `--option extra-substituters` on its own calls.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use nx_core::CacheEntry;

use crate::{Runner, Services};

/// The file nxd owns, beside `nix.conf`.
pub const FILE: &str = "nx.conf";
const INCLUDE: &str = "!include nx.conf";

/// What `nx.conf` says for `caches`; empty for none.
pub fn render(caches: &[CacheEntry]) -> String {
    if caches.is_empty() {
        return String::new();
    }
    let urls: Vec<&str> = caches.iter().map(|c| c.url.as_str()).collect();
    let keys: Vec<&str> = caches.iter().map(|c| c.key.as_str()).collect();
    format!(
        "# Written by nxd from the current nx generation's [[cache]] entries; nx cache changes it.\n\
         extra-trusted-substituters = {}\n\
         extra-trusted-public-keys = {}\n",
        urls.join(" "),
        keys.join(" ")
    )
}

/// Brings `dir/nx.conf` in line with `caches` and restarts the daemon if
/// it changed. Problems are warnings: the generation is live either way,
/// only the extra caches aren't.
pub fn apply(dir: &Path, runner: &dyn Runner, caches: &[CacheEntry], services: &mut Services) {
    let (ok, bad): (Vec<CacheEntry>, Vec<CacheEntry>) =
        caches.iter().cloned().partition(CacheEntry::acceptable);
    for c in &bad {
        services.warnings.push(format!(
            "skipped the extra cache {:?}: not an https URL with a name:key",
            c.url
        ));
    }
    let file = dir.join(FILE);
    let want = render(&ok);
    let have = std::fs::read_to_string(&file).unwrap_or_default();
    if want == have {
        return;
    }
    if let Err(e) = write(dir, &want).and_then(|()| include(dir)) {
        services
            .warnings
            .push(format!("the extra Nix caches were not set up: {e}"));
        return;
    }
    let restarted = runner.run(
        "systemctl",
        &["try-restart".as_ref(), "nix-daemon.service".as_ref()],
    );
    match restarted {
        Ok(out) if out.success => services.events.push(if ok.is_empty() {
            "restarted nix-daemon without extra caches".to_owned()
        } else {
            format!(
                "restarted nix-daemon to trust {}",
                ok.iter()
                    .map(|c| c.url.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
        Ok(out) => services.warnings.push(format!(
            "restarting nix-daemon failed: {}",
            out.stderr.trim()
        )),
        Err(e) => services
            .warnings
            .push(format!("restarting nix-daemon failed: {e}")),
    }
}

/// Replaces `nx.conf`, or removes it for no caches.
fn write(dir: &Path, text: &str) -> std::io::Result<()> {
    let file = dir.join(FILE);
    if text.is_empty() {
        return match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    replace(&file, text, 0o644)
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

/// Makes `nix.conf` include `nx.conf`, once. A missing include is no error
/// for Nix (`!include`), so the line can stay when there are no caches.
/// The file is replaced, not appended to: nxd runs as root and never writes
/// through a link.
fn include(dir: &Path) -> std::io::Result<()> {
    let conf = dir.join("nix.conf");
    let meta = std::fs::symlink_metadata(&conf)?;
    if !meta.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a plain file",
            conf.display()
        )));
    }
    let text = std::fs::read_to_string(&conf)?;
    if text.lines().any(|l| l.trim() == INCLUDE) {
        return Ok(());
    }
    let sep = if text.ends_with('\n') || text.is_empty() {
        ""
    } else {
        "\n"
    };
    let new = format!("{text}{sep}\n# Extra caches nx trusts (nx cache add).\n{INCLUDE}\n");
    replace(&conf, &new, meta.permissions().mode() & 0o7777)
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

    fn cache(url: &str, key: &str) -> CacheEntry {
        CacheEntry {
            url: url.into(),
            key: key.into(),
        }
    }

    #[test]
    fn writes_includes_and_restarts_only_on_change() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-nixconf-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("nix.conf"), "max-jobs = auto")?;
        let runner = Calls(RefCell::default());
        let caches = [cache(
            "https://nix-community.cachix.org",
            "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs=",
        )];
        let mut s = Services::default();
        apply(&dir, &runner, &caches, &mut s);
        let conf = std::fs::read_to_string(dir.join(FILE))?;
        assert!(conf.contains("extra-trusted-substituters = https://nix-community.cachix.org\n"));
        assert!(conf.contains("extra-trusted-public-keys = nix-community.cachix.org-1:"));
        let main = std::fs::read_to_string(dir.join("nix.conf"))?;
        assert!(main.starts_with("max-jobs = auto\n") && main.ends_with("!include nx.conf\n"));
        assert_eq!(
            *runner.0.borrow(),
            ["systemctl try-restart nix-daemon.service"]
        );
        assert_eq!(
            s.events,
            ["restarted nix-daemon to trust https://nix-community.cachix.org"]
        );
        // The same again: nothing to do.
        apply(&dir, &runner, &caches, &mut s);
        assert_eq!(runner.0.borrow().len(), 1);
        assert_eq!(std::fs::read_to_string(dir.join("nix.conf"))?, main);
        // None left: the file goes, the daemon restarts once more.
        apply(&dir, &runner, &[], &mut s);
        assert!(!dir.join(FILE).exists());
        assert_eq!(runner.0.borrow().len(), 2);
        std::fs::remove_dir_all(&dir)
    }

    #[test]
    fn never_writes_through_a_link() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-nixconf-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let target = dir.join("elsewhere");
        std::fs::write(&target, "untouched\n")?;
        std::os::unix::fs::symlink(&target, dir.join("nix.conf"))?;
        assert!(include(&dir).is_err());
        assert_eq!(std::fs::read_to_string(&target)?, "untouched\n");
        std::fs::remove_dir_all(&dir)
    }

    #[test]
    fn refuses_what_could_add_a_line() {
        assert!(!cache("https://x.org\ntrusted-users = *", "a:b").acceptable());
        assert!(!cache("http://x.org", "a:b").acceptable());
        assert!(!cache("https://x.org", "nokey").acceptable());
        assert!(cache("https://x.org", "x.org-1:abc=").acceptable());
    }
}
