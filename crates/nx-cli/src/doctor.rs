//! `nx doctor`: checks that what nx set up is what the system runs, and
//! that nothing in the environment undoes it (design, *CLI
//! surface*). Each check reads plain inputs ([`Inputs`]), so tests need no
//! bootc system; `run` gathers them from `/`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use nx_core::{Deployments, Lock, State, select};

use crate::problems::Problem;
use crate::ui::{self, Ui};

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub what: String,
    /// What to do about it.
    pub hint: Option<String>,
}

fn ok(what: impl Into<String>) -> Finding {
    Finding {
        level: Level::Ok,
        what: what.into(),
        hint: None,
    }
}

fn warn(what: impl Into<String>, hint: impl Into<String>) -> Finding {
    Finding {
        level: Level::Warn,
        what: what.into(),
        hint: Some(hint.into()),
    }
}

fn fail(what: impl Into<String>, hint: impl Into<String>) -> Finding {
    Finding {
        level: Level::Fail,
        what: what.into(),
        hint: Some(hint.into()),
    }
}

/// Everything the checks look at.
pub struct Inputs<'a> {
    pub state: &'a State,
    /// `/`: the merged system, where the extension and sockets show.
    pub root: &'a Path,
    /// The booted image's own tree, for commands it has.
    pub image_root: Option<PathBuf>,
    pub deployments: Option<Deployments>,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// A path's SELinux label, when SELinux is on.
    pub label: &'a dyn Fn(&Path) -> Option<String>,
}

pub fn run(ui: &mut Ui, inputs: &Inputs) -> Result<()> {
    let findings = checks(inputs);
    let out = &mut ui.out;
    let st = out.style;
    out.blank();
    for f in &findings {
        let sym = match f.level {
            Level::Ok => st.green(ui::SYM_DONE),
            Level::Warn => st.yellow(ui::SYM_WARN),
            Level::Fail => st.red(ui::SYM_ERROR),
        };
        out.line(format_args!("{sym} {}", f.what));
        if let Some(hint) = &f.hint {
            out.line(format_args!("  {}", st.dim(hint)));
        }
    }
    out.blank();
    let count = |l| findings.iter().filter(|f| f.level == l).count();
    let (fails, warns) = (count(Level::Fail), count(Level::Warn));
    match (fails, warns) {
        (0, 0) => {
            ui.done("no problems found");
            Ok(())
        }
        (0, w) => {
            ui.warn(format_args!("{}", ui::plural(w, "warning")));
            Ok(())
        }
        (f, w) => bail!(
            "{}{}",
            ui::plural(f, "problem"),
            if w > 0 {
                format!(", {}", ui::plural(w, "warning"))
            } else {
                String::new()
            }
        ),
    }
}

pub fn checks(i: &Inputs) -> Vec<Finding> {
    let mut out = Vec::new();
    let current = i
        .state
        .current_number()
        .ok()
        .flatten()
        .and_then(|n| select::read_lock(i.state, n).ok().map(|l| (n, l)));
    match &current {
        None => out.push(ok("nothing added yet")),
        Some((n, lock)) => {
            generation(i, *n, lock, &mut out);
            tools(i, lock, &mut out);
        }
    }
    for p in crate::problems::problems(i.state, i.deployments.as_ref()) {
        out.push(match p {
            Problem::NoHostTier { image, digest, why } => warn(
                format!(
                    "no host tier for {image} ({}): {why}",
                    ui::short_digest(&digest)
                ),
                "nx sync --refresh tries again",
            ),
            Problem::NotRebuilt { image, digest, why } => warn(
                format!(
                    "{image} ({}) wasn't rebuilt and {why}",
                    ui::short_digest(&digest)
                ),
                "nx sync tries again",
            ),
            Problem::LeftOut { image, packages } => warn(
                format!(
                    "{image} leaves out {}: they don't resolve for it",
                    packages.join(", ")
                ),
                "the next nx sync --refresh tries again",
            ),
            Problem::UpgradeFailed(why) => warn(
                format!("the weekly tool upgrade failed: {why}"),
                "nx upgrade --tool shows it",
            ),
        });
    }
    environment(i, &mut out);
    out
}

