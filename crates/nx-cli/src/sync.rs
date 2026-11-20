//! `nx sync`: gives every deployed image (booted, staged, rollback) a
//! generation that matches the manifest (design invariants 5 and 6).
//!
//! An update staged by `bootc upgrade` is re-resolved before the reboot, so
//! the host tier is waiting when it boots; the rollback image keeps one so
//! `bootc rollback` finds its host tier. Only the booted image's generation
//! is made live. The others are built from the image's own tree under
//! `/sysroot`, which is readable without root.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use nx_core::select;
use nx_core::{Deployment, Generation};
use nx_rpm::Refresh;

use crate::host::{self, Machine};
use crate::session::{Session, Unresolvable};
use crate::ui::Ui;

/// `nx sync [--refresh]`.
pub fn run(ui: &mut Ui, machine: &Machine, refresh: Refresh) -> Result<()> {
    if refresh != Refresh::IfMissing {
        // The refresh timer: fill the cache first, so what follows (and the
        // next `nx add`) answers from it.
        let manifest = Generation::current(&host::state())
            .ok()
            .flatten()
            .map(|g| g.manifest)
            .unwrap_or_default();
        host::load(ui, machine, refresh, &manifest)?;
    }
    let built = deployed(ui, machine, &host::state(), refresh)?;
    // Generations nothing deployed needs any more (an image that is gone,
    // older staged ones). Only after every image has its own: a failure
    // above returned already. The daily refresh collects the Nix store too.
    let removed = crate::gc::collect(ui, machine, refresh != Refresh::IfMissing)?;
    if built == 0 && removed == 0 {
        ui.done("every deployed image has a host tier that matches the manifest");
    }
    Ok(())
}

/// Which deployment an image is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Booted,
    Staged,
    Rollback,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Role::Booted => "booted",
            Role::Staged => "staged",
            Role::Rollback => "rollback",
        })
    }
}

/// What a deployed image needs: its role and the deployment. Only the
/// booted image's generation is made live.
fn wanted(machine: &Machine) -> Vec<(Role, Deployment)> {
    let Some(deployments) = machine.deployments() else {
        return Vec::new();
    };
    let mut out: Vec<(Role, Deployment)> = Vec::new();
    for (role, d) in [
        (Role::Booted, deployments.booted()),
        (Role::Staged, deployments.staged()),
        (Role::Rollback, deployments.rollback()),
    ] {
        let Some(d) = d else { continue };
        // A digest shared by two deployments needs one generation.
        if d.image_digest.is_some()
            && !out
                .iter()
                .any(|(_, other)| other.image_digest == d.image_digest)
        {
            out.push((role, d.clone()));
        }
    }
    out
}

/// Builds a generation for each deployed image that lacks one for the
/// current manifest; returns how many it built. Every image is tried; the
/// first failure is returned once the rest are done.
pub fn deployed(
    ui: &mut Ui,
    machine: &Machine,
    state: &nx_core::State,
    refresh: Refresh,
) -> Result<usize> {
    let current = match Generation::current(state) {
        Ok(Some(g)) => g,
        // Nothing added yet: nothing to carry across images.
        Ok(None) => return Ok(0),
        Err(e) => return Err(anyhow::Error::new(e).context("reading the current generation")),
    };
    let (mut built, mut first_error) = (0, None);
    let mut failures = BTreeMap::new();
    for (role, d) in wanted(machine) {
        let Some(digest) = d.image_digest.as_deref() else {
            continue;
        };
        if matches(state, digest, &current, refresh)? {
            continue;
        }
        let result = build(ui, machine, role, &d, refresh);
        match result {
            Ok(()) => built += 1,
            Err(e) => {
                ui.warn(format_args!(
                    "no host tier for the {role} image ({}): {e:#}",
                    crate::ui::short_digest(digest)
                ));
                failures.insert(digest.to_owned(), first_line(&e));
                first_error.get_or_insert(e);
            }
        }
    }
    // Only this run's failures stand: an image that now has a host tier, or
    // is no longer deployed, drops out.
    select::record_failures(state, &failures);
    match first_error {
        Some(e) => Err(e.context("some deployed images have no host tier yet")),
        None => Ok(built),
    }
}

