//! What making a generation live asks of systemd, beyond the merge itself:
//! stop and disable the units of packages that go away (before their files
//! do), create users and files for packages that appear, apply the presets,
//! and restart what an upgrade changed. The unit names come from the
//! generation's `activation.toml`, which the unprivileged CLI wrote, so
//! nothing here acts on a name until it is checked: well-formed, and a unit
//! file the extension itself ships (an image unit like `sshd.service` can
//! never be named, since the extension may not shadow the image).

use std::ffi::OsStr;
use std::path::Path;

use nx_core::Activation;
use nx_scriptlet::{Scope, UnitRef, valid_unit};

use crate::{Output, Runner};

/// What the service steps did, for the CLI to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Services {
    /// "enabled foo.service", "stopped bar.service".
    pub events: Vec<String>,
    /// Steps that failed or were skipped; the merge itself still stands.
    pub warnings: Vec<String>,
}

/// The difference between what is merged now and what will be.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Transition {
    pub remove: Vec<UnitRef>,
    pub preset: Vec<UnitRef>,
    pub restart: Vec<UnitRef>,
    pub sysusers: Vec<String>,
    pub tmpfiles: Vec<String>,
}

/// A package that appears gets its presets applied, one that disappears
/// has its units stopped and disabled, one whose version changes is
/// restarted (`%systemd_post`, `%systemd_preun`, `%systemd_postun_with_restart`).
pub(crate) fn plan(old: &Activation, new: &Activation) -> Transition {
    let mut t = Transition::default();
    for p in &old.packages {
        if new.package(&p.name).is_none() {
            t.remove.extend(p.intents.remove.iter().cloned());
        }
    }
    for p in &new.packages {
        let before = old.package(&p.name);
        let appears = before.is_none();
        let changes = before.is_some_and(|b| b.nevra != p.nevra);
        if appears {
            t.preset.extend(p.intents.preset.iter().cloned());
        }
        if changes {
            t.restart.extend(p.intents.restart.iter().cloned());
        }
        if appears || changes {
            t.sysusers.extend(p.sysusers_files.iter().cloned());
            t.tmpfiles.extend(p.tmpfiles_files.iter().cloned());
        }
    }
    t
}

/// Every package of `old` leaves: what a switch to the prefix backend,
/// which unmerges everything, does to the units.
pub(crate) fn plan_unmerge(old: &Activation) -> Transition {
    plan(old, &Activation::default())
}

fn sh(runner: &dyn Runner, program: &str, args: &[&str]) -> std::io::Result<Output> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    runner.run(program, &args)
}

fn problem(program: &str, args: &[&str], out: &Output) -> String {
    let text = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    format!(
        "{program} {}: {}",
        args.join(" "),
        text.lines().next().unwrap_or("failed")
    )
}

/// Runs `program args` and records a warning if it fails.
fn run(runner: &dyn Runner, program: &str, args: &[&str], s: &mut Services) -> Option<Output> {
    match sh(runner, program, args) {
        Ok(out) if out.success => Some(out),
        Ok(out) => {
            s.warnings.push(problem(program, args, &out));
            None
        }
        Err(e) => {
            s.warnings.push(format!("{program}: can't run it: {e}"));
            None
        }
    }
}

/// The units of `scope` in `want` that are well-formed and shipped by the
/// extension at `ext`.
fn unit_dir(scope: Scope) -> &'static str {
    match scope {
        Scope::System => "usr/lib/systemd/system",
        Scope::User => "usr/lib/systemd/user",
    }
}

fn shipped(ext: &Path, scope: Scope, want: &[UnitRef], s: &mut Services) -> Vec<String> {
    let dir = unit_dir(scope);
    let mut out: Vec<String> = Vec::new();
    for u in want.iter().filter(|u| u.scope == scope) {
        if !valid_unit(&u.name) {
            s.warnings
                .push(format!("{}: not a unit name nx will act on", u.name));
            continue;
        }
        // `foo@*.service` means the instances of the template `foo@.service`.
        let file = u.name.replace("@*", "@");
        // A symlink is an alias; one to /dev/null is a mask, not a unit.
        let path = ext.join(dir).join(&file);
        let present = std::fs::symlink_metadata(&path).is_ok_and(|m| {
            m.is_file()
                || (m.file_type().is_symlink()
                    && std::fs::read_link(&path).is_ok_and(|t| t != Path::new("/dev/null")))
        });
        if !present {
            s.warnings
                .push(format!("{}: the extension ships no such unit", u.name));
            continue;
        }
        if !out.contains(&u.name) {
            out.push(u.name.clone());
        }
    }
    out
}

