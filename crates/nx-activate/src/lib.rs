//! Making a generation live. This is what nxd runs as root; the CLI never
//! calls it except through nxd.
//!
//! Everything that touches the system goes through a [`Layout`] (where
//! things are) and a [`Runner`] (how commands run), so tests run
//! unprivileged against a temporary tree and a fake `systemd-sysext`.
//!
//! Activation (design, *Activation*): copy the generation's extension
//! tree into root's own directory (`/var/lib/nx-merged`, reflinked), label
//! the copy for the paths it will appear at, point `current` at the
//! generation, link `/var/lib/extensions/nx` to the copy, and
//! `systemd-sysext refresh`. The generation stays its builder's; what is
//! merged is root's. A merge that fails puts back what was there and is an
//! error.

pub mod nixconf;
mod units;
mod validate;
mod wheel;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use nx_core::State;

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
    /// Root's copies of extension trees, `/var/lib/nx-merged`: the one the
    /// link names, and the next while it is being merged. Outside the state
    /// directory, which administrators can write.
    pub merged: PathBuf,
    /// The root whose `usr/lib/os-release` the extension must match.
    pub root: PathBuf,
    /// The policy's `file_contexts`, or `None` when SELinux is off.
    pub file_contexts: Option<PathBuf>,
    /// The booted image's own tree, which an extension must not shadow.
    pub image: Image,
    /// Nix's configuration directory (`/etc/nix`), where nx 1.0 listed
    /// extra caches ([`nixconf`]); `None` when Nix isn't installed.
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
            merged: PathBuf::from("/var/lib/nx-merged"),
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

    /// What the link names now, if there is one.
    fn linked(&self) -> Option<PathBuf> {
        std::fs::read_link(self.link()).ok()
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
    /// What was done to units, users and files, and what wasn't.
    pub services: Services,
}

/// Makes generation `number` current and merged. `allowed` says which
/// uids besides root may own the generation's files (see [`validate`]).
///
/// Around the merge, units follow the generation's `activation.toml`: those
/// of packages that leave are stopped and disabled first, and after it
/// users and files are created for packages that appear, their presets
/// applied, and changed ones restarted. A merge that fails (overlay depth,
/// a kernel without overlayfs) puts back the previous generation and is an
/// error that carries what `systemd-sysext` said.
pub fn activate(
    layout: &Layout,
    runner: &dyn Runner,
    number: u32,
    allowed: &dyn Fn(u32) -> bool,
) -> Result<Activated> {
    let valid = validate(layout, number, allowed)?;
    let merged = merged_now(layout);
    let mut services = Services::default();
    if let Some(dir) = &layout.nix_conf {
        nixconf::clean(dir, runner, &mut services);
    }
    let transition = match &merged {
        Some((old, ext)) => {
            let t = units::plan(old, &valid.activation);
            units::stop(runner, &layout.root, ext, &t, &mut services);
            t
        }
        None => units::plan(&nx_core::Activation::default(), &valid.activation),
    };
    sysext(layout, runner, number, &valid)?;
    units::start(runner, &valid.sysext, &transition, &mut services);
    Ok(Activated { services })
}

/// Run by `nx select` at boot, once `current` names the generation for
/// the booted image and before `systemd-sysext` merges: gives it root's
/// copy and points the link at it, as an activation would (without the
/// refresh: the boot's merge is next). Nothing to do when nothing is linked
/// (nothing made live yet) or the link already names this generation's
/// copy. `allowed` is as for [`validate`].
pub fn stage_at_boot(
    layout: &Layout,
    runner: &dyn Runner,
    allowed: &dyn Fn(u32) -> bool,
) -> Result<()> {
    let Some(number) = layout.state.current_number()? else {
        unstage(layout);
        return Ok(());
    };
    if std::fs::symlink_metadata(layout.link()).is_err() {
        return Ok(());
    }
    let staged = layout.linked().is_some_and(|t| {
        t.parent() == Some(layout.merged.as_path())
            && t.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{number}.")))
            && t.is_dir()
    });
    if staged {
        return Ok(());
    }
    let valid = validate(layout, number, allowed)?;
    label(layout, runner, &valid.sysext)
        .and_then(|()| link(layout, &valid.sysext))
        .inspect_err(|_| validate::prune(layout, None))?;
    validate::prune(layout, Some(&valid.sysext));
    Ok(())
}

/// For a boot that can't tell which generation fits: removes root's copies,
/// so the link names nothing and nothing merges. The link stays, so the
/// next boot that can tell stages again.
pub fn unstage(layout: &Layout) {
    validate::prune(layout, None);
}

/// The record and extension of the generation that is merged now, if one
/// is: the current one, when the link is there, from the tree it names
/// (root's copy).
fn merged_now(layout: &Layout) -> Option<(nx_core::Activation, PathBuf)> {
    std::fs::symlink_metadata(layout.link()).ok()?;
    let n = layout.state.current_number().ok()??;
    let g = nx_core::Generation::read(&layout.state, n).ok()?;
    let ext = layout
        .linked()
        .filter(|t| t.is_dir())
        .unwrap_or_else(|| g.sysext());
    Some((g.activation, ext))
}

/// Labels root's copy, points `current` at `number`, links the copy and
/// merges. Any failure puts back what was there before: the previous
/// generation and link (or no `current` and no link, for the first one),
/// re-merged best-effort. Either way only the copy that ends up linked is
/// kept.
fn sysext(layout: &Layout, runner: &dyn Runner, number: u32, valid: &Validated) -> Result<()> {
    let was_linked = layout.linked();
    label(layout, runner, &valid.sysext)
        .inspect_err(|_| validate::prune(layout, was_linked.as_deref()))?;
    let previous = layout.state.current_number()?;
    layout.state.set_current(number)?;
    let merged = link(layout, &valid.sysext)
        .and_then(|()| refresh(runner))
        .and_then(|()| check_merged(runner));
    if let Err(e) = merged {
        match previous {
            Some(previous) => {
                let _ = layout.state.set_current(previous);
            }
            None => {
                let _ = std::fs::remove_file(layout.state.root().join("current"));
            }
        }
        match &was_linked {
            Some(target) => {
                let _ = link(layout, target);
            }
            None => {
                let _ = std::fs::remove_file(layout.link());
            }
        }
        let _ = refresh(runner);
        validate::prune(layout, was_linked.as_deref());
        return Err(e);
    }
    validate::prune(layout, Some(&valid.sysext));
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

/// Makes `/var/lib/extensions/nx` a link to `target` (root's copy), in one
/// rename.
fn link(layout: &Layout, target: &Path) -> Result<()> {
    let link = layout.link();
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
    std::os::unix::fs::symlink(target, &tmp).map_err(|source| Error::Io {
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
    Ok(nx_core::state::sync_dir(dir)?)
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
