//! `nx shell [-- command]`: the environment for building against the host
//! tier. With the sysext backend the host tier is merged into `/usr`, so a
//! shell needs nothing. With the prefix backend the generation's `usr` is
//! a prefix, and this exports the variables that find it, only inside the
//! shell (design: nx never exports them globally).

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use nx_core::Backend;

use crate::host;
use crate::ui::Ui;

pub fn run(ui: &mut Ui, command: &[String]) -> Result<()> {
    let state = host::state();
    let backend = state.backend().context("reading the activation backend")?;
    let mut cmd = match command.split_first() {
        Some((program, args)) => {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
        None => Command::new(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())),
    };
    match backend {
        None => bail!("nothing is live yet; nx add <name> makes the host tier"),
        Some(Backend::Sysext) => {
            if command.is_empty() {
                ui.step("sysext: the host tier is merged into /usr; opening a plain shell");
            }
        }
        Some(Backend::Prefix) => {
            // Follows `current`, so a rollback is picked up by new shells.
            let prefix = state.root().join("current/sysext/usr");
            for (key, value) in prefix_env(&prefix, &|k| std::env::var_os(k)) {
                cmd.env(key, value);
            }
            if command.is_empty() {
                ui.step(format_args!("prefix: {} is on the path", prefix.display()));
            }
        }
    }
    cmd.env("NX_SHELL", "1");
    let err = cmd.exec();
    bail!("running {}: {err}", cmd.get_program().to_string_lossy())
}

/// The variables that make `prefix` (a `usr` tree) visible to the shell,
/// tools and compilers, each added to what `get` finds.
fn prefix_env(
    prefix: &Path,
    get: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<(&'static str, OsString)> {
    let join = |dirs: &[&str], key: &str, tail: &[&str]| -> OsString {
        let mut paths: Vec<PathBuf> = dirs.iter().map(|d| prefix.join(d)).collect();
        match get(key).filter(|v| !v.is_empty()) {
            Some(old) => paths.extend(std::env::split_paths(&old)),
            None => paths.extend(tail.iter().map(PathBuf::from)),
        }
        std::env::join_paths(paths).unwrap_or_default()
    };
    let libs = ["lib64", "lib"];
    let mut path = std::env::split_paths(&get("PATH").unwrap_or_default()).collect::<Vec<_>>();
    // After the system's: nx never shadows a binary, and a generation
    // holds none the image has.
    path.push(prefix.join("bin"));
    vec![
        ("PATH", std::env::join_paths(path).unwrap_or_default()),
        (
            "PKG_CONFIG_PATH",
            join(
                &["lib64/pkgconfig", "lib/pkgconfig", "share/pkgconfig"],
                "PKG_CONFIG_PATH",
                &[],
            ),
        ),
        ("CPATH", join(&["include"], "CPATH", &[])),
        ("LIBRARY_PATH", join(&libs, "LIBRARY_PATH", &[])),
        ("LD_LIBRARY_PATH", join(&libs, "LD_LIBRARY_PATH", &[])),
        (
            "XDG_DATA_DIRS",
            join(
                &["share"],
                "XDG_DATA_DIRS",
                &["/usr/local/share", "/usr/share"],
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &'static [(&str, &str)]) -> Vec<(&'static str, String)> {
        let get = |k: &str| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        };
        prefix_env(Path::new("/var/lib/nx/current/sysext/usr"), &get)
            .into_iter()
            .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
            .collect()
    }

    fn value<'a>(env: &'a [(&str, String)], key: &str) -> &'a str {
        env.iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, v)| v.as_str())
    }

    #[test]
    fn the_prefix_goes_after_the_system_on_the_path_and_before_in_the_rest() {
        let e = env(&[("PATH", "/usr/bin:/bin"), ("CPATH", "/opt/inc")]);
        assert_eq!(
            value(&e, "PATH"),
            "/usr/bin:/bin:/var/lib/nx/current/sysext/usr/bin"
        );
        assert_eq!(
            value(&e, "CPATH"),
            "/var/lib/nx/current/sysext/usr/include:/opt/inc"
        );
        assert_eq!(
            value(&e, "PKG_CONFIG_PATH"),
            "/var/lib/nx/current/sysext/usr/lib64/pkgconfig:\
             /var/lib/nx/current/sysext/usr/lib/pkgconfig:\
             /var/lib/nx/current/sysext/usr/share/pkgconfig"
        );
    }

    #[test]
    fn xdg_data_dirs_keeps_the_default_when_unset() {
        let e = env(&[]);
        assert_eq!(
            value(&e, "XDG_DATA_DIRS"),
            "/var/lib/nx/current/sysext/usr/share:/usr/local/share:/usr/share"
        );
    }
}
