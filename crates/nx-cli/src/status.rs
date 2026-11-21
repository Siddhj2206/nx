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
    let (st, verbose) = (s.style, s.verbose);
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
                if let Some(note) = host_tier_for(state, d, st, verbose) {
                    value.push_str(&format!("  {note}"));
                }
                value
            }
        };
        s.row(key, &value);
    }
    s.row(
        "Host tier",
        &(host_tier(state, root, st, verbose) + &not_rebuilt(state, booted, st)),
    );
    s.row("Tool tier", &tool_tier(state, root, st));
    s.blank();
    Ok(())
}

/// Whether an image that isn't booted has its host tier ready (`-v` says
/// which generation). Nothing is said until nx has built a generation at
/// all.
fn host_tier_for(
    state: &State,
    d: &Deployment,
    st: crate::ui::Style,
    verbose: bool,
) -> Option<String> {
    let digest = d.image_digest.as_deref()?;
    if state.generations().ok()?.is_empty() {
        return None;
    }
    // What the last sync said about this image wins over an empty
    // placeholder from boot.
    // The record holds the failure's headline; `nx sync` tries again and
    // shows the whole problem.
    if let Some(why) = nx_core::select::failures(state).get(digest) {
        return Some(
            st.yellow(&format!("no host tier yet: {why}")) + &st.dim(" · nx sync says why"),
        );
    }
    Some(match nx_core::select::newest_for(state, digest) {
        Ok(Some(n)) => {
            let ready = if verbose {
                format!("host tier: generation {n}")
            } else {
                "host tier ready".to_owned()
            };
            st.dim(&ready) + &left_out(state, n, st) + &not_rebuilt(state, d, st)
        }
        Ok(None) => st.yellow("no host tier yet (nx sync)"),
        Err(e) => st.yellow(&format!("host tier unreadable: {e}")),
    })
}

/// Whether nxd made the current generation live (the link to root's copy
/// is there); `-v` says which generation.
fn host_tier(state: &State, root: &Path, st: crate::ui::Style, verbose: bool) -> String {
    let live = std::fs::symlink_metadata(root.join("var/lib/extensions/nx")).is_ok();
    match state.current_number() {
        Ok(Some(n)) => {
            let mut out = if live { "live" } else { "built, not made live" }.to_owned();
            if verbose {
                out.push_str(&st.dim(&format!(" · generation {n}")));
            }
            out + &left_out(state, n, st)
        }
        Ok(None) => st.dim("nothing added yet"),
        Err(e) => st.yellow(&format!("unreadable: {e}")),
    }
}

/// `  not rebuilt, keeps generation N: …` when `nx sync` couldn't rebuild the generation an
/// older nx built for this image.
fn not_rebuilt(state: &State, d: &Deployment, st: crate::ui::Style) -> String {
    d.image_digest
        .as_deref()
        .and_then(|digest| nx_core::select::rebuild_failures(state).remove(digest))
        .map(|why| format!("  {}", st.yellow(&format!("not rebuilt, {why}"))))
        .unwrap_or_default()
}

/// The current generation's nixpkgs release and how many tools it has.
fn tool_tier(state: &State, root: &Path, st: crate::ui::Style) -> String {
    if !root.join("nix/store").is_dir() {
        return st.yellow("off: the image has no Nix (/nix/store)");
    }
    let tool = state
        .current_number()
        .ok()
        .flatten()
        .and_then(|n| nx_core::select::read_lock(state, n).ok())
        .and_then(|l| l.tool)
        .filter(|t| !t.packages.is_empty());
    match tool {
        Some(t) => {
            let release = crate::tool::pinned(&t);
            format!(
                "{} {}",
                crate::ui::plural(t.packages.len(), "tool"),
                st.dim(&format!("· nixpkgs {}", release.version()))
            )
        }
        None => st.dim("nothing added yet"),
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
        std::fs::create_dir_all(dir.join("nix/store"))?;
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
        render_with(tag, rollback_release, false, |_, _| Ok(()))
    }

    /// [`render`], after `setup` fills the state and root.
    fn render_with(
        tag: &str,
        rollback_release: &str,
        verbose: bool,
        setup: impl FnOnce(&State, &Path) -> Result<()>,
    ) -> Result<String> {
        let root = fake_root(tag, rollback_release)?;
        let out = SharedBuf::default();
        let mut ui = Ui::plain(out.clone(), SharedBuf::default());
        ui.set_verbose(verbose);
        let state = State::new(root.join("var-lib-nx"));
        setup(&state, &root)?;
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
  Tool tier   nothing added yet

"
        );
        Ok(())
    }

    /// A live generation 3, and a failed sync for the rollback image.
    fn built(state: &State, root: &Path) -> Result<()> {
        std::fs::create_dir_all(state.generation_dir(3))?;
        state.set_current(3)?;
        std::fs::create_dir_all(root.join("var/lib/extensions"))?;
        std::os::unix::fs::symlink("/var/lib/nx/current", root.join("var/lib/extensions/nx"))?;
        let deployments = Deployments::from_json(STATUS)?;
        let rollback = deployments
            .rollback()
            .and_then(|d| d.image_digest.clone())
            .context("the fixture has a rollback image")?;
        let failed = [(rollback, "can't resolve foo".to_owned())].into();
        nx_core::select::record_failures(state, &failed);
        Ok(())
    }

    #[test]
    fn says_live_and_points_at_sync_for_a_failure() -> Result<()> {
        assert_eq!(
            render_with("built", "45", false, built)?,
            "
  ghcr.io/siddhj2206/neptuno:latest 45.20261118
  Neptuno 45 · booted from sha256:92409d3fcf84

  Staged      none
  Rollback    45.20261117 from sha256:292ee1bfc6d4  no host tier yet: can't resolve foo · nx sync says why
  Host tier   live
  Tool tier   nothing added yet

"
        );
        // -v names the generation.
        assert!(
            render_with("built-v", "45", true, built)?
                .contains("  Host tier   live · generation 3\n")
        );
        Ok(())
    }

    #[test]
    fn flags_a_release_change() -> Result<()> {
        assert!(render("change", "44")?.contains("from sha256:292ee1bfc6d4  → fedora 44"));
        Ok(())
    }
}