/// The units among `names` whose file has an `[Install]` section: the
/// ones `systemctl preset` can enable. An alias is read through to its
/// unit in the same directory.
fn installable(ext: &Path, scope: Scope, names: Vec<String>) -> Vec<String> {
    let dir = ext.join(unit_dir(scope));
    names
        .into_iter()
        .filter(|n| {
            let mut path = dir.join(n.replace("@*", "@"));
            if let Ok(target) = std::fs::read_link(&path)
                && let Some(name) = target.file_name()
            {
                path = dir.join(name);
            }
            std::fs::read_to_string(&path)
                .is_ok_and(|text| text.lines().any(|l| l.trim() == "[Install]"))
        })
        .collect()
}

/// The uids with a running user manager.
fn logged_in(runner: &dyn Runner) -> Vec<String> {
    let Ok(out) = sh(
        runner,
        "systemctl",
        &[
            "list-units",
            "user@*.service",
            "--legend=no",
            "--plain",
            "--no-pager",
        ],
    ) else {
        return Vec::new();
    };
    out.stdout
        .lines()
        .filter_map(|l| {
            let unit = l.split_whitespace().next()?;
            let uid = unit.strip_prefix("user@")?.strip_suffix(".service")?;
            uid.chars()
                .all(|c| c.is_ascii_digit())
                .then(|| uid.to_owned())
        })
        .collect()
}

/// The instances behind a `foo@*.service` pattern: those running now and
/// those enabled by a link in `/etc/systemd/system/*.wants` (`systemctl
/// disable` takes no glob, and a stop alone leaves the links dangling).
/// A name without a glob stands for itself.
fn expand(runner: &dyn Runner, root: &Path, name: &str) -> Vec<String> {
    let Some((head, _)) = name.split_once('*') else {
        return vec![name.to_owned()];
    };
    let tail = name.rsplit('*').next().unwrap_or("");
    let matches =
        |n: &str| n.starts_with(head) && n.ends_with(tail) && valid_unit(n) && !n.contains('*');
    let mut found: Vec<String> = Vec::new();
    if let Ok(out) = sh(
        runner,
        "systemctl",
        &["list-units", name, "--plain", "--no-legend", "--no-pager"],
    ) {
        found.extend(
            out.stdout
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .filter(|n| matches(n))
                .map(str::to_owned),
        );
    }
    if let Ok(dirs) = std::fs::read_dir(root.join("etc/systemd/system")) {
        for dir in dirs.flatten() {
            let dir_name = dir.file_name().to_string_lossy().into_owned();
            if !dir_name.ends_with(".wants") && !dir_name.ends_with(".requires") {
                continue;
            }
            if let Ok(links) = std::fs::read_dir(dir.path()) {
                for l in links.flatten() {
                    let n = l.file_name().to_string_lossy().into_owned();
                    if matches(&n) && !found.contains(&n) {
                        found.push(n);
                    }
                }
            }
        }
    }
    found
}

/// Stops and disables the units of packages that leave, while their files
/// are still merged. `old_ext` is the extension about to go.
pub(crate) fn stop(
    runner: &dyn Runner,
    root: &Path,
    old_ext: &Path,
    t: &Transition,
    s: &mut Services,
) {
    let system = shipped(old_ext, Scope::System, &t.remove, s);
    if !system.is_empty() {
        // Stopping first reaches running instances of a template; disabling
        // takes the enabled ones, which `list-unit-files` names.
        let mut stop_args = vec!["stop", "--no-block"];
        stop_args.extend(system.iter().map(String::as_str));
        let _ = sh(runner, "systemctl", &stop_args);
        let mut names: Vec<String> = Vec::new();
        for n in &system {
            names.extend(expand(runner, root, n));
        }
        if !names.is_empty() {
            let mut args = vec!["--no-reload", "disable", "--now", "--no-warn"];
            args.extend(names.iter().map(String::as_str));
            if run(runner, "systemctl", &args, s).is_some() {
                s.events
                    .push(format!("stopped and disabled {}", system.join(", ")));
            }
        }
    }
    let user = shipped(old_ext, Scope::User, &t.remove, s);
    if !user.is_empty() {
        let mut args = vec!["--global", "disable", "--no-warn"];
        args.extend(user.iter().map(String::as_str));
        run(runner, "systemctl", &args, s);
        for uid in logged_in(runner) {
            let machine = format!("{uid}@");
            let mut args = vec![
                "--user",
                "-M",
                machine.as_str(),
                "disable",
                "--now",
                "--no-warn",
            ];
            args.extend(user.iter().map(String::as_str));
            let _ = sh(runner, "systemctl", &args);
        }
        s.events
            .push(format!("disabled {} for every user", user.join(", ")));
    }
}