/// Whether the newest generation for `digest` was built from the
/// manifest `current` has. One that left packages out is tried again only
/// when the metadata is being refreshed: that is when they might resolve.
fn matches(
    state: &nx_core::State,
    digest: &str,
    current: &Generation,
    refresh: Refresh,
) -> Result<bool> {
    let Some(n) = select::newest_for(state, digest)? else {
        return Ok(false);
    };
    // The empty generation `nx select` made at boot is a placeholder, not a
    // host tier.
    let lock = select::read_lock(state, n)?;
    if lock.pending
        || (!lock.skipped.is_empty() && matches!(refresh, Refresh::IfStale | Refresh::Always))
    {
        return Ok(false);
    }
    if n == current.number {
        return Ok(true);
    }
    let g = Generation::read(state, n).with_context(|| format!("reading generation {n}"))?;
    Ok(g.manifest == current.manifest
        && crate::tool::same(g.lock.tool.as_ref(), current.lock.tool.as_ref()))
}

fn build(
    ui: &mut Ui,
    machine: &Machine,
    role: Role,
    d: &Deployment,
    refresh: Refresh,
) -> Result<()> {
    let digest = d.image_digest.as_deref().unwrap_or_default();
    let version = d.version.as_deref().unwrap_or("unknown version");
    ui.step(format_args!(
        "resolving the host tier for the {role} image {version} ({})",
        crate::ui::short_digest(digest)
    ));
    let booted = role == Role::Booted;
    let target;
    let machine = if booted {
        machine
    } else {
        target = machine.targeting(d)?;
        &target
    };
    let mut session = Session::open(ui, machine, refresh, false, |_| Ok(()))?;
    let was_current = session.current().map(|g| g.number);
    let manifest = session.manifest().clone();
    let (resolved, skipped) = resolve_what_fits(ui, &mut session, &manifest.host.packages)?;
    if !skipped.is_empty() {
        // A repeat of the last attempt (the daily refresh, with the same
        // repos still lacking them) would only add a generation like it.
        if let Some(n) = select::newest_for(session.state(), digest)?
            && same_as(
                session.state(),
                n,
                &manifest,
                &resolved.all,
                &skipped,
                session.tool(),
            )?
            && (!booted || session.state().current_number()? == Some(n))
        {
            ui.step(format_args!(
                "still nothing for {}; generation {n} stays",
                skipped.join(", ")
            ));
            return Ok(());
        }
        session.leave_out(skipped);
    }
    let built = session.build(ui, &resolved.all)?;
    let number = built.generation.number;
    if booted {
        // The booted image had none (it was updated without nx noticing, or
        // the boot fell back to an older generation): this one replaces it.
        // The lock is gone by now, so a change made meanwhile (an `nx add`
        // that made its own generation current) wins: this one stays built
        // and the next sync sees the manifest differ.
        let state = host::state();
        if state.current_number()? != was_current {
            ui.warn(format_args!(
                "generation {number} is built, but the current generation changed meanwhile; not replacing it"
            ));
            return Ok(());
        }
        crate::live::activate(ui, &state, number)?;
    }
    ui.done(format_args!(
        "generation {number} for the {role} image{}",
        if booted { ", live" } else { "" }
    ));
    Ok(())
}

