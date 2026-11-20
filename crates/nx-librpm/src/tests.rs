//! Against a real Fedora package and keys, recorded under `testdata/rpm`.

use super::*;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/rpm")
        .join(name)
}

const RPM: &str = "libxkbcommon-devel-1.13.1-4.fc45.x86_64.rpm";
const FEDORA_45: &str = "4f50a6114cd5c6976a7f1179655a4b02f577861e";

fn keyring(key: &str) -> Result<Keyring> {
    let mut k = Keyring::new()?;
    let text = std::fs::read_to_string(testdata(key)).map_err(|e| write_err(&testdata(key), e))?;
    k.add(&text)?;
    Ok(k)
}

fn fedora() -> Result<Keyring> {
    keyring("RPM-GPG-KEY-fedora-45-primary")
}

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nx-librpm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn reads_fingerprints() -> Result<()> {
    let text = std::fs::read_to_string(testdata("RPM-GPG-KEY-fedora-45-primary"))
        .map_err(|e| write_err(Path::new("key"), e))?;
    assert_eq!(fingerprints(&text)?, vec![FEDORA_45]);
    assert!(matches!(fingerprints("not a key"), Err(Error::Key(_))));
    Ok(())
}

#[test]
fn verifies_against_the_right_key_only() -> Result<()> {
    verify(&testdata(RPM), &fedora()?)?;
    let copr = keyring("copr-lilay-topgrade.gpg")?;
    let err = verify(&testdata(RPM), &copr).map(|()| "verified");
    assert!(matches!(err, Err(Error::Verify { .. })), "{err:?}");
    let err = verify(&testdata(RPM), &Keyring::new()?).map(|()| "verified");
    assert!(matches!(err, Err(Error::Verify { .. })), "{err:?}");
    Ok(())
}

#[test]
fn refuses_a_tampered_header() -> Result<()> {
    let dir = TempDir::new("tampered");
    let mut bytes = std::fs::read(testdata(RPM)).map_err(|e| write_err(&dir.0, e))?;
    // Inside the main header's data (past the lead and signature header).
    bytes[6000] ^= 0xff;
    let path = dir.0.join("bad.rpm");
    std::fs::write(&path, bytes).map_err(|e| write_err(&path, e))?;
    assert!(verify(&path, &fedora()?).is_err());
    Ok(())
}

#[test]
fn unpacks_the_payload() -> Result<()> {
    let dir = TempDir::new("unpack");
    let n = unpack(&testdata(RPM), &fedora()?, &dir.0)?;
    assert_eq!(n.files, 10, "{n:?}");
    assert!(n.bytes > 300_000, "{n:?}");
    let pc = dir.0.join("usr/lib64/pkgconfig/xkbcommon.pc");
    let text = std::fs::read_to_string(&pc).map_err(|e| write_err(&pc, e))?;
    assert!(text.contains("Name: xkbcommon"), "{text}");
    let link = std::fs::read_link(dir.0.join("usr/lib64/libxkbcommon.so"))
        .map_err(|e| write_err(&dir.0, e))?;
    assert_eq!(link, Path::new("libxkbcommon.so.0"));
    let meta = std::fs::metadata(&pc).map_err(|e| write_err(&pc, e))?;
    assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
    Ok(())
}

#[test]
fn refuses_to_unpack_over_files() -> Result<()> {
    let dir = TempDir::new("twice");
    unpack(&testdata(RPM), &fedora()?, &dir.0)?;
    assert!(unpack(&testdata(RPM), &fedora()?, &dir.0).is_err());
    Ok(())
}

#[test]
fn paths_stay_inside() {
    assert_eq!(relative("/usr/bin/rg"), Some(PathBuf::from("usr/bin/rg")));
    assert_eq!(relative("/usr/../etc/passwd"), None);
    assert_eq!(relative("/"), None);
}

#[test]
fn never_writes_through_a_symlink() -> std::io::Result<()> {
    let tmp = TempDir::new("escape");
    let dest = tmp.0.join("dest");
    let outside = tmp.0.join("outside");
    std::fs::create_dir_all(dest.join("usr"))?;
    std::fs::create_dir_all(&outside)?;
    // What a package's `usr/x -> <outside>` entry would leave behind.
    std::os::unix::fs::symlink(&outside, dest.join("usr/x"))?;
    std::fs::write(dest.join("usr/file"), "f")?;

    let mut real = HashSet::new();
    assert!(!real_dirs(&dest, Path::new("usr/x/y"), &mut real)?);
    assert!(!real_dirs(&dest, Path::new("usr/x"), &mut real)?);
    assert!(!real_dirs(&dest, Path::new("usr/file/y"), &mut real)?);
    assert!(
        std::fs::read_dir(&outside)?.next().is_none(),
        "wrote outside"
    );

    assert!(real_dirs(
        &dest,
        Path::new("usr/lib64/pkgconfig"),
        &mut real
    )?);
    assert!(dest.join("usr/lib64/pkgconfig").is_dir());
    assert!(real.contains(Path::new("usr/lib64")));
    Ok(())
}

/// What `rpm` printed for dbus-broker (`tests/fixtures/scriptlets/record.sh`)
/// is what the header reader returns: the same scriptlet texts, and the
/// trigger the package carries.
#[test]
fn reads_scriptlets_and_triggers_as_rpm_prints_them() -> Result<()> {
    let dir = TempDir::new("meta");
    let key = fedora()?;
    let n = unpack(&testdata("dbus-broker-37-9.fc45.x86_64.rpm"), &key, &dir.0)?;
    let recorded =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/scriptlets/dbus-broker");
    let read = |f: &str| std::fs::read_to_string(recorded.join(f)).unwrap_or_default();
    let scripts = &n.meta.scripts;
    let body = |phase| {
        scripts
            .phases
            .iter()
            .find(|(p, _)| *p == phase)
            .map(|(_, s)| s.body.trim().to_owned())
    };
    assert_eq!(body(Phase::Post), Some(read("post").trim().to_owned()));
    assert_eq!(body(Phase::Preun), Some(read("preun").trim().to_owned()));
    // An empty %postun with an interpreter is still there, and empty.
    assert_eq!(body(Phase::Postun), Some(String::new()));
    assert_eq!(scripts.phases.len(), 3);
    assert_eq!(scripts.triggers.len(), 1);
    let t = &scripts.triggers[0];
    assert_eq!(
        (t.target.as_str(), t.kind.as_str()),
        ("dbus-daemon", "postun")
    );
    assert_eq!(t.script.body.trim(), read("trigger.0").trim());
    Ok(())
}

#[test]
fn lists_only_files_outside_usr() -> Result<()> {
    let dir = TempDir::new("outside");
    let n = unpack(&testdata(RPM), &fedora()?, &dir.0)?;
    assert!(n.meta.files.is_empty(), "{:?}", n.meta.files);
    assert!(n.meta.scripts.phases.is_empty());
    Ok(())
}
