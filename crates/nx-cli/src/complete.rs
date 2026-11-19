//! Shell completion and man pages, both from clap's definitions. The shell
//! asks `nx` itself for completions (`COMPLETE=fish nx`, clap_complete's
//! dynamic engine), so values are live: what is installed for `remove`,
//! the pinned release's nixpkgs attributes for `add`, the added repos and
//! caches for their `remove`. Each reads one small file; none loads the
//! repos.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::CompletionCandidate;
use nx_core::{Generation, Manifest};

use crate::Cli;

/// Answers the shell when `COMPLETE` is set, and exits; otherwise returns.
pub fn answer() {
    // The registration script calls `nx` from PATH, not the path it was
    // generated with (the release build writes it from its own tree).
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .completer("nx")
        .complete();
}

fn manifest() -> Manifest {
    Generation::readable(&crate::host::state())
        .ok()
        .and_then(|s| s.generation)
        .map(|g| g.manifest)
        .unwrap_or_default()
}

fn candidates(items: impl IntoIterator<Item = String>) -> Vec<CompletionCandidate> {
    items.into_iter().map(CompletionCandidate::new).collect()
}

/// `nx remove`: what `nx list` shows, as it was added.
pub fn installed() -> Vec<CompletionCandidate> {
    let m = manifest();
    candidates(m.host.packages.into_iter().chain(m.tool.packages))
}

/// `nx repo remove`: the repos nx added.
pub fn repos() -> Vec<CompletionCandidate> {
    candidates(manifest().repos.into_iter().map(|r| r.name))
}

/// `nx cache remove`: the caches nx added.
pub fn caches() -> Vec<CompletionCandidate> {
    candidates(manifest().caches.into_iter().map(|c| c.url))
}

/// `nx add`: attributes of the pinned release's catalog that start with
/// what is typed (two characters at least; package sets only once a `.`
/// is typed). RPM names would need the repos loaded, which is too slow
/// for a key press.
pub fn packages(typed: &OsStr) -> Vec<CompletionCandidate> {
    let Some(typed) = typed.to_str().filter(|t| t.len() >= 2) else {
        return Vec::new();
    };
    let Some(tool) = Generation::readable(&crate::host::state())
        .ok()
        .and_then(|s| s.generation)
        .and_then(|g| g.lock.tool)
    else {
        return Vec::new();
    };
    let Ok(cache) = crate::host::cache_dir() else {
        return Vec::new();
    };
    let path = nx_nix::catalog::path(&cache.join("nix"), &crate::tool::pinned(&tool));
    let Ok(catalog) = nx_nix::Catalog::load(&path) else {
        return Vec::new();
    };
    matching(&catalog, typed)
}

fn matching(catalog: &nx_nix::Catalog, typed: &str) -> Vec<CompletionCandidate> {
    let nested = typed.contains('.');
    catalog
        .packages
        .iter()
        .filter(|p| p.attr.starts_with(typed) && (nested || !p.nested()))
        .take(200)
        .map(|p| CompletionCandidate::new(&p.attr).help(Some(p.description.clone().into())))
        .collect()
}

/// `nx manpages <dir>` (hidden; the release build runs it): `nx.1` and one
/// page per visible command.
pub fn manpages(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut cmd = Cli::command();
    cmd.build();
    clap_mangen::generate_to(cmd, dir).with_context(|| format!("writing to {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_top_level_attributes_by_prefix() {
        let package = |attr: &str| nx_nix::catalog::Package {
            attr: attr.into(),
            ..Default::default()
        };
        let catalog = nx_nix::Catalog {
            packages: vec![
                package("python3Packages.ripgrep"),
                package("ripgrep"),
                package("ripgrep-all"),
                package("rsync"),
            ],
            ..Default::default()
        };
        let names = |typed: &str| -> Vec<String> {
            matching(&catalog, typed)
                .iter()
                .map(|c| c.get_value().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(names("rip"), ["ripgrep", "ripgrep-all"]);
        assert_eq!(names("python3Packages."), ["python3Packages.ripgrep"]);
    }

    #[test]
    fn writes_a_page_per_visible_command() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-man-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        manpages(&dir)?;
        let exists = |name: &str| dir.join(name).exists();
        let (top, add, select) = (exists("nx.1"), exists("nx-add.1"), exists("nx-select.1"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(top && add, "nx.1 and nx-add.1");
        assert!(!select, "no page for a hidden command");
        Ok(())
    }
}
