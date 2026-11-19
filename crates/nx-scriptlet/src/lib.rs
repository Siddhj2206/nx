//! nx never runs an RPM scriptlet (design, *Scriptlet translation*). This
//! crate reads each one, matches it against the idioms Fedora's macros
//! expand to, and says what declarative thing that means: units to preset,
//! stop or restart, and users to create. Whatever it can't match is
//! returned as a [`Leftover`], and the caller refuses the package.
//!
//! The matcher is deliberately small. It knows a handful of shell shapes
//! exactly (an `if [ $1 -eq 1 ]` guard around one command, a command with
//! `|| :` after it) and nothing else, so a script it accepts can't be doing
//! anything it didn't name. Pure: no file, no process, no network.

mod files;
mod shell;
mod units;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use files::{PackageFile, PackageMeta, Tmpfiles, tmpfiles};
pub use units::valid_unit;

/// A scriptlet as the header records it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    /// The interpreter; empty means `/bin/sh`.
    pub interpreter: String,
    pub body: String,
}

/// When in a transaction a scriptlet runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Pretrans,
    Pre,
    Post,
    Preun,
    Postun,
    Posttrans,
    Preuntrans,
    Postuntrans,
}

impl Phase {
    pub const ALL: [Phase; 8] = [
        Phase::Pretrans,
        Phase::Pre,
        Phase::Post,
        Phase::Preun,
        Phase::Postun,
        Phase::Posttrans,
        Phase::Preuntrans,
        Phase::Postuntrans,
    ];

    /// The spec-file name, `%post`.
    pub fn name(self) -> &'static str {
        match self {
            Phase::Pretrans => "%pretrans",
            Phase::Pre => "%pre",
            Phase::Post => "%post",
            Phase::Preun => "%preun",
            Phase::Postun => "%postun",
            Phase::Posttrans => "%posttrans",
            Phase::Preuntrans => "%preuntrans",
            Phase::Postuntrans => "%postuntrans",
        }
    }
}

/// A scriptlet that runs when another package changes (a trigger), or when
/// files under a path change (a file trigger).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    /// `in`, `un`, `postun` (rpm's own words), or a path prefix's kind for
    /// file triggers.
    pub kind: String,
    /// The package (or, for a file trigger, the path) it watches.
    pub target: String,
    pub script: Script,
}

/// Every scriptlet and trigger of one package.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scripts {
    pub phases: Vec<(Phase, Script)>,
    pub triggers: Vec<Trigger>,
    pub file_triggers: Vec<Trigger>,
}

/// Which units a package's scriptlets touch, as systemd sees the scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    System,
    User,
}

/// One unit a scriptlet names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitRef {
    pub scope: Scope,
    pub name: String,
}

/// What a package's scriptlets amount to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intents {
    /// Units to apply the presets to when the package is first installed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preset: Vec<UnitRef>,
    /// Units to stop and disable when the package is removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<UnitRef>,
    /// Units to restart (if running) when the package is upgraded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart: Vec<UnitRef>,
    /// `sysusers.d` lines for users and groups the scriptlets create.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sysusers: Vec<String>,
    /// Commands recognised and deliberately not run, for the report
    /// (`ldconfig`, cache updates).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// Script text no idiom matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    /// Where it came from: `%post`, `%triggerin -- samba-client`.
    pub source: String,
    pub lines: Vec<String>,
}

/// The answer for one package.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Translation {
    pub intents: Intents,
    pub leftovers: Vec<Leftover>,
}

impl Translation {
    /// A fingerprint of the leftover text, which an allowlist entry
    /// records: when the package's scriptlets change, so does it, and the
    /// package is refused again until someone reviews the new text.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        for l in &self.leftovers {
            h.update(l.source.as_bytes());
            h.update(b"\n");
            for line in &l.lines {
                h.update(line.as_bytes());
                h.update(b"\n");
            }
            h.update(b"\0");
        }
        let mut out = String::with_capacity(64);
        for b in h.finalize() {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }
}

/// What the translator needs to know about the system a trigger would fire
/// on.
pub struct Context<'a> {
    /// Whether a package of this name will be installed (in the image or
    /// the generation).
    pub present: &'a dyn Fn(&str) -> bool,
    /// Whether nx can remove it (it is in the generation, not the image).
    pub removable: &'a dyn Fn(&str) -> bool,
}

/// Translates a package's scriptlets.
pub fn translate(scripts: &Scripts, ctx: &Context) -> Translation {
    let mut t = Translation::default();
    for (phase, script) in &scripts.phases {
        run(phase.name(), script, &mut t);
    }
    for trig in &scripts.triggers {
        // A trigger fires when its target package is installed (`in`) or
        // removed (`un`, `postun`). The image's packages are never removed,
        // and a target nx doesn't install can't fire anything.
        let fires = match trig.kind.as_str() {
            "in" | "prein" => (ctx.present)(&trig.target),
            _ => (ctx.removable)(&trig.target),
        };
        if fires {
            run(
                &format!("%trigger{} -- {}", trig.kind, trig.target),
                &trig.script,
                &mut t,
            );
        }
    }
    for trig in &scripts.file_triggers {
        run(
            &format!("%filetrigger{} -- {}", trig.kind, trig.target),
            &trig.script,
            &mut t,
        );
    }
    t.intents.dedup();
    t
}

impl Intents {
    fn dedup(&mut self) {
        fn unique<T: PartialEq>(v: &mut Vec<T>) {
            let mut out: Vec<T> = Vec::with_capacity(v.len());
            for x in v.drain(..) {
                if !out.contains(&x) {
                    out.push(x);
                }
            }
            *v = out;
        }
        unique(&mut self.preset);
        unique(&mut self.remove);
        unique(&mut self.restart);
        unique(&mut self.sysusers);
        unique(&mut self.skipped);
    }
}

const SHELLS: [&str; 7] = [
    "",
    "/bin/sh",
    "/bin/bash",
    "/usr/bin/sh",
    "/usr/bin/bash",
    "sh",
    "bash",
];

fn run(source: &str, script: &Script, t: &mut Translation) {
    let lines = shell::lines(&script.body);
    let interp = script.interpreter.trim();
    if lines.is_empty() {
        // An empty body does nothing under any interpreter; `ldconfig` as
        // the interpreter is the old `%post -p /sbin/ldconfig`.
        if interp.ends_with("ldconfig") {
            t.intents.skipped.push("ldconfig".into());
        }
        return;
    }
    if !SHELLS.contains(&interp) {
        t.leftovers.push(Leftover {
            source: format!("{source} (-p {interp})"),
            lines,
        });
        return;
    }
    let mut left = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if line.starts_with("if ") {
            let Some(end) = block_end(&lines, i) else {
                left.extend(lines[i..].iter().cloned());
                break;
            };
            if !units::block(&lines[i..=end], &mut t.intents) {
                left.extend(lines[i..=end].iter().cloned());
            }
            i = end + 1;
        } else {
            if !units::command(line, units::Guard::ALWAYS, &mut t.intents) {
                left.push(line.clone());
            }
            i += 1;
        }
    }
    if !left.is_empty() {
        t.leftovers.push(Leftover {
            source: source.to_owned(),
            lines: left,
        });
    }
}

/// The index of the `fi` closing the `if` at `start`.
fn block_end(lines: &[String], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, line) in lines.iter().enumerate().skip(start) {
        if line.starts_with("if ") {
            depth += 1;
        } else if line == "fi" {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
