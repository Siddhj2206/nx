//! Making a generation live. This is what nxd runs as root; the CLI never
//! calls it except through nxd.
//!
//! Everything that touches the system goes through a [`Layout`] (where
//! things are) and a [`Runner`] (how commands run), so tests run
//! unprivileged against a temporary tree and a fake `systemd-sysext`.
//!
//! The sysext backend (design, *Activation backends*): label the
//! generation's extension tree for the paths it will appear at, point
//! `current` at it, make sure `/var/lib/extensions/nx` links to it, and
//! `systemd-sysext refresh`. The prefix backend is the fallback when the
//! merge fails: no link, nothing merged, and `nx shell` exports the
//! generation's `usr` as a prefix.

pub mod nixconf;
mod units;
mod validate;
mod wheel;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use nx_core::{Backend, State};

pub use units::Services;
pub use validate::{Validated, validate, wheel_uids};
pub use wheel::Wheel;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The generation is not one nxd will make live.
    #[error("generation {number} can't be activated: {reason}")]
    Refused { number: u32, reason: String },
    #[error("{program} failed: {detail}")]
    Command {
        program: &'static str,
        detail: String,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    State(#[from] nx_core::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where things are. [`Layout::system`] is the real machine; tests put
/// every path under a temporary directory.
#[derive(Debug, Clone)]
pub struct Layout {
    pub state: State,
    /// systemd-sysext's primary search path, `/var/lib/extensions`.
    pub extensions: PathBuf,
    /// The root whose `usr/lib/os-release` the extension must match.
    pub root: PathBuf,
    /// The policy's `file_contexts`, or `None` when SELinux is off.
    pub file_contexts: Option<PathBuf>,
    /// The booted image's own tree, which an extension must not shadow.
    pub image: Image,
    /// Nix's configuration directory (`/etc/nix`), where the generation's
    /// extra caches are listed; `None` when Nix isn't installed.
    pub nix_conf: Option<PathBuf>,
}

/// Where to check an extension for files that would hide the image's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// Not a bootc system: nothing is the image's to protect.
    None,
    /// The booted deployment's root under `/sysroot`.
    Root(PathBuf),
    /// A bootc system whose booted deployment couldn't be found: refuse
    /// rather than merge unchecked.
    Unreadable(String),
}

impl Layout {
    /// The real machine, with `state` as nx's state directory.
    pub fn system(state: State) -> Self {
        Self {
            state,
            extensions: PathBuf::from("/var/lib/extensions"),
            root: PathBuf::from("/"),
            file_contexts: file_contexts(Path::new("/")),
            image: Image::None,
            nix_conf: Some(PathBuf::from("/etc/nix")).filter(|d| d.join("nix.conf").is_file()),
        }
    }

    /// The link systemd-sysext finds nx's extension through. Its name is
    /// the extension's: it must match `extension-release.nx`.
    pub fn link(&self) -> PathBuf {
        self.extensions.join("nx")
    }

    /// What the link points at: the current generation's extension tree.
    fn link_target(&self) -> PathBuf {
        self.state.root().join("current/sysext")
    }
}

/// The loaded policy's `file_contexts`, if SELinux is on (selinuxfs is
/// mounted) under `root`.
pub fn file_contexts(root: &Path) -> Option<PathBuf> {
    if !root.join("sys/fs/selinux/enforce").exists() {
        return None;
    }
    let config = std::fs::read_to_string(root.join("etc/selinux/config")).unwrap_or_default();
    let policy = config
        .lines()
        .find_map(|l| l.strip_prefix("SELINUXTYPE="))
        .map_or("targeted", str::trim);
    let path = root
        .join("etc/selinux")
        .join(policy)
        .join("contexts/files/file_contexts");
    path.is_file().then_some(path)
}

/// What a command printed.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs the system's commands (`setfiles`, `systemd-sysext`).
pub trait Runner {
    fn run(&self, program: &str, args: &[&OsStr]) -> std::io::Result<Output>;
}

/// Runs them for real.
pub struct System;

impl Runner for System {
    fn run(&self, program: &str, args: &[&OsStr]) -> std::io::Result<Output> {
        let out = Command::new(program).args(args).output()?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// What activation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activated {
    pub backend: Backend,
    /// Why the sysext backend was passed over, when it was tried and the
    /// merge failed.
    pub fallback: Option<String>,
    /// What was done to units, users and files, and what wasn't.
    pub services: Services,
}

/// Makes generation `number` current and live. `forced` picks a backend;
/// otherwise every activation probes: it tries sysext, and falls back to
/// prefix if the merge fails (overlay depth, an unsupported kernel), so a
/// system that can merge again later goes back to sysext by itself. The
/// backend that ended up in use is recorded for `nx shell`. `allowed` says
/// which uids besides root may own the generation's files (see
/// [`validate`]).
///
/// Around the merge, units follow the generation's `activation.toml`: those
/// of packages that leave are stopped and disabled first, and after it
/// users and files are created for packages that appear, their presets
/// applied, and changed ones restarted. The prefix backend merges nothing,
/// so it sets none of that up.
pub fn activate(
    layout: &Layout,
    runner: &dyn Runner,
    number: u32,
    forced: Option<Backend>,
    allowed: &dyn Fn(u32) -> bool,
) -> Result<Activated> {
    let valid = validate(layout, number, allowed)?;
    let merged = merged_now(layout);
    let mut services = Services::default();
    if let Some(dir) = &layout.nix_conf {
        nixconf::apply(dir, runner, &valid.caches, &mut services);
    }
    if forced == Some(Backend::Prefix) {
        unmerge_units(runner, &layout.root, merged.as_ref(), &mut services);
        prefix(layout, runner, number)?;
        prefix_notice(&valid.activation, &mut services);
        return Ok(Activated {
            backend: Backend::Prefix,
            fallback: None,
            services,
        });
    }
    let transition = match &merged {
        Some((old, ext)) => {
            let t = units::plan(old, &valid.activation);
            units::stop(runner, &layout.root, ext, &t, &mut services);
            t
        }
        None => units::plan(&nx_core::Activation::default(), &valid.activation),
    };
    match sysext(layout, runner, number, &valid) {
        Ok(()) => {
            units::start(runner, &valid.sysext, &transition, &mut services);
            Ok(Activated {
                backend: Backend::Sysext,
                fallback: None,
                services,
            })
        }
        Err(Error::Command {
            program: "systemd-sysext",
            detail,
        }) if forced.is_none() => {
            unmerge_units(runner, &layout.root, merged.as_ref(), &mut services);
            prefix(layout, runner, number)?;
            prefix_notice(&valid.activation, &mut services);
            Ok(Activated {
                backend: Backend::Prefix,
                fallback: Some(detail),
                services,
            })
        }
        Err(e) => Err(e),
    }
}

/// What [`settle_at_boot`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// No generation is current, or none is linked for merging.
    NothingToMerge,
    /// nx's extension is merged over `/usr`.
    Merged,
    /// The boot-time merge didn't happen; the prefix backend is recorded
    /// and the link removed. The reason is what `systemd-sysext` says.
    FellBack(String),
    /// Not merged, but the manifest forces sysext: nothing is changed, the
    /// same error a forced activation gives.
    ForcedAndUnmerged(String),
}

/// Run after `systemd-sysext` at boot: when a generation is linked but its
/// extension is not merged over `/usr` (overlay depth, an unsupported
/// kernel), switches to the prefix backend, so `nx shell` exports the
/// generation's `usr` instead of the system booting with a host tier that
/// silently isn't there. The next activation tries sysext again.
pub fn settle_at_boot(layout: &Layout, runner: &dyn Runner) -> Result<Settled> {
    let Some(number) = layout.state.current_number()? else {
        return Ok(Settled::NothingToMerge);
    };
    if std::fs::symlink_metadata(layout.link()).is_err() {
        return Ok(Settled::NothingToMerge);
    }
    // A placeholder generation has nothing to merge, so nothing to fall
    // back for; a forced backend is the manifest's to decide.
    let generation = nx_core::Generation::read(&layout.state, number)?;
    if generation.lock.pending {
        return Ok(Settled::NothingToMerge);
    }
    let status = sysext_cmd(runner, &["status", "--json=short"])?;
    if status.success && merged_over_usr(&status.stdout) {
        if layout.state.backend()? != Some(Backend::Sysext) {
            layout.state.set_backend(Backend::Sysext)?;
        }
        return Ok(Settled::Merged);
    }
    let why = if status.success {
        "nx's extension is not merged over /usr".to_owned()
    } else {
        detail(&status)
    };
    if generation.manifest.host.backend == Some(Backend::Sysext) {
        return Ok(Settled::ForcedAndUnmerged(why));
    }
    // The record first: a crash between the two leaves a link nothing
    // merges, which the next settle finds again.
    layout.state.set_backend(Backend::Prefix)?;
    let link = layout.link();
    std::fs::remove_file(&link).map_err(|source| Error::Io {
        path: link.clone(),
        source,
    })?;
    sync_dir(&layout.extensions)?;
    Ok(Settled::FellBack(why))
}

/// The record and extension of the generation that is merged now, if one
/// is: the current one under the sysext backend.
fn merged_now(layout: &Layout) -> Option<(nx_core::Activation, PathBuf)> {
    if layout.state.backend().ok()? != Some(Backend::Sysext) {
        return None;
    }
    let n = layout.state.current_number().ok()??;
    let g = nx_core::Generation::read(&layout.state, n).ok()?;
    Some((
        g.activation,
        layout
            .state
            .root()
            .join("generations")
            .join(n.to_string())
            .join("sysext"),
    ))
}

/// Stops the units of everything merged, before it is unmerged.
fn unmerge_units(
    runner: &dyn Runner,
    root: &Path,
    merged: Option<&(nx_core::Activation, PathBuf)>,
    services: &mut Services,
) {
    if let Some((old, ext)) = merged {
        units::stop(runner, root, ext, &units::plan_unmerge(old), services);
    }
}

/// The prefix backend merges nothing, so a package's units (which name
/// `/usr` paths), users and files are not set up.
fn prefix_notice(new: &nx_core::Activation, services: &mut Services) {
    let any = new.packages.iter().any(|p| {
        !p.intents.preset.is_empty()
            || !p.intents.restart.is_empty()
            || !p.sysusers_files.is_empty()
            || !p.tmpfiles_files.is_empty()
    });
    if any {
        services.warnings.push(
            "the prefix backend merges nothing, so services, users and files from packages are not set up"
                .into(),
        );
    }
}

/// Labels, points `current` at `number`, links the extension and merges.
/// Any failure puts back what was there before: the previous generation
/// (or no `current` and no link, for the first one), re-merged best-effort.
fn sysext(layout: &Layout, runner: &dyn Runner, number: u32, valid: &Validated) -> Result<()> {
    label(layout, runner, &valid.sysext)?;
    let previous = layout.state.current_number()?;
    layout.state.set_current(number)?;
    let merged = link(layout)
        .and_then(|()| refresh(runner))
        .and_then(|()| check_merged(runner));
    if let Err(e) = merged {
        match previous {
            Some(previous) => {
                let _ = layout.state.set_current(previous);
            }
            None => {
                let _ = std::fs::remove_file(layout.link());
                let _ = std::fs::remove_file(layout.state.root().join("current"));
            }
        }
        let _ = refresh(runner);
        return Err(e);
    }
    layout.state.set_backend(Backend::Sysext)?;
    Ok(())
}

/// Unmerges whatever nx merged and records the generation as current.
fn prefix(layout: &Layout, runner: &dyn Runner, number: u32) -> Result<()> {
    let link = layout.link();
    let linked = std::fs::symlink_metadata(&link).is_ok();
    if linked {
        std::fs::remove_file(&link).map_err(|source| Error::Io {
            path: link.clone(),
            source,
        })?;
        sync_dir(&layout.extensions)?;
        refresh(runner)?;
    }
    layout.state.set_current(number)?;
    layout.state.set_backend(Backend::Prefix)?;
    Ok(())
}

/// Gives each file of the extension the label of the path it will appear
/// at once merged (`usr_t`, `bin_t`, `lib_t`): `setfiles -r <tree> <contexts>
/// <tree>`. `restorecon` would label by where the files sit,
/// `/var/lib/nx/...`, as `var_lib_t`. `-r` needs the real path, not one
/// through `current`.
fn label(layout: &Layout, runner: &dyn Runner, tree: &Path) -> Result<()> {
    let Some(contexts) = &layout.file_contexts else {
        return Ok(());
    };
    let out = runner
        .run(
            "setfiles",
            &[
                OsStr::new("-r"),
                tree.as_os_str(),
                contexts.as_os_str(),
                tree.as_os_str(),
            ],
        )
        .map_err(|e| spawn("setfiles", &e))?;
    if out.success {
        Ok(())
    } else {
        Err(Error::Command {
            program: "setfiles",
            detail: detail(&out),
        })
    }
}

/// Makes `/var/lib/extensions/nx` a link to the current extension tree, in
/// one rename.
fn link(layout: &Layout) -> Result<()> {
    let link = layout.link();
    let target = layout.link_target();
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            if std::fs::read_link(&link).is_ok_and(|t| t == target) {
                return Ok(());
            }
        }
        Ok(_) => {
            return Err(Error::Io {
                path: link,
                source: std::io::Error::other("exists and is not a symlink; nx won't replace it"),
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::Io { path: link, source }),
    }
    std::fs::create_dir_all(&layout.extensions).map_err(|source| Error::Io {
        path: layout.extensions.clone(),
        source,
    })?;
    let tmp = layout.extensions.join(".nx.part");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&target, &tmp).map_err(|source| Error::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &link).map_err(|source| Error::Io {
        path: link.clone(),
        source,
    })?;
    sync_dir(&layout.extensions)
}

