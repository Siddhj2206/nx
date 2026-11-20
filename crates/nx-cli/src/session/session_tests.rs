//! The pipeline end to end, without the network, Nix or nxd: a fixture
//! root (not bootc) with one repo, canned HTTP serving its metadata and a
//! real signed package, and a `Live` that only moves `current`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use nx_core::{DeploymentSource, Deployments, State};
use nx_rpm::Refresh;

use super::{Env, How, Session};
use crate::changes::Change;
use crate::host::Machine;
use crate::ui::{SharedBuf, Ui};

const RPM: &str = "libxkbcommon-devel-1.13.1-4.fc45.x86_64.rpm";
const REPO: &str = "https://repo.test/";

/// Puts a recorded rpmdb (`tests/fixtures/rpmdb/<name>`) into `root`, at
/// both places librpm may look: Fedora's `/usr/lib/sysimage/rpm` and an
/// ostree system's `/usr/share/rpm`. rpm 6 won't create one unprivileged.
fn install_rpmdb(root: &Path, name: &str) -> std::io::Result<()> {
    let db = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/rpmdb")
        .join(name)
        .join("rpmdb.sqlite");
    for dir in ["usr/lib/sysimage/rpm", "usr/share/rpm"] {
        std::fs::create_dir_all(root.join(dir))?;
        std::fs::copy(&db, root.join(dir).join("rpmdb.sqlite"))?;
    }
    Ok(())
}

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/rpm")
        .join(name)
}

/// Canned answers by URL; anything else is a 404. Records what was asked.
#[derive(Default)]
struct Canned {
    bodies: HashMap<String, Vec<u8>>,
    asked: Mutex<Vec<String>>,
}

impl nx_rpm::Http for Canned {
    fn get(&self, url: &str) -> nx_rpm::Result<Vec<u8>> {
        if let Ok(mut a) = self.asked.lock() {
            a.push(url.to_owned());
        }
        self.bodies
            .get(url)
            .cloned()
            .ok_or_else(|| nx_rpm::Error::Fetch {
                url: url.to_owned(),
                reason: "404".into(),
            })
    }
}

/// No Nix here: a host-tier change never runs it.
struct NoNix;

impl nx_nix::Nix for NoNix {
    fn run(&self, args: &[String], _: &mut dyn FnMut(&str)) -> nx_nix::Result<String> {
        Err(nx_nix::Error::Invalid(format!(
            "nix {} ran",
            args.join(" ")
        )))
    }
}

/// Makes a generation current, as nxd would, without merging anything.
struct MoveCurrent;

impl crate::live::Live for MoveCurrent {
    fn activate(&self, _: &mut Ui, state: &State, n: u32) -> Result<()> {
        Ok(state.set_current(n)?)
    }
}

struct NotBootc;

impl DeploymentSource for NotBootc {
    fn deployments(&self) -> nx_core::Result<Deployments> {
        Err(nx_core::Error::Invalid("not bootc".into()))
    }
}

/// A Fedora 45 root with one repo, `fedora`, that has `libxkbcommon-devel`
/// and is signed with Fedora's key; the HTTP that serves it.
struct Fixture {
    dir: PathBuf,
    machine: Machine,
    bodies: HashMap<String, Vec<u8>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = nx_core::state::remove_tree(&self.dir);
    }
}

impl Fixture {
    fn new(tag: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("nx-session-{tag}-{}", std::process::id()));
        let _ = nx_core::state::remove_tree(&dir);
        let root = dir.join("root");
        for d in ["usr/lib", "etc/yum.repos.d", "etc/pki/rpm-gpg"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        // As tmpfiles makes it: a dry run takes no lock and creates nothing,
        // and without it would share the user's own cache with other tests.
        std::fs::create_dir_all(dir.join("state"))?;
        std::fs::write(
            root.join("usr/lib/os-release"),
            "ID=fedora\nVERSION_ID=45\n",
        )?;
        std::fs::copy(
            testdata("RPM-GPG-KEY-fedora-45-primary"),
            root.join("etc/pki/rpm-gpg/fedora"),
        )?;
        std::fs::write(
            root.join("etc/yum.repos.d/fedora.repo"),
            format!(
                "[fedora]\nbaseurl={REPO}\ngpgcheck=1\ngpgkey=file:///etc/pki/rpm-gpg/fedora\n"
            ),
        )?;
        // An empty rpmdb: the image has nothing. Without one, loading the
        // installed set fails (before 1.0.1 it read the running system's).
        install_rpmdb(&root, "empty")?;
        let rpm = std::fs::read(testdata(RPM))?;
        let sha = nx_rpm::fetch::sha256_hex(&rpm);
        let primary = format!(
            r#"<?xml version="1.0"?><metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="1"><package type="rpm"><name>libxkbcommon-devel</name><arch>x86_64</arch><version epoch="0" ver="1.13.1" rel="4.fc45"/><checksum type="sha256" pkgid="YES">{sha}</checksum><summary>Development files for libxkbcommon</summary><size package="{}" installed="200000" archive="200000"/><location href="Packages/l/{RPM}"/><format><rpm:sourcerpm>libxkbcommon-1.13.1-4.fc45.src.rpm</rpm:sourcerpm><rpm:provides><rpm:entry name="libxkbcommon-devel" flags="EQ" epoch="0" ver="1.13.1" rel="4.fc45"/></rpm:provides></format></package></metadata>"#,
            rpm.len()
        );
        let primary_sum = nx_rpm::fetch::sha256_hex(primary.as_bytes());
        let repomd = format!(
            r#"<repomd><revision>1</revision><data type="primary"><checksum type="sha256">{primary_sum}</checksum><location href="repodata/{primary_sum}-primary.xml"/></data></repomd>"#
        );
        let bodies = HashMap::from([
            (format!("{REPO}repodata/repomd.xml"), repomd.into_bytes()),
            (
                format!("{REPO}repodata/{primary_sum}-primary.xml"),
                primary.into_bytes(),
            ),
            (format!("{REPO}Packages/l/{RPM}"), rpm),
        ]);
        let mut machine = Machine::read(&root, &NotBootc)?;
        // The package's arch, whatever this test runs on.
        machine.arch = "x86_64".into();
        Ok(Self {
            dir,
            machine,
            bodies,
        })
    }

