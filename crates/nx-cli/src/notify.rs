//! `nx notify` (hidden; `nx-notify.service`, a user unit that
//! `nx-notify.path` starts when the state directory changes): tells the
//! session what nx's background jobs couldn't do. The root services have no
//! session to talk to, so they leave a record (`sync-failures.json`, a
//! generation's `skipped`, `upgrade-failure.json`) and this reads it. Each
//! note is sent once; one that clears and comes back is sent again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use nx_core::{DeploymentSource, Deployments, State};

use crate::problems::Problem;

use crate::ui::Ui;

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub summary: String,
    pub body: String,
}

impl Note {
    /// One line of the seen file.
    fn key(&self) -> String {
        format!("{}\t{}", self.summary, self.body).replace('\n', " ")
    }
}

/// Shows a notification in the user's session.
pub trait Notifier {
    fn send(&self, note: &Note) -> Result<()>;
}

/// `org.freedesktop.Notifications` on the session bus, through `busctl`
/// (systemd's, so every image has it).
pub struct Busctl;

impl Notifier for Busctl {
    fn send(&self, note: &Note) -> Result<()> {
        let out = Command::new("busctl")
            .args([
                "--user",
                // The expiry, -1, would read as an option.
                "--",
                "call",
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "Notify",
                "susssasa{sv}i",
                "nx",
                "0",
                "dialog-warning",
                &note.summary,
                &note.body,
                "0",
                "0",
                "-1",
            ])
            .output()
            .context("running busctl")?;
        if !out.status.success() {
            bail!(
                "busctl: {}",
                String::from_utf8_lossy(&out.stderr).trim().to_owned()
            );
        }
        Ok(())
    }
}

pub fn run(
    ui: &mut Ui,
    state: &State,
    source: &dyn DeploymentSource,
    notifier: &dyn Notifier,
    seen_file: &Path,
) -> Result<()> {
    // Without the deployments the notes still go out, just less specific.
    let deployments = source.deployments().ok();
    let notes = notes(state, deployments.as_ref());
    let seen = read_seen(seen_file);
    let mut kept = BTreeSet::new();
    for note in notes {
        let key = note.key();
        if !seen.contains(&key) {
            if let Err(e) = notifier.send(&note) {
                // Not marked seen, so the next change tries again.
                ui.warn(format_args!("couldn't notify ({e:#}): {}", note.summary));
                continue;
            }
            ui.step(format_args!("notified: {}", note.summary));
        }
        kept.insert(key);
    }
    write_seen(seen_file, &kept)
}

/// What the records say now, one note per problem.
pub fn notes(state: &State, deployments: Option<&Deployments>) -> Vec<Note> {
    crate::problems::problems(state, deployments)
        .into_iter()
        .map(|p| match p {
            Problem::NoHostTier { image, why, .. } => Note {
                summary: format!("nx couldn't set up packages for {image}"),
                body: format!("{why}. nx status has the details."),
            },
            Problem::LeftOut { image, packages } => Note {
                summary: format!("nx left packages out for {image}"),
                body: format!(
                    "{} didn't resolve for it; nx tries again daily.",
                    packages.join(", ")
                ),
            },
            Problem::UpgradeFailed(why) => Note {
                summary: "nx couldn't upgrade your tools".to_owned(),
                body: format!("{why}. Run nx upgrade to see more."),
            },
        })
        .collect()
}

/// `$XDG_STATE_HOME/nx/notified`, else `~/.local/state/nx/notified`.
pub fn seen_file() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(
            std::env::var_os("HOME").context("neither XDG_STATE_HOME nor HOME is set")?,
        )
        .join(".local/state"),
    };
    Ok(dir.join("nx/notified"))
}

fn read_seen(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn write_seen(path: &Path, seen: &BTreeSet<String>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text: String = seen.iter().map(|k| format!("{k}\n")).collect();
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use nx_core::select;

    use crate::ui::SharedBuf;

    const STATUS: &str = include_str!("../../nx-core/tests/fixtures/rpm-ostree-status.json");

    struct Recorder(RefCell<Vec<String>>);

    impl Notifier for Recorder {
        fn send(&self, note: &Note) -> Result<()> {
            self.0.borrow_mut().push(note.summary.clone());
            Ok(())
        }
    }

    #[test]
    fn sends_each_note_once() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-notify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let seen = dir.join("home/nx/notified");
        let state = State::new(dir.join("state"));
        std::fs::create_dir_all(state.root())?;
        let deployments = Deployments::from_json(STATUS)?;
        let staged = deployments
            .rollback()
            .and_then(|d| d.image_digest.clone())
            .unwrap_or_default();
        select::record_failures(&state, &BTreeMap::from([(staged, "no gcc".to_owned())]));
        select::record_upgrade_failure(&state, Some("no network"));

        let sent = Recorder(RefCell::new(Vec::new()));
        let mut ui = Ui::plain(SharedBuf::default(), SharedBuf::default());
        run(&mut ui, &state, &deployments, &sent, &seen)?;
        assert_eq!(
            *sent.0.borrow(),
            [
                "nx couldn't set up packages for the rollback image",
                "nx couldn't upgrade your tools"
            ]
        );
        // Nothing new: nothing sent.
        run(&mut ui, &state, &deployments, &sent, &seen)?;
        assert_eq!(sent.0.borrow().len(), 2);
        // Cleared, then back: sent again.
        select::record_upgrade_failure(&state, None);
        run(&mut ui, &state, &deployments, &sent, &seen)?;
        select::record_upgrade_failure(&state, Some("no network"));
        run(&mut ui, &state, &deployments, &sent, &seen)?;
        assert_eq!(sent.0.borrow().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