/// The current generation against the booted image and what is merged.
fn generation(i: &Inputs, n: u32, lock: &Lock, out: &mut Vec<Finding>) {
    let booted = i
        .deployments
        .as_ref()
        .and_then(|d| d.booted())
        .and_then(|d| d.image_digest.clone());
    match (booted, lock.image.as_deref()) {
        (Some(b), Some(image)) if b != image => out.push(fail(
            format!("generation {n} was built for another image"),
            "nx sync builds one for the booted image",
        )),
        (Some(_), _) if lock.pending => out.push(warn(
            format!("generation {n} is a placeholder: the booted image has no host tier yet"),
            "nx sync",
        )),
        (Some(_), _) => out.push(ok(format!("generation {n} is built for the booted image"))),
        (None, _) => {}
    }
    let sock = i.root.join(nx_proto::SOCKET.trim_start_matches('/'));
    if sock.exists() {
        out.push(ok("nxd is listening"));
    } else {
        out.push(fail(
            "nxd isn't listening, so changes can't go live",
            "systemctl enable --now nxd.socket",
        ));
    }
    if lock.host.is_empty() {
        return;
    }
    // The link nxd makes on activation names root's copy: what is merged.
    let link = i.root.join("var/lib/extensions/nx");
    if std::fs::symlink_metadata(&link).is_err() {
        out.push(warn(
            format!("generation {n} was never made live"),
            "nx sync",
        ));
        return;
    }
    if i.root.join(nx_core::generation::EXTENSION_RELEASE).exists() {
        out.push(ok("the host tier is merged over /usr"));
    } else {
        out.push(fail(
            "the host tier isn't merged over /usr",
            "journalctl -b -u systemd-sysext says why; nx sync merges it again",
        ));
    }
    if let Some(label) = (i.label)(&link.join("usr")).filter(|l| l.contains(":var_lib_t:")) {
        out.push(fail(
            format!("the extension is labelled {label}, so confined programs can't read it"),
            "nx rollback to it again relabels it (nxd runs setfiles)",
        ));
    }
}

/// The tool tier: Nix's daemon, paths still in the store, commands the
/// image took over.
fn tools(i: &Inputs, lock: &Lock, out: &mut Vec<Finding>) {
    let Some(tool) = lock.tool.as_ref().filter(|t| !t.packages.is_empty()) else {
        return;
    };
    if !i.root.join("nix/var/nix/daemon-socket/socket").exists() {
        out.push(fail(
            "the Nix daemon isn't running, so tools can't be added or upgraded",
            "systemctl enable --now nix-daemon.service",
        ));
    }
    // The store under this check's root, as the daemon socket above.
    let has = |p: &str| i.root.join(p.trim_start_matches('/')).exists();
    if crate::tool::incomplete(&has, tool) {
        out.push(warn(
            "some tools' Nix paths are gone from the store",
            "nx upgrade --tool fetches them again",
        ));
    }
    if let Some(image) = &i.image_root {
        for p in &tool.packages {
            for b in &p.bins {
                if crate::host::has_command(image, &b.name) {
                    out.push(warn(
                        format!(
                            "the image has {} now, so {}'s copy is hidden",
                            b.name, p.attr
                        ),
                        format!("nx remove {} if the image's is enough", p.attr),
                    ));
                }
            }
        }
    }
}

/// Variables that make a build or a program mix Nix's libraries with the
/// host's, and tools missing from or ahead on `PATH`.
fn environment(i: &Inputs, out: &mut Vec<Finding>) {
    for var in [
        "LD_LIBRARY_PATH",
        "LIBRARY_PATH",
        "PKG_CONFIG_PATH",
        "CPATH",
        "LD_PRELOAD",
    ] {
        if let Some(v) = (i.env)(var).filter(|v| into_nix(v)) {
            out.push(fail(
                format!("{var} points into Nix ({v})"),
                "programs and builds then load Nix's libraries beside the host's; unset it in your shell config",
            ));
        }
    }
    let path = (i.env)("PATH").unwrap_or_default();
    let dirs: Vec<&str> = path.split(':').collect();
    let bin = "/var/lib/nx/current/bin";
    let at = |d: &str| dirs.iter().position(|x| *x == d);
    match (at(bin), at("/usr/bin")) {
        (None, _) => out.push(warn(
            format!("{bin} isn't on PATH, so tools can't be found by name"),
            "log in again (the image's /etc/profile.d/nx.sh adds it)",
        )),
        (Some(a), Some(u)) if a < u => out.push(warn(
            format!("{bin} comes before /usr/bin on PATH"),
            "a tool then hides the image's command of the same name; it belongs at the end",
        )),
        _ => out.push(ok("tools are on PATH, after the system's commands")),
    }
}