    fn state(&self) -> State {
        State::new(self.dir.join("state"))
    }

    fn env(&self) -> Env {
        Env {
            state: self.state(),
            http: Box::new(Canned {
                bodies: self.bodies.clone(),
                ..Canned::default()
            }),
            nix: Box::new(NoNix),
            live: Box::new(MoveCurrent),
        }
    }
}

fn ui() -> Ui {
    Ui::plain(SharedBuf::default(), SharedBuf::default())
}

fn devel() -> Vec<String> {
    vec!["libxkbcommon-devel".to_owned()]
}

#[test]
fn a_host_package_is_planned_built_made_live_and_removed() -> Result<()> {
    let f = Fixture::new("add")?;
    let mut ui = ui();
    let asked = devel();

    let mut s = Session::open_in(
        f.env(),
        &mut ui,
        &f.machine,
        Refresh::IfMissing,
        false,
        |m| {
            m.add_host(&asked);
            Ok(())
        },
    )?;
    let outcome = s.plan(
        &mut ui,
        &How {
            asked: &asked,
            ..How::default()
        },
    )?;
    assert_eq!(
        outcome.changes,
        [Change::Added {
            name: "libxkbcommon-devel".into(),
            version: "1.13.1-4.fc45".into()
        }]
    );
    assert_eq!(outcome.download(), 76_110);
    let built = s.install(&mut ui, &outcome.all)?;
    drop(s);
    let n = built.generation.number;
    assert_eq!(f.state().current_number()?, Some(n));
    assert!(
        built
            .generation
            .sysext()
            .join("usr/include/xkbcommon/xkbcommon.h")
            .is_file()
    );
    let lock = &built.generation.lock;
    assert_eq!(
        lock.host[0].nevra,
        "libxkbcommon-devel-1.13.1-4.fc45.x86_64"
    );
    assert_eq!(lock.host[0].requested, asked);
    assert_eq!(lock.parent, None);

    // The same again changes nothing; taking it out removes it.
    let mut s = Session::open_in(
        f.env(),
        &mut ui,
        &f.machine,
        Refresh::IfMissing,
        false,
        |_| Ok(()),
    )?;
    assert!(s.plan(&mut ui, &How::default())?.changes.is_empty());
    s.edit(|m| {
        m.remove_host(&asked);
    })?;
    let outcome = s.plan(&mut ui, &How::default())?;
    assert!(
        matches!(&outcome.changes[..], [Change::Removed { name, .. }] if name == "libxkbcommon-devel"),
        "{:?}",
        outcome.changes
    );
    let built = s.install(&mut ui, &outcome.all)?;
    assert!(built.generation.lock.host.is_empty());
    assert_eq!(built.generation.lock.parent, Some(n));
    Ok(())
}

#[test]
fn a_dry_run_plans_and_changes_nothing() -> Result<()> {
    let f = Fixture::new("dry")?;
    let mut ui = ui();
    let asked = devel();
    let mut s = Session::open_in(
        f.env(),
        &mut ui,
        &f.machine,
        Refresh::IfMissing,
        true,
        |m| {
            m.add_host(&asked);
            Ok(())
        },
    )?;
    let outcome = s.plan(
        &mut ui,
        &How {
            asked: &asked,
            ..How::default()
        },
    )?;
    assert_eq!(outcome.changes.len(), 1);
    assert!(
        s.install(&mut ui, &outcome.all).is_err(),
        "a dry run can't build"
    );
    assert_eq!(f.state().generations()?, Vec::<u32>::new());
    Ok(())
}

#[test]
fn repos_change_only_when_the_session_opens() -> Result<()> {
    let f = Fixture::new("edit")?;
    let mut ui = ui();
    let mut s = Session::open_in(
        f.env(),
        &mut ui,
        &f.machine,
        Refresh::IfMissing,
        true,
        |_| Ok(()),
    )?;
    let err = s.edit(|m| {
        m.repos.push(nx_core::RepoEntry {
            name: "copr:a/b".into(),
            baseurl: None,
            metalink: None,
            gpgkey: None,
            priority: None,
        });
    });
    assert!(err.is_err());
    Ok(())
}
