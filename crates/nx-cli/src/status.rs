//! `nx status`: the image nx resolves against, laid out like nx 0.x's `info`
//! (a bold head, then dim keys padded to one column).

use std::path::Path;

use anyhow::{Context, Result};
use nx_core::{Deployment, DeploymentSource, OsRelease, State};

use crate::ui::Ui;

pub fn run(
    ui: &mut Ui,
    source: &dyn DeploymentSource,
    state: &State,
    root: &Path,
    sysroot: &Path,
) -> Result<()> {
    let deployments = source
        .deployments()
        .context("reading deployments (is this a bootc system?)")?;
    let booted = deployments.booted().context("no booted deployment")?;
    let release = OsRelease::read(root).context("reading the booted os-release")?;

    let s = &mut ui.out;
    let st = s.style;
    s.blank();
    let image = booted.image().unwrap_or("not a container image");
    let version = booted.version.as_deref().unwrap_or("");
    s.line(format_args!("{} {}", st.bold(image), st.dim(version)));
    s.line(format_args!(
        "{} {} {}",
        release.get("NAME").unwrap_or(&release.id),
        release.version_id,
        st.dim(&format!("· booted from {}", short_digest(booted))),
    ));
    s.blank();

    for (key, deployment) in [
        ("Staged", deployments.staged()),
        ("Rollback", deployments.rollback()),
    ] {
        let value = match deployment {
            None => st.dim("none"),
            Some(d) => {
                // Another deployment's os-release says whether it moves to a
                // new Fedora release; unreadable only drops that detail.
                let other = OsRelease::read(&d.root(sysroot)).ok();
                let mut value = format!(
                    "{} {}",
                    d.version.as_deref().unwrap_or("unknown version"),
                    st.dim(&format!("from {}", short_digest(d))),
                );
                if let Some(other) = other.filter(|o| o.version_id != release.version_id) {
                    let arrow = format!("→ {} {}", other.id, other.version_id);
                    value.push_str(&format!("  {}", st.yellow(&arrow)));
                }
                if d.pinned {
                    value.push_str(&format!("  {}", st.dim("pinned")));
                }
                if let Some(note) = host_tier_for(state, d, st) {
                    value.push_str(&format!("  {note}"));
                }
                value
            }
        };
        s.row(key, &value);
    }
    s.row("Host tier", &host_tier(state, st));
    s.row("Tool tier", &st.dim("not built yet (M6)"));
    s.blank();
    Ok(())
}

/// Whether an image that isn't booted has its host tier ready. Nothing is
/// said until nx has built a generation at all.
fn host_tier_for(state: &State, d: &Deployment, st: crate::ui::Style) -> Option<String> {
    let digest = d.image_digest.as_deref()?;
    if state.generations().ok()?.is_empty() {
        return None;
    }
    Some(match nx_core::select::newest_for(state, digest) {
        Ok(Some(n)) => {
            let left_out = nx_core::select::read_lock(state, n)
                .map(|l| l.skipped)
                .unwrap_or_default();
            if left_out.is_empty() {
                st.dim(&format!("host tier: generation {n}"))
            } else {
                st.yellow(&format!(
                    "host tier: generation {n}, without {}",
                    left_out.join(", ")
                ))
            }
        }
        Ok(None) => match nx_core::select::failures(state).get(digest) {
            Some(why) => st.yellow(&format!("no host tier yet: {why}")),
            None => st.yellow("no host tier yet (nx sync)"),
        },
        Err(e) => st.yellow(&format!("host tier unreadable: {e}")),
    })
}

/// The current generation and the backend that made it live.
fn host_tier(state: &State, st: crate::ui::Style) -> String {
    match (state.current_number(), state.backend()) {
        (Ok(Some(n)), Ok(Some(backend))) => format!(
            "generation {n} · {}{}",
            backend.name(),
            left_out(state, n, st)
        ),
        (Ok(Some(n)), _) => format!(
            "generation {n} {}{}",
            st.dim("· not made live"),
            left_out(state, n, st)
        ),
        (Ok(None), _) => st.dim("nothing added yet"),
        (Err(e), _) => st.yellow(&format!("unreadable: {e}")),
    }
}

/// `  without a, b` when the generation leaves manifest packages out.
fn left_out(state: &State, n: u32, st: crate::ui::Style) -> String {
    match nx_core::select::read_lock(state, n) {
        Ok(lock) if !lock.skipped.is_empty() => {
            st.yellow(&format!("  without {}", lock.skipped.join(", ")))
        }
        _ => String::new(),
    }
}

fn short_digest(d: &Deployment) -> &str {
    d.image_digest
        .as_deref()
        .map_or("no digest", crate::ui::short_digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;
    use nx_core::Deployments;

    const STATUS: &str = include_str!("../../nx-core/tests/fixtures/rpm-ostree-status.json");

    /// A fake `/` (and `/sysroot`) whose os-release says Fedora 45, and whose
    /// rollback deployment says `rollback_release`.
    fn fake_root(tag: &str, rollback_release: &str) -> std::io::Result<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("nx-status-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("usr/lib"))?;
        std::fs::write(
            dir.join("usr/lib/os-release"),
            "NAME=\"Neptuno\"\nID=fedora\nVERSION_ID=45\n",
        )?;
        let rollback = dir.join(
            "ostree/deploy/default/deploy/30e1f7c3f36696120b98a48861728ab6840206364bc44310bfce3ca165e6bb30.0/usr/lib",
        );
        std::fs::create_dir_all(&rollback)?;
        std::fs::write(
            rollback.join("os-release"),
            format!("ID=fedora\nVERSION_ID={rollback_release}\n"),
        )?;
        Ok(dir)
    }

    fn render(tag: &str, rollback_release: &str) -> Result<String> {
        let root = fake_root(tag, rollback_release)?;
        let out = SharedBuf::default();
        let mut ui = Ui::plain(out.clone(), SharedBuf::default());
        let state = State::new(root.join("var-lib-nx"));
        let result = run(
            &mut ui,
            &Deployments::from_json(STATUS)?,
            &state,
            &root,
            &root,
        );
        let _ = std::fs::remove_dir_all(&root);
        result.map(|()| out.text())
    }

    #[test]
    fn renders_like_info() -> Result<()> {
        assert_eq!(
            render("same", "45")?,
            "
  ghcr.io/siddhj2206/neptuno:latest 45.20261118
  Neptuno 45 · booted from sha256:92409d3fcf84

  Staged      none
  Rollback    45.20261117 from sha256:292ee1bfc6d4
  Host tier   nothing added yet
  Tool tier   not built yet (M6)

"
        );
        Ok(())
    }

    #[test]
    fn flags_a_release_change() -> Result<()> {
        assert!(render("change", "44")?.contains("from sha256:292ee1bfc6d4  → fedora 44"));
        Ok(())
    }
}
