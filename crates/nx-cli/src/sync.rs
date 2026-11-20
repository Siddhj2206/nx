//! `nx sync`: gives every deployed image (booted, staged, rollback) a
//! generation that matches the manifest (design invariants 5 and 6).
//!
//! An update staged by `bootc upgrade` is re-resolved before the reboot, so
//! the host tier is waiting when it boots; the rollback image keeps one so
//! `bootc rollback` finds its host tier. Only the booted image's generation
//! is made live. The others are built from the image's own tree under
//! `/sysroot`, which is readable without root.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use nx_core::select;
use nx_core::{Deployment, Fallback, Generation};
use nx_nix::Nix;
use nx_rpm::Refresh;

use crate::host::{self, Machine};
use crate::session::{Session, Unresolvable};
use crate::ui::Ui;

/// `nx sync [--refresh] [--rebuild]`. The daily refresh also collects
/// `nix`'s store. `rebuild` builds every deployed image's generation again,
/// whether or not it matches.
pub fn run(
    ui: &mut Ui,
    machine: &Machine,
    nix: &dyn Nix,
    refresh: Refresh,
    rebuild: bool,
) -> Result<()> {
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
    let Deployed { built, unbuilt } = deployed(ui, machine, &host::state(), refresh, rebuild)?;
    if !unbuilt.is_empty() {
        bail!(
            "{} no host tier yet\n{}",
            if unbuilt.len() == 1 {
                "a deployed image has"
            } else {
                "some deployed images have"
            },
            unbuilt.join("\n")
        );
    }
    // Generations nothing deployed needs any more (an image that is gone,
    // older staged ones). Only after every image has its own: a failure
    // above returned already. The daily refresh collects the Nix store too.
    let nix = (refresh != Refresh::IfMissing).then_some(nix);
    let removed = crate::gc::collect(ui, machine, nix)?;
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

/// What [`deployed`] did.
#[derive(Debug, Default)]
pub struct Deployed {
    /// Generations built.
    pub built: usize,
    /// Images still without a host tier, one line each: which image, and
    /// why. The caller reports them, once.
    pub unbuilt: Vec<String>,
}

/// Builds a generation for each deployed image that lacks one for the
/// current manifest, or whose one an nx older than
/// [`select::REBUILD_BEFORE`] built (every one with `rebuild`). Every image
/// is tried: one that fails is in [`Deployed::unbuilt`], not an error; one
/// that fails to rebuild keeps the generation it has, with a warning.
pub fn deployed(
    ui: &mut Ui,
    machine: &Machine,
    state: &nx_core::State,
    refresh: Refresh,
    rebuild: bool,
) -> Result<Deployed> {
    let current = match Generation::current(state) {
        Ok(Some(g)) => g,
        // Nothing added yet: nothing to carry across images.
        Ok(None) => return Ok(Deployed::default()),
        Err(e) => return Err(anyhow::Error::new(e).context("reading the current generation")),
    };
    let mut built = 0;
    let (mut failures, mut unbuilt) = (BTreeMap::new(), Vec::new());
    for (role, d) in wanted(machine) {
        let Some(digest) = d.image_digest.as_deref() else {
            continue;
        };
        let suspect = match have(state, digest, &current, refresh, rebuild)? {
            Have::Matching => continue,
            Have::Stale => false,
            Have::Suspect { number, builder } => {
                ui.step(format_args!(
                    "rebuilding generation {number} for the {role} image: {}",
                    match builder {
                        _ if rebuild => "asked to".to_owned(),
                        Some(b) => format!("nx {b} built it"),
                        None => "nx 1.0 built it".to_owned(),
                    }
                ));
                true
            }
        };
        let result = build(ui, machine, role, &d, refresh, suspect);
        match result {
            Ok(()) => built += 1,
            // The image still has a host tier, only an old one.
            Err(e) if suspect => ui.warn(format_args!(
                "the {role} image keeps the generation it has; rebuilding failed: {}",
                first_line(&e)
            )),
            Err(e) => {
                unbuilt.push(format!(
                    "no host tier for the {role} image ({}): {e:#}",
                    crate::ui::short_digest(digest)
                ));
                failures.insert(digest.to_owned(), first_line(&e));
            }
        }
    }
    // Only this run's failures stand: an image that now has a host tier, or
    // is no longer deployed, drops out.
    select::record_failures(state, &failures);
    Ok(Deployed { built, unbuilt })
}

/// What an image has, as far as `nx sync` cares.
#[derive(Debug, PartialEq, Eq)]
enum Have {
    /// A generation built from the manifest `current` has.
    Matching,
    /// None, or one built from another manifest: build one.
    Stale,
    /// One that matches but is to be built again: an nx older than the
    /// floor built it (`builder`, `None` for nx 1.0), or `--rebuild`.
    Suspect {
        number: u32,
        builder: Option<String>,
    },
}

/// Whether the newest generation for `digest` was built from the
/// manifest `current` has, by an nx at or above the floor. One that left
/// packages out is tried again only when the metadata is being refreshed:
/// that is when they might resolve.
fn have(
    state: &nx_core::State,
    digest: &str,
    current: &Generation,
    refresh: Refresh,
    rebuild: bool,
) -> Result<Have> {
    let Some(n) = select::newest_for(state, digest)? else {
        return Ok(Have::Stale);
    };
    // The empty generation `nx select` made at boot is a placeholder, not a
    // host tier.
    let lock = select::read_lock(state, n)?;
    if lock.pending
        || (!lock.skipped.is_empty() && matches!(refresh, Refresh::IfStale | Refresh::Always))
    {
        return Ok(Have::Stale);
    }
    if n != current.number {
        let g = Generation::read(state, n).with_context(|| format!("reading generation {n}"))?;
        if g.manifest != current.manifest
            || !crate::tool::same(g.lock.tool.as_ref(), current.lock.tool.as_ref())
        {
            return Ok(Have::Stale);
        }
    }
    if rebuild || lock.built_before(select::REBUILD_BEFORE) {
        return Ok(Have::Suspect {
            number: n,
            builder: lock.builder,
        });
    }
    Ok(Have::Matching)
}

fn build(
    ui: &mut Ui,
    machine: &Machine,
    role: Role,
    d: &Deployment,
    refresh: Refresh,
    rebuild: bool,
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
        // A rebuild replaces it whatever it holds.
        if !rebuild
            && let Some(n) = select::newest_for(session.state(), digest)?
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
    session.journal().fallback(
        Fallback::PartialResolve,
        &format!(
            "the manifest didn't resolve as a whole; leaving out {}",
            skipped.join(", ")
        ),
    );
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
    fn an_old_builders_generation_is_rebuilt() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-sync-builder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = nx_core::State::new(&dir);
        let mut lock = nx_core::Lock::new(Some("sha256:new".into()), "45", "x86_64");
        let current = Generation {
            number: 3,
            dir: Default::default(),
            manifest: nx_core::Manifest::default(),
            lock: lock.clone(),
            activation: Default::default(),
        };
        let write = |lock: &nx_core::Lock| -> Result<()> {
            let gen_dir = dir.join("generations/3");
            std::fs::create_dir_all(&gen_dir)?;
            std::fs::write(gen_dir.join("lock.toml"), lock.to_toml()?)?;
            Ok(())
        };
        let have = |rebuild| have(&state, "sha256:new", &current, Refresh::IfMissing, rebuild);
        write(&lock)?;
        assert_eq!(have(false)?, Have::Matching);
        let suspect = |builder: Option<&str>| Have::Suspect {
            number: 3,
            builder: builder.map(str::to_owned),
        };
        assert_eq!(have(true)?, suspect(Some(nx_core::VERSION)));
        // The bootc hook ran 1.0.0's nx sync for an image that ships 1.0.1.
        lock.builder = Some("1.0.0".into());
        write(&lock)?;
        assert_eq!(have(false)?, suspect(Some("1.0.0")));
        // nx 1.0 didn't say.
        lock.builder = None;
        write(&lock)?;
        assert_eq!(have(false)?, suspect(None));
        let _ = std::fs::remove_dir_all(&dir);
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
        let have = |refresh, rebuild| have(&state, "sha256:new", &current, refresh, rebuild);
        assert_eq!(have(Refresh::IfMissing, false)?, Have::Stale);
        lock.pending = false;
        write(2, &lock)?;
        assert_eq!(have(Refresh::IfMissing, false)?, Have::Matching);
        // One that left a package out is retried when the metadata refreshes,
        // not on every sync.
        lock.skipped = vec!["copr-thing".into()];
        write(2, &lock)?;
        assert_eq!(have(Refresh::IfMissing, false)?, Have::Matching);
        assert_eq!(have(Refresh::Always, false)?, Have::Stale);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
