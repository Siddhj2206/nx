//! `nx sync`: gives every deployed image (booted, staged, rollback) a
//! generation that matches the manifest (design invariants 5 and 6).
//!
//! An update staged by `bootc upgrade` is re-resolved before the reboot, so
//! the host tier is waiting when it boots; the rollback image keeps one so
//! `bootc rollback` finds its host tier. Only the booted image's generation
//! is made live. The others are built from the image's own tree under
//! `/sysroot`, which is readable without root.

use anyhow::{Context, Result};
use nx_core::select;
use nx_core::{Deployment, Generation};
use nx_rpm::Refresh;

use crate::apply::Session;
use crate::host::{self, Machine};
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
    let built = deployed(ui, machine, refresh)?;
    // Generations nothing deployed needs any more (an image that is gone,
    // older staged ones). Only after every image has its own: a failure
    // above returned already.
    let removed = crate::gc::collect(ui, machine)?;
    if built == 0 && removed == 0 {
        ui.done("every deployed image has a host tier that matches the manifest");
    }
    Ok(())
}

/// What a deployed image needs: its role, the deployment, and whether its
/// generation is live.
fn wanted(machine: &Machine) -> Vec<(&'static str, Deployment)> {
    let Some(deployments) = machine.deployments() else {
        return Vec::new();
    };
    let mut out: Vec<(&'static str, Deployment)> = Vec::new();
    for (role, d) in [
        ("booted", deployments.booted()),
        ("staged", deployments.staged()),
        ("rollback", deployments.rollback()),
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
pub fn deployed(ui: &mut Ui, machine: &Machine, refresh: Refresh) -> Result<usize> {
    let state = host::state();
    let current = match Generation::current(&state) {
        Ok(Some(g)) => g,
        // Nothing added yet: nothing to carry across images.
        Ok(None) => return Ok(0),
        Err(e) => return Err(anyhow::Error::new(e).context("reading the current generation")),
    };
    let (mut built, mut first_error) = (0, None);
    for (role, d) in wanted(machine) {
        let Some(digest) = d.image_digest.as_deref() else {
            continue;
        };
        if matches(&state, digest, &current)? {
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
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e.context("some deployed images have no host tier yet")),
        None => Ok(built),
    }
}

/// Whether the newest generation for `digest` was built from the
/// manifest `current` has.
fn matches(state: &nx_core::State, digest: &str, current: &Generation) -> Result<bool> {
    let Some(n) = select::newest_for(state, digest)? else {
        return Ok(false);
    };
    // The empty generation `nx select` made at boot is a placeholder, not a
    // host tier.
    if select::read_lock(state, n)?.pending {
        return Ok(false);
    }
    if n == current.number {
        return Ok(true);
    }
    let g = Generation::read(state, n).with_context(|| format!("reading generation {n}"))?;
    Ok(g.manifest == current.manifest)
}

fn build(
    ui: &mut Ui,
    machine: &Machine,
    role: &str,
    d: &Deployment,
    refresh: Refresh,
) -> Result<()> {
    let digest = d.image_digest.as_deref().unwrap_or_default();
    let version = d.version.as_deref().unwrap_or("unknown version");
    ui.step(format_args!(
        "resolving the host tier for the {role} image {version} ({})",
        crate::ui::short_digest(digest)
    ));
    let booted = d.booted;
    let target;
    let machine = if booted {
        machine
    } else {
        target = machine.targeting(d)?;
        &target
    };
    let mut session = Session::open(ui, machine, refresh, false, |_| Ok(()))?;
    let was_current = session.current.as_ref().map(|g| g.number);
    let manifest = session.manifest.clone();
    let resolved = session.resolve(ui, &manifest.host.packages)?;
    let built = session.build(ui, &manifest, &resolved.all)?;
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
        crate::live::activate(ui, &state, number, manifest.host.backend)?;
    }
    ui.done(format_args!(
        "generation {number} for the {role} image{}",
        if booted { ", live" } else { "" }
    ));
    Ok(())
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
        let roles: Vec<&str> = wanted(&machine).iter().map(|(role, _)| *role).collect();
        assert_eq!(roles, ["booted", "rollback"]);
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
        assert!(!matches(&state, "sha256:new", &current)?);
        lock.pending = false;
        write(2, &lock)?;
        assert!(matches(&state, "sha256:new", &current)?);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
