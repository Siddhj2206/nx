//! The idioms themselves: guards on `$1`, systemd-update-helper and
//! `systemctl` lines, cache and linker commands, and the old
//! `getent … || useradd` pattern.

use crate::shell;
use crate::{Intents, Scope, UnitRef};

/// In which of a scriptlet's three situations its guard holds. rpm passes
/// `$1`, the number of instances of the package left after the
/// transaction: 1 on install, 2 on upgrade, 0 on removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard {
    install: bool,
    upgrade: bool,
    remove: bool,
}

impl Guard {
    pub const ALWAYS: Guard = Guard {
        install: true,
        upgrade: true,
        remove: true,
    };

    fn and(self, o: Guard) -> Guard {
        Guard {
            install: self.install && o.install,
            upgrade: self.upgrade && o.upgrade,
            remove: self.remove && o.remove,
        }
    }

    /// `[ $1 -OP N ]`.
    fn parse(tokens: &[&str]) -> Option<Guard> {
        let [lhs, op, rhs] = tokens else { return None };
        if lhs.trim_matches('"') != "$1" {
            return None;
        }
        let n: i64 = rhs.trim_matches('"').parse().ok()?;
        let holds = |v: i64| match *op {
            "-eq" | "=" | "==" => v == n,
            "-ne" | "!=" => v != n,
            "-ge" => v >= n,
            "-gt" => v > n,
            "-le" => v <= n,
            "-lt" => v < n,
            _ => false,
        };
        if !["-eq", "=", "==", "-ne", "!=", "-ge", "-gt", "-le", "-lt"].contains(op) {
            return None;
        }
        Some(Guard {
            install: holds(1),
            upgrade: holds(2),
            remove: holds(0),
        })
    }
}

/// Whether `name` is a unit nx will act on: the characters a unit file name
/// has, a type systemd would load from a package, and a glob only where a
/// template's instances are meant (`foo@*.service`).
pub fn valid_unit(name: &str) -> bool {
    const TYPES: [&str; 5] = [".service", ".socket", ".timer", ".path", ".target"];
    let Some(stem) = TYPES.iter().find_map(|t| name.strip_suffix(t)) else {
        return false;
    };
    if stem.is_empty() || stem.starts_with('.') || stem.starts_with('-') {
        return false;
    }
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '-' | '@');
    match stem.split_once("@*") {
        Some((head, "")) => !head.is_empty() && head.chars().all(ok),
        _ => stem.chars().all(ok),
    }
}

fn units(words: &[String], scope: Scope) -> Option<Vec<UnitRef>> {
    if words.is_empty() || !words.iter().all(|w| valid_unit(w)) {
        return None;
    }
    Some(
        words
            .iter()
            .map(|w| UnitRef {
                scope,
                name: w.clone(),
            })
            .collect(),
    )
}

/// An `if … ; then … fi` block (`lines` runs from the `if` through its
/// `fi`): a guard, and commands that each match. `false` leaves `out`
/// untouched.
pub fn block(lines: &[String], out: &mut Intents) -> bool {
    let Some(first) = lines.first() else {
        return false;
    };
    let mut head = first.trim_start_matches("if ").trim().to_owned();
    let mut body = &lines[1..lines.len() - 1];
    if let Some(h) = head.strip_suffix("then") {
        head = h.trim().trim_end_matches(';').trim().to_owned();
    } else if body.first().is_some_and(|l| l == "then") {
        head = head.trim_end_matches(';').trim().to_owned();
        body = &body[1..];
    } else {
        return false;
    }
    let Some(guard) = condition(&head) else {
        return false;
    };
    let mut staged = Intents::default();
    if body.is_empty() || !body.iter().all(|l| command(l, guard, &mut staged)) {
        return false;
    }
    merge(out, staged);
    true
}

fn merge(out: &mut Intents, from: Intents) {
    out.preset.extend(from.preset);
    out.remove.extend(from.remove);
    out.restart.extend(from.restart);
    out.sysusers.extend(from.sysusers);
    out.skipped.extend(from.skipped);
}

