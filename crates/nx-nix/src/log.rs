//! Nix's `--log-format internal-json` stream, as the notes nx draws:
//! what was fetched and how big it was, what was built, retries, warnings
//! and errors (ported from nx 0.x's `nixlog`).
//!
//! The format is Nix's own and unversioned (ADR-0001). The parser keeps to
//! activity and result types that have been stable for years, and degrades
//! rather than fails: an action, type or field it doesn't know is skipped,
//! a field of another JSON type reads as absent, and an `@nix` line it
//! can't read at all is dropped. A Nix release that changes the log loses
//! nx some notes and progress, never the install. A line that isn't `@nix`
//! (Nix printing plainly) comes out as text.
//! Nix logs download retries at error level with "warning:" in the text,
//! and recovered failures as "error (ignored):": notes are classified by
//! text, not level.

use std::collections::HashMap;

use crate::{store_name, strip_ansi};

// Activity types (Nix's ActivityType).
const ACT_COPY_PATH: i64 = 100;
const ACT_FILE_TRANSFER: i64 = 101;
const ACT_BUILD: i64 = 105;
const ACT_SUBSTITUTE: i64 = 108;

// Result types (Nix's ResultType).
const RES_PROGRESS: i64 = 105;

// Verbosity levels.
const LVL_ERROR: i64 = 0;
const LVL_WARN: i64 = 1;
const LVL_TALK: i64 = 4;

/// One `@nix` line.
#[derive(Debug, Clone, Default)]
pub struct Event {
    pub action: String,
    pub id: u64,
    pub parent: u64,
    pub kind: i64,
    pub level: i64,
    pub text: String,
    pub msg: String,
    pub fields: Vec<serde_json::Value>,
}

impl Event {
    /// Reads one line; `None` for one that isn't an `@nix` line. Each field
    /// is read on its own, so one Nix renamed or retyped is only missing:
    /// a missing `level` reads as quieter than any nx shows.
    pub fn parse(line: &str) -> Option<Self> {
        let json = line.trim().strip_prefix("@nix")?;
        let value: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        let get = |k: &str| value.get(k);
        let text = |k: &str| {
            get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let int = |k: &str| get(k).and_then(serde_json::Value::as_i64);
        let id = |k: &str| get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        Some(Self {
            action: text("action"),
            id: id("id"),
            parent: id("parent"),
            kind: int("type").unwrap_or(-1),
            level: int("level").unwrap_or(i64::MAX),
            text: text("text"),
            msg: text("msg"),
            fields: get("fields")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        })
    }

    fn str(&self, i: usize) -> &str {
        self.fields
            .get(i)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
    }

    fn int(&self, i: usize) -> u64 {
        self.fields
            .get(i)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    }
}

/// Something worth a permanent line of output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A store path came from a binary cache: its name and download size.
    Fetched {
        name: String,
        size: u64,
    },
    /// A derivation started building locally.
    Building {
        name: String,
    },
    Built {
        name: String,
    },
    /// A download failed and Nix is trying again.
    Retry {
        what: String,
        why: String,
    },
    Warning(String),
    Error(String),
    /// A line that wasn't an event.
    Raw(String),
}

#[derive(Default)]
struct Activity {
    kind: i64,
    /// For a substitution and the copy and transfer under it: the path.
    path: Option<String>,
    build: Option<String>,
}

/// Turns lines into notes.
#[derive(Default)]
pub struct Tracker {
    acts: HashMap<u64, Activity>,
    /// Bytes received per substituted path, from its transfer's progress.
    sizes: HashMap<String, u64>,
    /// Transfer URL → path, to name a retry.
    urls: HashMap<String, String>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles one raw line.
    pub fn line(&mut self, line: &str) -> Vec<Note> {
        match Event::parse(line) {
            Some(ev) => self.handle(&ev),
            None if line.trim().is_empty() => Vec::new(),
            None => vec![Note::Raw(line.trim_end().to_owned())],
        }
    }

    fn handle(&mut self, ev: &Event) -> Vec<Note> {
        match ev.action.as_str() {
            "start" => self.start(ev),
            "stop" => self.stop(ev),
            "result" => {
                self.result(ev);
                Vec::new()
            }
            "msg" => msg(ev),
            _ => Vec::new(),
        }
    }

    fn start(&mut self, ev: &Event) -> Vec<Note> {
        let parent_path = self.acts.get(&ev.parent).and_then(|a| a.path.clone());
        let mut a = Activity {
            kind: ev.kind,
            ..Activity::default()
        };
        let mut notes = Vec::new();
        match ev.kind {
            ACT_SUBSTITUTE => a.path = Some(ev.str(0).to_owned()),
            ACT_COPY_PATH => a.path = parent_path,
            ACT_FILE_TRANSFER => {
                if let Some(p) = parent_path {
                    self.urls.insert(ev.str(0).to_owned(), p.clone());
                    a.path = Some(p);
                }
            }
            ACT_BUILD if !ev.str(0).is_empty() => {
                let name = store_name(ev.str(0)).trim_end_matches(".drv").to_owned();
                notes.push(Note::Building { name: name.clone() });
                a.build = Some(name);
            }
            _ => {}
        }
        self.acts.insert(ev.id, a);
        notes
    }

