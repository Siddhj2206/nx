//! Running `nix`, which talks to the daemon: the only writer of the store
//! (ADR-0001). Fedora's nix ships only the `nix` command, so realising is
//! `nix build`, never `nix-store`.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

use crate::{Error, Result, log};

/// Runs `nix` with arguments. `on_line` gets each line of its stderr as it
/// arrives (the internal-json log, when asked for); the result is its
/// stdout. A failure carries Nix's own error, condensed.
pub trait Nix {
    fn run(&self, args: &[String], on_line: &mut dyn FnMut(&str)) -> Result<String>;
}

/// The real `nix` on `PATH` (`NX_NIX` names another, for tests in a VM).
pub struct System;

impl Nix for System {
    fn run(&self, args: &[String], on_line: &mut dyn FnMut(&str)) -> Result<String> {
        let program = std::env::var_os("NX_NIX")
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "nix".into());
        let mut child = Command::new(&program)
            .args(args)
            // Nix's own colour codes would end up in nx's lines.
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(Error::Spawn)?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Nix("no stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Nix("no stderr".into()))?;
        let reader = std::thread::spawn(move || {
            let mut out = String::new();
            stdout.read_to_string(&mut out).map(|_| out)
        });
        let mut lines = Vec::new();
        for line in BufReader::new(stderr).lines() {
            let line = line.map_err(Error::Spawn)?;
            on_line(&line);
            lines.push(line);
        }
        let status = child.wait().map_err(Error::Spawn)?;
        let out = reader
            .join()
            .map_err(|_| Error::Nix("reading nix's output failed".into()))?
            .map_err(Error::Spawn)?;
        if !status.success() {
            return Err(Error::Nix(log::error_text(&lines)));
        }
        Ok(out)
    }
}

/// Realises `installables` (store paths, or `<drv>^<outputs>` for what no
/// cache has) through the daemon, handing each note of its log to
/// `on_note`. Links nothing: the generation's roots come after.
pub fn realise(
    nix: &dyn Nix,
    installables: &[String],
    on_note: &mut dyn FnMut(log::Note),
) -> Result<()> {
    if installables.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = ["build", "--no-link", "--log-format", "internal-json", "-v"]
        .map(String::from)
        .to_vec();
    args.extend(installables.iter().cloned());
    let mut tracker = log::Tracker::new();
    nix.run(&args, &mut |line| {
        for note in tracker.line(line) {
            on_note(note);
        }
    })
    .map(|_| ())
}

/// Registers `paths` as GC roots at `link`, `link-1`, `link-2`… (Nix's
/// naming for several out-links). The daemon records each link's absolute
/// path under `/nix/var/nix/gcroots/auto`, so `link` must be where it stays:
/// a generation's final directory, never its `.part`.
pub fn add_roots(nix: &dyn Nix, paths: &[String], link: &std::path::Path) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec![
        "build".to_owned(),
        "--out-link".to_owned(),
        link.display().to_string(),
    ];
    args.extend(paths.iter().cloned());
    nix.run(&args, &mut |_| {}).map(|_| ())
}

/// `nix store gc`: deletes every store path no root holds. Returns Nix's
/// summary line ("1234 store paths deleted, 1.2 GiB freed"), which Nix
/// 2.35 prints on stdout.
pub fn collect_garbage(nix: &dyn Nix) -> Result<String> {
    let mut last = String::new();
    let out = nix.run(&["store".to_owned(), "gc".to_owned()], &mut |line| {
        if line.contains("freed") {
            last = crate::strip_ansi(line.trim());
        }
    })?;
    if let Some(line) = out.lines().rev().find(|l| l.contains("freed")) {
        last = crate::strip_ansi(line.trim());
    }
    Ok(last)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Answers every call with the same stdout, after feeding `log` to
    /// `on_line`, and records the arguments.
    pub struct Fake {
        answer: String,
        log: Vec<String>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Fake {
        pub fn answering(answer: &str) -> Self {
            Self {
                answer: answer.to_owned(),
                log: Vec::new(),
                calls: RefCell::default(),
            }
        }

        pub fn logging(log: &str) -> Self {
            Self {
                answer: String::new(),
                log: log.lines().map(str::to_owned).collect(),
                calls: RefCell::default(),
            }
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl Nix for Fake {
        fn run(&self, args: &[String], on_line: &mut dyn FnMut(&str)) -> Result<String> {
            self.calls.borrow_mut().push(args.to_vec());
            for l in &self.log {
                on_line(l);
            }
            Ok(self.answer.clone())
        }
    }

    #[test]
    fn realises_and_reports_fetches() -> Result<()> {
        let nix = Fake::logging(include_str!("../../../testdata/nix-build-storepath.jsonl"));
        let mut notes = Vec::new();
        let path = "/nix/store/kzg6alm5cpb735v0xxchyihhbfbg7ffh-hyperfine-2.0.0".to_owned();
        realise(&nix, std::slice::from_ref(&path), &mut |n| notes.push(n))?;
        assert_eq!(
            notes,
            [log::Note::Fetched {
                name: "hyperfine-2.0.0".into(),
                size: 720_696
            }]
        );
        let args = &nix.calls()[0];
        assert_eq!(args[0], "build");
        assert_eq!(args.last(), Some(&path));
        add_roots(
            &nix,
            std::slice::from_ref(&path),
            std::path::Path::new("/g/7/gcroots/root"),
        )?;
        assert_eq!(
            nix.calls()[1][..3],
            ["build", "--out-link", "/g/7/gcroots/root"]
        );
        Ok(())
    }

    #[test]
    fn reads_the_gc_summary_from_stdout() -> Result<()> {
        let nix = Fake::answering("0 store paths deleted, 0.0 KiB freed\n");
        assert_eq!(
            collect_garbage(&nix)?,
            "0 store paths deleted, 0.0 KiB freed"
        );
        Ok(())
    }
}