/// Resolves the manifest; if it won't resolve as a whole, resolves each
/// package alone and leaves out the ones that fail, with a warning. Better
/// that than no host tier for an image: the others still come up. If no
/// single package is at fault (two conflict), the original error stands.
fn resolve_what_fits(
    ui: &mut Ui,
    session: &mut Session,
    specs: &[String],
) -> Result<(crate::session::Resolved, Vec<String>)> {
    let whole = match session.resolve(ui, specs) {
        Ok(resolved) => return Ok((resolved, Vec::new())),
        Err(e) => e,
    };
    let (mut kept, mut skipped) = (Vec::new(), Vec::new());
    for spec in specs {
        match session.resolve(ui, std::slice::from_ref(spec)) {
            Ok(_) => kept.push(spec.clone()),
            // Only a problem with the package itself: not the network's.
            Err(e) if e.downcast_ref::<Unresolvable>().is_none() => return Err(e),
            Err(e) => {
                ui.warn(format_args!("leaving out {spec}: {}", first_line(&e)));
                skipped.push(spec.clone());
            }
        }
    }
    // Nothing fits: not a host tier, and no one package to blame.
    if skipped.is_empty() || kept.is_empty() {
        return Err(whole);
    }
    let resolved = session
        .resolve(ui, &kept)
        .context("the rest of the manifest doesn't resolve either")?;
    Ok((resolved, skipped))
}

fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}")
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Whether generation `n` is what building now would make: the same
/// manifest, packages and left-out ones.
fn same_as(
    state: &nx_core::State,
    n: u32,
    manifest: &nx_core::Manifest,
    all: &[nx_rpm::Planned],
    skipped: &[String],
    tool: Option<&nx_core::LockedTool>,
) -> Result<bool> {
    let lock = select::read_lock(state, n)?;
    let have: std::collections::BTreeSet<&str> =
        lock.host.iter().map(|p| p.nevra.as_str()).collect();
    let want: std::collections::BTreeSet<String> = all.iter().map(|p| p.package.nevra()).collect();
    // A manifest change that resolves to the same packages (a repo, a
    // scriptlet allowance) still makes a generation.
    let g = Generation::read(state, n)?;
    Ok(g.manifest == *manifest
        && crate::tool::same(g.lock.tool.as_ref(), tool)
        && lock.skipped == skipped
        && have.len() == want.len()
        && want.iter().all(|nevra| have.contains(nevra.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nx_core::Deployments;

    const STATUS: &str = include_str!("../../nx-core/tests/fixtures/rpm-ostree-status.json");

    #[test]
    fn every_deployed_image_is_wanted_once() -> Result<()> {
        let root = std::env::temp_dir().join(format!("nx-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("usr/lib"))?;
        std::fs::create_dir_all(root.join("run"))?;
        std::fs::write(root.join("run/ostree-booted"), "")?;
        std::fs::write(
            root.join("usr/lib/os-release"),
            "ID=fedora\nVERSION_ID=45\n",
        )?;
        let machine = Machine::read(&root, &Deployments::from_json(STATUS)?)?;
        let roles: Vec<Role> = wanted(&machine).iter().map(|(role, _)| *role).collect();
        assert_eq!(roles, [Role::Booted, Role::Rollback]);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn a_boot_placeholder_is_not_a_host_tier() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-sync-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = nx_core::State::new(&dir);
        let mut lock = nx_core::Lock::new(Some("sha256:new".into()), "45", "x86_64");
        let write = |n: u32, lock: &nx_core::Lock| -> Result<()> {
            let gen_dir = dir.join(format!("generations/{n}"));
            std::fs::create_dir_all(&gen_dir)?;
            std::fs::write(gen_dir.join("lock.toml"), lock.to_toml()?)?;
            Ok(())
        };
        let current = Generation {
            number: 2,
            dir: Default::default(),
            manifest: nx_core::Manifest::default(),
            lock: lock.clone(),
            activation: Default::default(),
        };
        lock.pending = true;
        write(2, &lock)?;
        // `nx select` made generation 2 current for the booted image, but it
        // is empty: sync must still resolve for the image.
        assert!(!matches(
            &state,
            "sha256:new",
            &current,
            Refresh::IfMissing
        )?);
        lock.pending = false;
        write(2, &lock)?;
        assert!(matches(&state, "sha256:new", &current, Refresh::IfMissing)?);
        // One that left a package out is retried when the metadata refreshes,
        // not on every sync.
        lock.skipped = vec!["copr-thing".into()];
        write(2, &lock)?;
        assert!(matches(&state, "sha256:new", &current, Refresh::IfMissing)?);
        assert!(!matches(&state, "sha256:new", &current, Refresh::Always)?);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