    fn stop(&mut self, ev: &Event) -> Vec<Note> {
        let Some(a) = self.acts.remove(&ev.id) else {
            return Vec::new();
        };
        match (a.kind, a.path, a.build) {
            (ACT_SUBSTITUTE, Some(path), _) => {
                let size = self.sizes.remove(&path).unwrap_or(0);
                vec![Note::Fetched {
                    name: store_name(&path).to_owned(),
                    size,
                }]
            }
            (ACT_BUILD, _, Some(name)) => vec![Note::Built { name }],
            _ => Vec::new(),
        }
    }

    fn result(&mut self, ev: &Event) {
        if ev.kind != RES_PROGRESS {
            return;
        }
        let Some(a) = self.acts.get(&ev.id) else {
            return;
        };
        if a.kind == ACT_FILE_TRANSFER
            && let Some(path) = &a.path
        {
            let seen = ev.int(0).max(ev.int(1));
            let size = self.sizes.entry(path.clone()).or_default();
            *size = (*size).max(seen);
        }
    }

    /// Names the path a retried URL was for.
    pub fn retried(&self, url: &str) -> Option<&str> {
        self.urls.get(url).map(|p| store_name(p))
    }
}

fn msg(ev: &Event) -> Vec<Note> {
    if ev.level >= LVL_TALK {
        return Vec::new();
    }
    let text = strip_ansi(&ev.msg);
    // The plan Nix prints before fetching: nx draws its own.
    if text.starts_with("  /nix/store/")
        || text.contains(" will be fetched (")
        || text.contains(" will be built:")
    {
        return Vec::new();
    }
    // "retrying in 281 ms", or (Nix 2.35, resuming) "retrying from offset
    // 11583470 in 32 ms".
    if text.contains("; retrying ") {
        return vec![retry(&text)];
    }
    if text.starts_with("error (ignored):") {
        // Nix recovered by itself (a busy eval cache, say).
        return Vec::new();
    }
    if let Some(rest) = text.strip_prefix("warning:") {
        return vec![Note::Warning(rest.trim().to_owned())];
    }
    match ev.level {
        LVL_ERROR => vec![Note::Error(condense(
            text.strip_prefix("error:").unwrap_or(&text).trim(),
        ))],
        LVL_WARN => vec![Note::Warning(text)],
        _ => Vec::new(),
    }
}

