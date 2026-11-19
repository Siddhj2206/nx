//! Solves against tiny hand-written repos: an "image" (installed) and a
//! "fedora" repo, in rpm-md form.

use super::*;

fn pkg(name: &str, ver: &str, rel: &str, extra: &str) -> String {
    format!(
        r#"<package type="rpm"><name>{name}</name><arch>x86_64</arch>
<version epoch="0" ver="{ver}" rel="{rel}"/><summary>{name} summary</summary>
<location href="Packages/{name}-{ver}-{rel}.x86_64.rpm"/>
<format><rpm:sourcerpm>{src}-{ver}-{rel}.src.rpm</rpm:sourcerpm>
<rpm:provides><rpm:entry name="{name}" flags="EQ" epoch="0" ver="{ver}" rel="{rel}"/></rpm:provides>
{extra}</format></package>"#,
        src = name.trim_end_matches("-devel").trim_end_matches("-libs"),
    )
}

fn requires(names: &[&str]) -> String {
    let entries: String = names
        .iter()
        .map(|n| format!(r#"<rpm:entry name="{n}"/>"#))
        .collect();
    format!("<rpm:requires>{entries}</rpm:requires>")
}

fn primary(pkgs: &[String]) -> String {
    format!(
        r#"<?xml version="1.0"?><metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="{}">{}</metadata>"#,
        pkgs.len(),
        pkgs.concat()
    )
}

struct Fixture {
    dir: std::path::PathBuf,
    pool: Pool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(tag: &str, image: &[String], fedora: &[String]) -> Result<Fixture> {
    let dir = std::env::temp_dir().join(format!("nx-solv-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Write {
        path: dir.clone(),
        message: e.to_string(),
    })?;
    let mut pool = Pool::new("x86_64")?;
    for (name, pkgs) in [("image", image), ("fedora", fedora)] {
        let path = dir.join(format!("{name}.xml"));
        std::fs::write(&path, primary(pkgs)).map_err(|e| Error::Write {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let id = pool.load_rpmmd(name, &path, 99)?;
        if name == "image" {
            pool.set_installed(id);
        }
    }
    pool.prepare();
    Ok(Fixture { dir, pool })
}

fn image() -> Vec<String> {
    vec![
        pkg("glibc", "2.42", "1.fc45", ""),
        pkg("libxkbcommon", "1.11.0", "1.fc45", &requires(&["glibc"])),
    ]
}

fn names(solution: &Solution) -> Vec<String> {
    match solution {
        Solution::Install(p) => p.iter().map(Package::nevra).collect(),
        Solution::Problems(p) => p.messages.clone(),
    }
}

#[test]
fn installs_only_what_the_image_lacks() -> Result<()> {
    let fedora = vec![
        pkg("glibc", "2.42", "1.fc45", ""),
        pkg("libxkbcommon", "1.11.0", "1.fc45", ""),
        pkg(
            "libxkbcommon-devel",
            "1.11.0",
            "1.fc45",
            &requires(&["libxkbcommon", "pkgconf"]),
        ),
        pkg("pkgconf", "2.4", "1.fc45", ""),
    ];
    let mut f = fixture("only", &image(), &fedora)?;
    let s = f.pool.install(&["libxkbcommon-devel"], &[], &[])?;
    assert_eq!(
        names(&s),
        vec![
            "libxkbcommon-devel-1.11.0-1.fc45.x86_64",
            "pkgconf-2.4-1.fc45.x86_64"
        ]
    );
    Ok(())
}

#[test]
fn never_upgrades_the_image() -> Result<()> {
    // Fedora has moved on: only a newer libxkbcommon and a -devel that
    // needs exactly it.
    let fedora = vec![
        pkg("libxkbcommon", "1.12.0", "1.fc45", ""),
        pkg(
            "libxkbcommon-devel",
            "1.12.0",
            "1.fc45",
            r#"<rpm:requires><rpm:entry name="libxkbcommon" flags="EQ" epoch="0" ver="1.12.0" rel="1.fc45"/></rpm:requires>"#,
        ),
    ];
    let mut f = fixture("upgrade", &image(), &fedora)?;
    let s = f.pool.install(&["libxkbcommon-devel"], &[], &[])?;
    assert!(matches!(s, Solution::Problems(_)), "{s:?}");
    Ok(())
}

#[test]
fn hidden_packages_are_ignored() -> Result<()> {
    let fedora = vec![pkg("ripgrep", "14.1", "1.fc45", "")];
    let mut f = fixture("hidden", &image(), &fedora)?;
    let rg = f.pool.select("ripgrep")?;
    let s = f.pool.install(&["ripgrep"], &rg, &[])?;
    assert!(matches!(s, Solution::Problems(_)), "{s:?}");
    // And visible again on the next solve.
    let s = f.pool.install(&["ripgrep"], &[], &[])?;
    assert_eq!(names(&s), vec!["ripgrep-14.1-1.fc45.x86_64"]);
    Ok(())
}

#[test]
fn problems_name_the_hidden_builds_in_the_way() -> Result<()> {
    // The image has libxkbcommon 1.11.0; the repo only has a newer -devel
    // that needs exactly the newer library.
    let fedora = vec![
        pkg("libxkbcommon", "1.12.0", "1.fc45", ""),
        pkg(
            "libxkbcommon-devel",
            "1.12.0",
            "1.fc45",
            r#"<rpm:requires><rpm:entry name="libxkbcommon" flags="EQ" epoch="0" ver="1.12.0" rel="1.fc45"/></rpm:requires>"#,
        ),
    ];
    let mut f = fixture("involved", &image(), &fedora)?;
    let hidden = f.pool.select("libxkbcommon-devel")?;
    let Solution::Problems(p) = f.pool.install(&["libxkbcommon-devel"], &hidden, &[])? else {
        panic!("a hidden-only package can't install");
    };
    assert!(hidden.iter().all(|id| p.involved.contains(id)), "{p:?}");
    Ok(())
}

#[test]
fn unknown_names_are_a_problem_not_an_error() -> Result<()> {
    let mut f = fixture("unknown", &image(), &[])?;
    let s = f.pool.install(&["nope"], &[], &[])?;
    assert_eq!(names(&s), vec!["no package matches nope"]);
    Ok(())
}

#[test]
fn package_details_and_solv_roundtrip() -> Result<()> {
    let f = fixture("solv", &image(), &[pkg("pkgconf", "2.4", "1.fc45", "")])?;
    let id = f.pool.select("pkgconf")?;
    let p = f.pool.package(id[0]);
    assert_eq!(p.source, "pkgconf-2.4-1.fc45.src.rpm");
    assert_eq!(p.source_name(), "pkgconf");
    assert_eq!(p.location, "Packages/pkgconf-2.4-1.fc45.x86_64.rpm");
    assert!(!p.installed);

    let solv = f.dir.join("fedora.solv");
    f.pool.write_solv(RepoId(1), &solv)?;
    let mut again = Pool::new("x86_64")?;
    again.load_solv("fedora", &solv, 99)?;
    again.prepare();
    assert_eq!(again.select("pkgconf")?.len(), 1);
    Ok(())
}

#[test]
fn source_names() {
    assert_eq!(source_name("wayland-1.24.0-1.fc45.src.rpm"), "wayland");
    assert_eq!(
        source_name("xorg-x11-proto-devel-2024.1-3.fc45.src.rpm"),
        "xorg-x11-proto-devel"
    );
}

#[test]
fn evr_order() -> Result<()> {
    let pool = Pool::new("x86_64")?;
    assert_eq!(pool.evr_cmp("1.10-1", "1.9-1"), std::cmp::Ordering::Greater);
    assert_eq!(
        pool.evr_cmp("1:1.0-1", "2.0-1"),
        std::cmp::Ordering::Greater
    );
    Ok(())
}

/// A real Fedora package, recorded under `testdata/rpm`.
fn fixture_rpm() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/rpm/libxkbcommon-devel-1.13.1-4.fc45.x86_64.rpm")
}

#[test]
fn reads_the_metadata_checksum() -> Result<()> {
    let with_sum = pkg(
        "pkgconf",
        "2.4",
        "1.fc45",
        "",
    )
    .replace(
        "<location",
        r#"<checksum type="sha256" pkgid="YES">834489e0eb7f4cb5a0addf9557f36ff592d295b130ba9121ec2868413abb9584</checksum><location"#,
    );
    let f = fixture("sum", &image(), &[with_sum])?;
    let id = f.pool.select("pkgconf")?;
    assert_eq!(
        f.pool.package(id[0]).sha256,
        "834489e0eb7f4cb5a0addf9557f36ff592d295b130ba9121ec2868413abb9584"
    );
    Ok(())
}

#[test]
fn favored_builds_win() -> Result<()> {
    let fedora = vec![
        pkg("ripgrep", "14.1", "1.fc45", ""),
        pkg("ripgrep", "15.0", "1.fc45", ""),
    ];
    let mut f = fixture("favor", &image(), &fedora)?;
    let old: Vec<_> = f
        .pool
        .select("ripgrep")?
        .into_iter()
        .filter(|id| f.pool.package(*id).evr == "14.1-1.fc45")
        .collect();
    let s = f.pool.install(&["ripgrep"], &[], &old)?;
    assert_eq!(names(&s), vec!["ripgrep-14.1-1.fc45.x86_64"]);
    let s = f.pool.install(&["ripgrep"], &[], &[])?;
    assert_eq!(names(&s), vec!["ripgrep-15.0-1.fc45.x86_64"]);
    Ok(())
}

#[test]
fn adds_a_package_file() -> Result<()> {
    // Many solvables, all hidden in a first solve: a package added after it
    // must still be found.
    let many: Vec<String> = (0..100)
        .map(|i| pkg(&format!("filler{i}"), "1", "1.fc45", ""))
        .collect();
    let mut f = fixture("rpm", &image(), &many)?;
    let all: Vec<_> = f.pool.all().collect();
    f.pool.install(&["filler1"], &all, &[])?;
    let repo = f.pool.create_empty("koji", 99)?;
    let id = f.pool.add_rpm(repo, &fixture_rpm())?;
    f.pool.prepare();
    let p = f.pool.package(id);
    assert_eq!(p.nevra(), "libxkbcommon-devel-1.13.1-4.fc45.x86_64");
    assert_eq!(p.source, "libxkbcommon-1.13.1-4.fc45.src.rpm");
    assert_eq!(f.pool.select("pkgconfig(xkbcommon)")?, vec![id]);
    Ok(())
}

#[test]
fn finds_image_owners_of_a_file() -> Result<()> {
    let image = vec![pkg("ripgrep", "14.1", "1.fc45", "<file>/usr/bin/rg</file>")];
    let f = fixture(
        "owner",
        &image,
        &[pkg("fd", "9", "1.fc45", "<file>/usr/bin/fd</file>")],
    )?;
    let names = |path| -> Result<Vec<String>> {
        Ok(f.pool
            .image_owners(path)?
            .into_iter()
            .map(|id| f.pool.package(id).name)
            .collect())
    };
    assert_eq!(names("/usr/bin/rg")?, vec!["ripgrep"]);
    assert!(names("/usr/bin/fd")?.is_empty(), "fd is not in the image");
    Ok(())
}