/// `[ guard ] && [ -x helper ]`, parts in any order.
fn condition(head: &str) -> Option<Guard> {
    let mut guard = Guard::ALWAYS;
    let mut guarded = false;
    for part in head.split("&&") {
        let part = part.trim();
        let inner = part.strip_prefix('[')?.strip_suffix(']')?;
        let tokens: Vec<&str> = inner.split_whitespace().collect();
        if let ["-x", path] = tokens[..] {
            let path = path.trim_matches('"');
            if path.ends_with("systemd-update-helper") || path.ends_with("systemctl") {
                continue;
            }
            return None;
        }
        guard = guard.and(Guard::parse(&tokens)?);
        guarded = true;
    }
    guarded.then_some(guard)
}

/// One command under `guard`. `false` when it is not an idiom nx knows.
pub fn command(line: &str, guard: Guard, out: &mut Intents) -> bool {
    let line = shell::strip_noise(line);
    if let Some((left, right)) = shell::split_or(line) {
        return account(shell::strip_noise(left), right, out);
    }
    let Some(words) = shell::words(line) else {
        return false;
    };
    let Some(program) = words.first() else {
        return false;
    };
    let name = program.rsplit('/').next().unwrap_or(program);
    match name {
        "systemd-update-helper" => helper(&words[1..], guard, out),
        "systemctl" => systemctl(&words[1..], guard, out),
        "ldconfig" => {
            out.skipped.push("ldconfig".into());
            true
        }
        "exit" => words.len() == 2 && words[1] == "0",
        ":" | "true" => true,
        "groupadd" | "useradd" => account_command(&words, out),
        _ if CACHES.contains(&name) => {
            out.skipped.push(name.to_owned());
            true
        }
        "touch" if words.get(1).is_some_and(|w| w == "--no-create") => {
            out.skipped.push("touch --no-create".into());
            true
        }
        _ => false,
    }
}

/// Commands that rebuild a cache. The merged `/usr` is read-only, so they
/// could not write their output there anyway, and nothing in the host
/// tier depends on one (design, *Scriptlet translation*).
const CACHES: [&str; 8] = [
    "glib-compile-schemas",
    "gtk-update-icon-cache",
    "gtk4-update-icon-cache",
    "update-desktop-database",
    "update-mime-database",
    "fc-cache",
    "gio-querymodules-64",
    "gio-querymodules",
];

fn helper(args: &[String], guard: Guard, out: &mut Intents) -> bool {
    let Some((verb, rest)) = args.split_first() else {
        return false;
    };
    let (kind, scope) = match verb.as_str() {
        "install-system-units" => (Kind::Preset, Scope::System),
        "install-user-units" => (Kind::Preset, Scope::User),
        "remove-system-units" => (Kind::Remove, Scope::System),
        "remove-user-units" => (Kind::Remove, Scope::User),
        "mark-restart-system-units" | "mark-reload-system-units" => (Kind::Restart, Scope::System),
        "mark-restart-user-units" | "mark-reload-user-units" => (Kind::Restart, Scope::User),
        // nxd's refresh reloads the manager; restarts are done per unit.
        "system-reload-restart"
        | "system-reload"
        | "system-restart"
        | "user-reload-restart"
        | "user-reload"
        | "user-restart"
        | "user-reexec" => return rest.is_empty(),
        _ => return false,
    };
    add(kind, units(rest, scope), guard, out)
}

fn systemctl(args: &[String], guard: Guard, out: &mut Intents) -> bool {
    let mut scope = Scope::System;
    let mut i = 0;
    while let Some(opt) = args.get(i).filter(|a| a.starts_with('-')) {
        match opt.as_str() {
            "--no-reload" | "--system" | "-q" | "--quiet" | "--no-warn" | "--now"
            | "--no-block" => {}
            "--global" => scope = Scope::User,
            _ => return false,
        }
        i += 1;
    }
    let Some(verb) = args.get(i) else {
        return false;
    };
    let rest = &args[i + 1..];
    match verb.as_str() {
        "preset" => add(Kind::Preset, units(rest, scope), guard, out),
        "disable" | "stop" => add(Kind::Remove, units(rest, scope), guard, out),
        "try-restart" | "try-reload-or-restart" => {
            add(Kind::Restart, units(rest, scope), guard, out)
        }
        // nxd's refresh reloads the manager.
        "daemon-reload" | "daemon-reexec" => rest.is_empty(),
        _ => false,
    }
}

enum Kind {
    Preset,
    Remove,
    Restart,
}