/// `systemd-sysext refresh`. systemd 262 reloads the service manager
/// itself.
fn refresh(runner: &dyn Runner) -> Result<()> {
    let out = sysext_cmd(runner, &["refresh"])?;
    if out.success {
        Ok(())
    } else {
        Err(Error::Command {
            program: "systemd-sysext",
            detail: detail(&out),
        })
    }
}

/// Checks nx's extension is the one merged over `/usr`: a refresh can
/// succeed and merge nothing.
fn check_merged(runner: &dyn Runner) -> Result<()> {
    let status = sysext_cmd(runner, &["status", "--json=short"])?;
    if status.success && merged_over_usr(&status.stdout) {
        Ok(())
    } else {
        Err(Error::Command {
            program: "systemd-sysext",
            detail: "refresh succeeded but nx's extension is not merged over /usr".into(),
        })
    }
}

fn sysext_cmd(runner: &dyn Runner, args: &[&str]) -> Result<Output> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    runner
        .run("systemd-sysext", &args)
        .map_err(|e| spawn("systemd-sysext", &e))
}

/// Makes a rename or removal in `dir` durable.
fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })
}

/// Whether `systemd-sysext status --json` lists `nx` under `/usr`.
fn merged_over_usr(json: &str) -> bool {
    let Ok(serde_json::Value::Array(hierarchies)) = serde_json::from_str(json) else {
        return false;
    };
    hierarchies.iter().any(|h| {
        h["hierarchy"] == "/usr"
            && h["extensions"]
                .as_array()
                .is_some_and(|e| e.iter().any(|n| n == "nx"))
    })
}

fn spawn(program: &'static str, e: &std::io::Error) -> Error {
    Error::Command {
        program,
        detail: format!("can't run it: {e}"),
    }
}

fn detail(out: &Output) -> String {
    let text = if out.stderr.trim().is_empty() {
        &out.stdout
    } else {
        &out.stderr
    };
    text.trim().to_owned()
}

#[cfg(test)]
mod tests;