fn retry(text: &str) -> Note {
    let what = text
        .split('\'')
        .nth(1)
        .filter(|u| u.starts_with("http"))
        .unwrap_or("a download")
        .to_owned();
    let why = text
        .split_once("(curl error: ")
        .and_then(|(_, r)| r.split_once(')'))
        .map(|(w, _)| w.to_owned())
        .or_else(|| {
            text.find("HTTP error ").map(|i| {
                text[i..]
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
        .unwrap_or_else(|| "network error".to_owned());
    Note::Retry { what, why }
}

/// Cuts a Nix error down to what went wrong: the innermost error of an
/// evaluation trace, without the paragraphs on how to configure Nix.
pub fn condense(msg: &str) -> String {
    let mut msg = msg;
    if let Some(i) = msg.rfind("error: ")
        && msg[..i].contains('…')
    {
        msg = &msg[i + "error: ".len()..];
    }
    if let Some(i) = msg.find("\n\n") {
        msg = &msg[..i];
    }
    let lines: Vec<&str> = msg.lines().map(str::trim).collect();
    let first = lines.first().copied().unwrap_or_default();
    if let Some(rest) = first.strip_prefix("Refusing to evaluate package '")
        && let Some((name, why)) = rest.split_once("' in ")
        && let Some((_, why)) = why.split_once(" because it ")
    {
        let why = why.replace(['‘', '’'], "");
        return format!("{name} {}", why.trim_end_matches('.'));
    }
    lines.join("\n")
}

/// The error a failed `nix` run reports: its error-level messages, or its
/// plain stderr when it wasn't logging JSON.
pub fn error_text(lines: &[String]) -> String {
    let mut errors = Vec::new();
    let mut plain = Vec::new();
    for l in lines {
        match Event::parse(l) {
            Some(ev) if ev.action == "msg" && ev.level == LVL_ERROR => {
                let text = strip_ansi(&ev.msg);
                if !text.starts_with("error (ignored):") {
                    errors.push(condense(
                        text.strip_prefix("error:").unwrap_or(&text).trim(),
                    ));
                }
            }
            Some(_) => {}
            None => plain.push(strip_ansi(l)),
        }
    }
    if !errors.is_empty() {
        return errors.join("\n");
    }
    let text = plain.join("\n");
    let text = text.trim();
    let text = match text.rfind("error:") {
        Some(i) => &text[i + "error:".len()..],
        None => text,
    };
    let text = condense(text.trim());
    if text.is_empty() {
        "nix failed without saying why".to_owned()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes(log: &str) -> Vec<Note> {
        let mut t = Tracker::new();
        log.lines().flat_map(|l| t.line(l)).collect()
    }

    #[test]
    fn a_substitution_is_one_fetched_note_with_its_size() {
        // Nix 2.35 realising hyperfine (M6 spike).
        let log = include_str!("../../../testdata/nix-build-storepath.jsonl");
        assert_eq!(
            notes(log),
            [Note::Fetched {
                name: "hyperfine-2.0.0".into(),
                size: 720_696
            }]
        );
    }

    #[test]
    fn nx_0x_fixtures_still_read() {
        let log = include_str!("../../../testdata/fetch-mosh.jsonl");
        let n = notes(log);
        let fetched = n
            .iter()
            .filter(|n| matches!(n, Note::Fetched { .. }))
            .count();
        assert_eq!(fetched, 15, "{n:?}");
        // "error (ignored)" is dropped.
        assert!(!n.iter().any(|n| matches!(n, Note::Error(_))), "{n:?}");
    }

    #[test]
    fn builds_retries_and_errors() {
        let log = r#"@nix {"action":"start","id":1,"level":3,"parent":0,"text":"building","type":105,"fields":["/nix/store/w44icb02v5qg6qj4cjmia2f6520kx1w1-neovim-0.12.6.drv","",1,1]}
@nix {"action":"stop","id":1}
@nix {"action":"msg","level":0,"msg":"warning: error: unable to download 'https://cache.nixos.org/nar/x.nar.zst': Couldn't connect to server (curl error: Couldn't connect to server); retrying in 281 ms"}
@nix {"action":"msg","level":1,"msg":"warning: unable to download 'https://cache.nixos.org/nar/y.nar.zst': HTTP error 206 (curl error: Failure when receiving data from the peer); retrying from offset 78754907 in 169 ms (attempt 2/5)"}
@nix {"action":"msg","level":0,"msg":"\u001b[31;1merror:\u001b[0m path '/nix/store/00000000000000000000000000000000-nothing-1.0' is required, but there is no substituter that can build it"}
not json"#;
        assert_eq!(
            notes(log),
            [
                Note::Building {
                    name: "neovim-0.12.6".into()
                },
                Note::Built {
                    name: "neovim-0.12.6".into()
                },
                Note::Retry {
                    what: "https://cache.nixos.org/nar/x.nar.zst".into(),
                    why: "Couldn't connect to server".into()
                },
                Note::Retry {
                    what: "https://cache.nixos.org/nar/y.nar.zst".into(),
                    why: "Failure when receiving data from the peer".into()
                },
                Note::Error(
                    "path '/nix/store/00000000000000000000000000000000-nothing-1.0' is required, but there is no substituter that can build it"
                        .into()
                ),
                Note::Raw("not json".into()),
            ]
        );
    }

    #[test]
    fn a_log_nx_does_not_know_loses_notes_not_the_install() {
        // What a newer Nix might write: an unknown action, activity and
        // result type, new fields, a retyped field, a msg without a level,
        // and lines that aren't JSON or an object.
        let log = r#"@nix {"action":"phase","id":9,"name":"unpack"}
@nix {"action":"start","id":2,"level":3,"parent":0,"text":"","type":142,"fields":[{"new":true}],"extra":1}
@nix {"action":"start","id":3,"level":3,"parent":0,"text":"building","type":105,"fields":[{"drvPath":"/nix/store/x-y.drv"}]}
@nix {"action":"result","id":2,"type":199,"fields":"no longer a list"}
@nix {"action":"stop","id":2}
@nix {"action":"stop","id":3}
@nix {"action":"start","id":"4","level":3,"parent":0,"type":108,"fields":["/nix/store/5m5ri3ilhw2ihz9p6icdkzs5vbjm3a6b-jq-1.8.2","https://cache.nixos.org"]}
@nix {"action":"msg","msg":"something new"}
@nix {"action":"msg","level":"error","msg":"also new"}
@nix ["a","list"]
@nix {"truncated":
@nix
"#;
        let mut lines: Vec<String> = log.lines().map(str::to_owned).collect();
        assert_eq!(notes(log), [], "every line is skipped");
        // A substitution after them still reads.
        let ok = include_str!("../../../testdata/nix-build-storepath.jsonl");
        assert_eq!(notes(&format!("{log}{ok}")).len(), 1);
        // A failed run with only unreadable lines says so plainly.
        assert_eq!(error_text(&lines), "nix failed without saying why");
        lines.push("error: plain words".into());
        assert_eq!(error_text(&lines), "plain words");
    }

    #[test]
    fn condenses_traces_and_refusals() {
        let trace = "… while evaluating\n\n       error: Refusing to evaluate package 'vscode-1.106' in /nix/store/x/default.nix:1 because it has an unfree license (‘unfree’), refusing to evaluate.\n\n       a) To temporarily allow";
        assert_eq!(
            condense(trace),
            "vscode-1.106 has an unfree license (unfree), refusing to evaluate"
        );
        let plain = [
            "error:".to_owned(),
            "       attribute 'nope' missing".to_owned(),
        ];
        assert_eq!(error_text(&plain), "attribute 'nope' missing");
    }
}