/// Records `units` for `kind` when the guard says the command runs in the
/// situation that kind means: a preset on first install, a stop and disable
/// only on removal (never on the removal half of an upgrade), a restart on
/// upgrade.
fn add(kind: Kind, units: Option<Vec<UnitRef>>, guard: Guard, out: &mut Intents) -> bool {
    let Some(units) = units else {
        return false;
    };
    match kind {
        Kind::Preset if guard.install => out.preset.extend(units),
        Kind::Remove if guard.remove && !guard.upgrade && !guard.install => {
            out.remove.extend(units);
        }
        Kind::Restart if guard.upgrade => out.restart.extend(units),
        _ => return false,
    }
    true
}

/// `getent group x || groupadd x`, and the same for `passwd`/`useradd`.
fn account(left: &str, right: &str, out: &mut Intents) -> bool {
    let Some(l) = shell::words(left) else {
        return false;
    };
    let Some(r) = shell::words(shell::strip_noise(right)) else {
        return false;
    };
    let guard_ok = match (l.as_slice(), r.first().map(|w| w.rsplit('/').next())) {
        ([getent, db, name], Some(Some(cmd))) if getent == "getent" => {
            (db == "group" && cmd == "groupadd" && r.last() == Some(name))
                || (db == "passwd" && cmd == "useradd" && r.last() == Some(name))
        }
        _ => false,
    };
    guard_ok && account_command(&r, out)
}

fn account_command(words: &[String], out: &mut Intents) -> bool {
    let program = words[0].rsplit('/').next().unwrap_or("");
    let mut line_out = Vec::new();
    let ok = if program == "groupadd" {
        groupadd(&words[1..], &mut line_out)
    } else {
        useradd(&words[1..], &mut line_out)
    };
    if ok {
        out.sysusers.extend(line_out);
    }
    ok
}

fn valid_name(n: &str) -> bool {
    let mut chars = n.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && n.len() <= 31
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

fn groupadd(args: &[String], out: &mut Vec<String>) -> bool {
    let mut gid = "-".to_owned();
    let mut name = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-r" | "--system" | "-f" => {}
            "-g" => match it.next() {
                Some(g) if g.chars().all(|c| c.is_ascii_digit()) => gid = g.clone(),
                _ => return false,
            },
            n if !n.starts_with('-') && name.is_none() => name = Some(n),
            _ => return false,
        }
    }
    let Some(name) = name.filter(|n| valid_name(n)) else {
        return false;
    };
    out.push(format!("g {name} {gid}"));
    true
}

fn useradd(args: &[String], out: &mut Vec<String>) -> bool {
    let (mut uid, mut group, mut home, mut shell_, mut gecos) =
        ("-".to_owned(), None, None, None, None);
    let mut extra = Vec::new();
    let mut name = None;
    let mut it = args.iter();
    let plain = |s: &str| !s.contains(['"', '\\', ' ']);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-r" | "--system" | "-M" | "-N" | "-U" => {}
            "-u" => match it.next() {
                Some(u) if u.chars().all(|c| c.is_ascii_digit()) && !u.is_empty() => {
                    uid = u.clone();
                }
                _ => return false,
            },
            "-g" => match it.next() {
                Some(g) if valid_name(g) || g.chars().all(|c| c.is_ascii_digit()) => {
                    group = Some(g.clone());
                }
                _ => return false,
            },
            "-G" => match it.next() {
                Some(g) if g.split(',').all(valid_name) => {
                    extra.extend(g.split(',').map(str::to_owned));
                }
                _ => return false,
            },
            "-d" => match it.next() {
                Some(d) if d.starts_with('/') && plain(d) => home = Some(d.clone()),
                _ => return false,
            },
            "-s" => match it.next() {
                Some(s) if s.starts_with('/') && plain(s) => shell_ = Some(s.clone()),
                _ => return false,
            },
            "-c" => match it.next() {
                Some(c) if !c.contains(['"', '\\']) => gecos = Some(c.clone()),
                _ => return false,
            },
            n if !n.starts_with('-') && name.is_none() => name = Some(n.to_owned()),
            _ => return false,
        }
    }
    let Some(name) = name.filter(|n| valid_name(n)) else {
        return false;
    };
    let id = group.map_or(uid.clone(), |g| format!("{uid}:{g}"));
    let gecos = gecos.map_or_else(|| "-".to_owned(), |c| format!("\"{c}\""));
    out.push(format!(
        "u {name} {id} {gecos} {} {}",
        home.as_deref().unwrap_or("-"),
        shell_.as_deref().unwrap_or("-")
    ));
    for g in extra {
        out.push(format!("m {name} {g}"));
    }
    true
}