/// Whether a colon list has an entry in the store or a Nix profile.
fn into_nix(list: &str) -> bool {
    list.split(':').any(|d| {
        d.starts_with("/nix/") || d.contains("/.nix-profile") || d.contains("/nix/profile")
    })
}

/// `stat -c %C`: the SELinux label, if SELinux is on.
pub fn label(path: &Path) -> Option<String> {
    let out = Command::new("stat")
        .arg("-c")
        .arg("%C")
        .arg(path)
        .output()
        .ok()?;
    let label = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && label.contains(':')).then_some(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;
    use std::collections::BTreeMap;

    const STATUS: &str = include_str!("../../nx-core/tests/fixtures/rpm-ostree-status.json");

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("nx-doctor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        Fixture(dir)
    }

    fn findings(dir: &Path, env: &BTreeMap<&str, &str>) -> Vec<Finding> {
        let state = State::new(dir.join("state"));
        let lookup = |k: &str| env.get(k).map(|v| (*v).to_owned());
        let inputs = Inputs {
            state: &state,
            root: &dir.join("root"),
            image_root: None,
            deployments: Deployments::from_json(STATUS).ok(),
            env: &lookup,
            label: &|_| None,
        };
        checks(&inputs)
    }

    #[test]
    fn a_fresh_system_with_a_good_path_is_fine() {
        let f = fixture("fresh");
        let env = BTreeMap::from([("PATH", "/usr/local/bin:/usr/bin:/var/lib/nx/current/bin")]);
        assert_eq!(
            findings(&f.0, &env),
            [
                ok("nothing added yet"),
                ok("tools are on PATH, after the system's commands"),
            ]
        );
    }

    #[test]
    fn flags_leaks_and_path_order() {
        let f = fixture("leaks");
        let env = BTreeMap::from([
            ("PATH", "/var/lib/nx/current/bin:/usr/bin"),
            ("LD_LIBRARY_PATH", "/home/u/.nix-profile/lib"),
        ]);
        let got: Vec<(Level, String)> = findings(&f.0, &env)
            .into_iter()
            .map(|f| (f.level, f.what))
            .collect();
        assert_eq!(
            got,
            [
                (Level::Ok, "nothing added yet".to_owned()),
                (
                    Level::Fail,
                    "LD_LIBRARY_PATH points into Nix (/home/u/.nix-profile/lib)".to_owned()
                ),
                (
                    Level::Warn,
                    "/var/lib/nx/current/bin comes before /usr/bin on PATH".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn renders_findings_and_fails_on_a_problem() {
        let f = fixture("render");
        let state = State::new(f.0.join("state"));
        let env = |k: &str| (k == "LD_PRELOAD").then(|| "/nix/store/x/lib.so".to_owned());
        let inputs = Inputs {
            state: &state,
            root: &f.0,
            image_root: None,
            deployments: None,
            env: &env,
            label: &|_| None,
        };
        let out = SharedBuf::default();
        let err = SharedBuf::default();
        let mut ui = Ui::plain(out.clone(), err.clone());
        let result = run(&mut ui, &inputs).map_err(|e| e.to_string());
        assert_eq!(result, Err("1 problem, 1 warning".to_owned()));
        assert_eq!(
            out.text(),
            "
  ✓ nothing added yet
  ✗ LD_PRELOAD points into Nix (/nix/store/x/lib.so)
    programs and builds then load Nix's libraries beside the host's; unset it in your shell config
  ! /var/lib/nx/current/bin isn't on PATH, so tools can't be found by name
    log in again (the image's /etc/profile.d/nx.sh adds it)

"
        );
    }
}
