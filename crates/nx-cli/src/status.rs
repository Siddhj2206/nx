//! `nx status`: the image nx resolves against, laid out like nx 0.x's `info`
//! (a bold head, then dim keys padded to one column).

use std::path::Path;

use anyhow::{Context, Result};
use nx_core::{Deployment, DeploymentSource, OsRelease};

use crate::ui::Ui;

pub fn run(ui: &mut Ui, source: &dyn DeploymentSource, root: &Path, sysroot: &Path) -> Result<()> {
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
                value
            }
        };
        s.row(key, &value);
    }
    s.row(
        "Host tier",
        &st.dim("builds generations (nx add); activates from M3"),
    );
    s.row("Tool tier", &st.dim("not built yet (M6)"));
    s.blank();
    Ok(())
}

/// `sha256:92409d3fcf8435…` → `sha256:92409d3fcf84`, as short as nx 0.x
/// showed nixpkgs revisions, long enough to tell images apart.
fn short_digest(d: &Deployment) -> &str {
    let Some(digest) = d.image_digest.as_deref() else {
        return "no digest";
    };
    let end = digest.find(':').map_or(12, |colon| colon + 13);
    digest.get(..end).unwrap_or(digest)
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
        let result = run(&mut ui, &Deployments::from_json(STATUS)?, &root, &root);
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
  Host tier   builds generations (nx add); activates from M3
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
