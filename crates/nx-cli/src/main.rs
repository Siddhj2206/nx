//! The `nx` command.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nx_core::{Deployment, DeploymentSource, OsRelease, RpmOstree};

/// Packages for a bootc system: Fedora RPMs live on the image's /usr, and
/// sealed nixpkgs tools beside it.
#[derive(Parser)]
#[command(name = "nx", version, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the image nx resolves against: booted, staged and rollback.
    Status,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Status => status(&RpmOstree, Path::new("/"), Path::new("/sysroot")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("nx: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn status(source: &dyn DeploymentSource, root: &Path, sysroot: &Path) -> Result<()> {
    let deployments = source
        .deployments()
        .context("reading deployments (is this a bootc system?)")?;
    let booted = deployments.booted().context("no booted deployment")?;
    let release = OsRelease::read(root).context("reading the booted os-release")?;

    println!(
        "image     {}",
        booted.image().unwrap_or("(not a container image)")
    );
    println!("release   {} {}", release.id, release.version_id);
    println!("booted    {}", describe(booted, Some(&release)));
    for (label, deployment) in [
        ("staged", deployments.staged()),
        ("rollback", deployments.rollback()),
    ] {
        let line = match deployment {
            // Another deployment's os-release says whether it changes the
            // Fedora release; unreadable is fine, it only drops that detail.
            Some(d) => describe(d, OsRelease::read(&d.root(sysroot)).ok().as_ref()),
            None => "none".to_owned(),
        };
        println!("{label:<9} {line}");
    }
    println!("host tier not built yet (M1)");
    println!("tool tier not built yet (M6)");
    Ok(())
}

fn describe(deployment: &Deployment, release: Option<&OsRelease>) -> String {
    let digest = deployment
        .image_digest
        .as_deref()
        .map_or("no digest", short_digest);
    let mut parts = vec![digest.to_owned()];
    if let Some(version) = &deployment.version {
        parts.push(version.clone());
    }
    if let Some(release) = release {
        parts.push(format!("{} {}", release.id, release.version_id));
    }
    if deployment.pinned {
        parts.push("pinned".to_owned());
    }
    parts.join(" · ")
}

/// `sha256:92409d3fcf84…` → `sha256:92409d3fcf84`.
fn short_digest(digest: &str) -> &str {
    let end = digest.find(':').map_or(12, |colon| colon + 13);
    digest.get(..end).unwrap_or(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_digests() {
        assert_eq!(
            short_digest("sha256:92409d3fcf8435bc77e7339def63550887b5cdfde2302af8a0589c0305e7d2b8"),
            "sha256:92409d3fcf84"
        );
        assert_eq!(short_digest("sha256:abc"), "sha256:abc");
    }
}
