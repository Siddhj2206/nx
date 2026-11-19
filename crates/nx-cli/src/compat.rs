//! Design invariant 2: a generation built for another image is merged only
//! if it still fits the image that booted.
//!
//! It fits when the Fedora release and architecture are the same, no file
//! of it hides one of the image's (invariant 1), and solving its manifest
//! against this image, from the metadata already cached, needs nothing the
//! generation doesn't have. Image relatives must be the image's exact build,
//! so a generation with a `-devel` package of a library the new image moved
//! past fails here, and nothing is merged for it. The check never touches the
//! network (boot doesn't wait on it: invariant 7); a repo with no cached
//! metadata, or a build only Koji has, makes it fail closed.

use std::collections::HashSet;
use std::path::PathBuf;

use nx_core::Generation;
use nx_rpm::{Package, Plan, Refresh, Relative, Universe, Ureq};

use crate::host::{self, Machine};

/// Why `generation` can't be merged on `machine`'s image, if it can't.
pub fn check(machine: &Machine, generation: &Generation) -> Result<(), String> {
    let lock = &generation.lock;
    if lock.release != machine.release.version_id {
        return Err(format!(
            "it was built for Fedora {}, and this image is Fedora {}",
            lock.release, machine.release.version_id
        ));
    }
    if lock.arch != machine.arch {
        return Err(format!(
            "it was built for {}, and this machine is {}",
            lock.arch, machine.arch
        ));
    }
    let image_root = machine.image_root().map_err(|e| format!("{e:#}"))?;
    nx_core::generation::check_extension(&generation.sysext(), &image_root)
        .map_err(|e| e.to_string())?;
    if lock.host.is_empty() {
        return Ok(());
    }
    let cache = host::cache_dir().map_err(|e| format!("{e:#}"))?;
    let mut opts = machine.options(Refresh::Never, cache);
    opts.extra =
        host::added_repos(&generation.manifest, &opts.vars).map_err(|e| format!("{e:#}"))?;
    let mut universe =
        Universe::load(&opts, &Ureq::new(), &mut |_| {}).map_err(|e| e.to_string())?;
    if let Some(skipped) = universe.skipped.first() {
        return Err(format!(
            "its repos can't be read offline ({}: {})",
            skipped.repo, skipped.error
        ));
    }
    let specs: Vec<&str> = generation
        .manifest
        .host
        .packages
        .iter()
        .map(String::as_str)
        .collect();
    let locked: Vec<String> = lock.host.iter().map(|p| p.nevra.clone()).collect();
    let mut offline = |_: &Relative, _: &Package| -> nx_rpm::Result<PathBuf> {
        Err(nx_rpm::Error::Invalid(
            "the image's exact build is not in the cached metadata".into(),
        ))
    };
    let plan = universe
        .plan(&specs, &locked, &mut offline)
        .map_err(|e| e.to_string())?;
    let all = match plan {
        Plan::Install(all) => all,
        Plan::Problems(problems) => return Err(problems.join("; ")),
    };
    let have: HashSet<&str> = locked.iter().map(String::as_str).collect();
    let moved: Vec<String> = all
        .iter()
        .map(|p| p.package.nevra())
        .filter(|n| !have.contains(n.as_str()))
        .collect();
    if moved.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "this image needs {} where the generation has other builds",
            moved.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nx_core::DeploymentSource;
    use nx_core::{Lock, Manifest};

    struct NoDeployments;
    impl DeploymentSource for NoDeployments {
        fn deployments(&self) -> nx_core::Result<nx_core::Deployments> {
            Err(nx_core::Error::Invalid("none".into()))
        }
    }

    fn machine(tag: &str, release: &str) -> std::io::Result<(Machine, std::path::PathBuf)> {
        let root = std::env::temp_dir().join(format!("nx-compat-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("usr/lib"))?;
        std::fs::write(
            root.join("usr/lib/os-release"),
            format!("ID=fedora\nVERSION_ID={release}\n"),
        )?;
        let m = Machine::read(&root, &NoDeployments).map_err(std::io::Error::other)?;
        Ok((m, root))
    }

    fn generation(root: &std::path::Path, release: &str) -> std::io::Result<Generation> {
        let dir = root.join("generation");
        std::fs::create_dir_all(dir.join("sysext/usr/bin"))?;
        std::fs::write(dir.join("sysext/usr/bin/tool"), "")?;
        Ok(Generation {
            number: 1,
            dir,
            manifest: Manifest::default(),
            lock: Lock::new(Some("sha256:old".into()), release, std::env::consts::ARCH),
            activation: Default::default(),
        })
    }

    #[test]
    fn another_fedora_release_does_not_fit() -> std::io::Result<()> {
        let (m, root) = machine("release", "46")?;
        let g = generation(&root, "45")?;
        let why = check(&m, &g).err().unwrap_or_default();
        assert_eq!(
            why,
            "it was built for Fedora 45, and this image is Fedora 46"
        );
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn a_file_the_image_now_ships_does_not_fit() -> std::io::Result<()> {
        let (m, root) = machine("overlap", "45")?;
        let g = generation(&root, "45")?;
        std::fs::create_dir_all(root.join("usr/bin"))?;
        std::fs::write(root.join("usr/bin/tool"), "")?;
        let why = check(&m, &g).err().unwrap_or_default();
        assert!(why.contains("the image already has /usr/bin/tool"), "{why}");
        // Without the clash, an empty host tier fits with no metadata at all.
        std::fs::remove_file(root.join("usr/bin/tool"))?;
        assert_eq!(check(&m, &g), Ok(()));
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