fn valid_conf(name: &str) -> bool {
    name.ends_with(".conf")
        && name.len() > 5
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '@' | '-'))
}

/// After the merge: users and files first (a unit may need them), then the
/// presets, then restarts. `new_ext` is the extension now merged.
pub(crate) fn start(runner: &dyn Runner, new_ext: &Path, t: &Transition, s: &mut Services) {
    for (tool, dir, files, flag) in [
        ("systemd-sysusers", "usr/lib/sysusers.d", &t.sysusers, None),
        (
            "systemd-tmpfiles",
            "usr/lib/tmpfiles.d",
            &t.tmpfiles,
            Some("--create"),
        ),
    ] {
        let mut paths = Vec::new();
        for f in files {
            if valid_conf(f) && new_ext.join(dir).join(f).is_file() {
                paths.push(format!("/{dir}/{f}"));
            } else {
                s.warnings
                    .push(format!("{f}: not a {tool} file the extension ships"));
            }
        }
        if paths.is_empty() {
            continue;
        }
        let mut args: Vec<&str> = flag.into_iter().collect();
        args.extend(paths.iter().map(String::as_str));
        if run(runner, tool, &args, s).is_some() {
            let what = if tool == "systemd-sysusers" {
                "users and groups"
            } else {
                "files and directories"
            };
            s.events
                .push(format!("created {what} from {}", files.join(", ")));
        }
    }

    // A unit with no [Install] section (polkit.service) can't be enabled,
    // and systemctl preset fails on it; the macros skip it, and so does
    // this.
    let system = installable(
        new_ext,
        Scope::System,
        shipped(new_ext, Scope::System, &t.preset, s),
    );
    if !system.is_empty() {
        let mut args = vec!["preset"];
        args.extend(system.iter().map(String::as_str));
        run(runner, "systemctl", &args, s);
        // The preset says whether the unit is wanted; a package installed
        // live is wanted now, not at the next boot. A template has no
        // instance to start.
        for n in system.iter().filter(|n| !n.contains('@')) {
            if state(runner, &["is-enabled", n]) != "enabled" {
                continue;
            }
            if state(runner, &["is-active", n]) != "active" {
                run(runner, "systemctl", &["start", n], s);
            }
            if state(runner, &["is-active", n]) == "active" {
                s.events.push(format!("enabled and started {n}"));
            } else {
                s.events.push(format!("enabled {n}"));
            }
        }
    }
    let user = installable(
        new_ext,
        Scope::User,
        shipped(new_ext, Scope::User, &t.preset, s),
    );
    if !user.is_empty() {
        let mut args = vec!["--global", "preset"];
        args.extend(user.iter().map(String::as_str));
        run(runner, "systemctl", &args, s);
        for n in user.iter().filter(|n| !n.contains('@')) {
            if state(runner, &["--global", "is-enabled", n]) == "enabled" {
                s.events
                    .push(format!("enabled {n} for every user, from their next login"));
            }
        }
    }

    let restart = shipped(new_ext, Scope::System, &t.restart, s);
    if !restart.is_empty() {
        let mut args = vec!["try-restart"];
        args.extend(restart.iter().map(String::as_str));
        if run(runner, "systemctl", &args, s).is_some() {
            s.events
                .push(format!("restarted {} if running", restart.join(", ")));
        }
    }
    let user_restart = shipped(new_ext, Scope::User, &t.restart, s);
    if !user_restart.is_empty() {
        for uid in logged_in(runner) {
            let machine = format!("{uid}@");
            let mut args = vec!["--user", "-M", machine.as_str(), "try-restart"];
            args.extend(user_restart.iter().map(String::as_str));
            let _ = sh(runner, "systemctl", &args);
        }
    }
}

/// `systemctl` query output, first word, empty when it can't be asked.
fn state(runner: &dyn Runner, args: &[&str]) -> String {
    sh(runner, "systemctl", args)
        .ok()
        .and_then(|o| o.stdout.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default()
}
