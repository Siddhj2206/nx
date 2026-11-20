//! The pipeline end to end, without the network, Nix or nxd: a fixture
//! root (not bootc) with one repo, canned HTTP serving its metadata and a
//! real signed package, and a `Live` that only moves `current`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use nx_core::{DeploymentSource, Deployments, State};
use nx_rpm::Refresh;

use nx_http::Canned;

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
            http: Box::new(Canned::new(self.bodies.clone())),
            nix: Box::new(NoNix),
            live: Box::new(MoveCurrent),
            journal: nx_core::Journal::off(),
            shim: Some(self.dir.join("nx-shim")),
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

/// `tests/fixtures/nixpkgs`, recorded by its `record.sh`.
fn nixpkgs(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/nixpkgs")
        .join(name)
}

/// What `record.sh` recorded, and where nx asks for it: the channel (a
/// redirect to the release), the package list, hello's closure's narinfos.
/// Returns the HTTP, the release directory and what the closure downloads.
fn recorded_nixpkgs() -> Result<(Canned, String, u64)> {
    let at = std::fs::read_to_string(nixpkgs("git-revision.url"))?
        .trim()
        .to_owned();
    let release = at.strip_suffix("/git-revision").unwrap_or(&at).to_owned();
    let mut http = Canned::new([
        (at.clone(), std::fs::read(nixpkgs("git-revision"))?),
        (
            format!("{release}/packages.json.br"),
            std::fs::read(nixpkgs("packages.json.br"))?,
        ),
    ]);
    http.redirects.insert(
        format!("{}/git-revision", nx_nix::channel::CHANNEL),
        at.clone(),
    );
    let mut download = 0;
    for entry in std::fs::read_dir(nixpkgs("narinfo"))? {
        let path = entry?.path();
        let text = std::fs::read_to_string(&path)?;
        download += text
            .lines()
            .find_map(|l| l.strip_prefix("FileSize: "))
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or_default();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        http.bodies
            .insert(format!("{}/{name}", nx_nix::CACHE), text.into_bytes());
    }
    Ok((http, release, download))
}

/// `nix` over a store of its own: `eval` answers what was recorded,
/// `build --no-link` puts the store paths it is given there with the
/// commands `bins` names (as substituting would), and every call is kept.
struct FakeNix {
    root: PathBuf,
    eval: String,
    bins: Vec<(&'static str, &'static str)>,
    calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

impl nx_nix::Nix for FakeNix {
    fn run(&self, args: &[String], _: &mut dyn FnMut(&str)) -> nx_nix::Result<String> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(args.to_vec());
        }
        match args.first().map(String::as_str) {
            Some("eval") => Ok(self.eval.clone()),
            Some("build") if args.iter().any(|a| a == "--no-link") => {
                for path in args.iter().filter(|a| a.starts_with(nx_nix::STORE)) {
                    let dir = self.on_disk(path);
                    std::fs::create_dir_all(dir.join("bin")).map_err(nx_nix::Error::Spawn)?;
                    for (_, name) in self.bins.iter().filter(|(p, _)| p == path) {
                        let bin = dir.join("bin").join(name);
                        std::fs::write(&bin, "#!/bin/sh\n")
                            .and_then(|()| {
                                use std::os::unix::fs::PermissionsExt;
                                std::fs::set_permissions(
                                    &bin,
                                    std::fs::Permissions::from_mode(0o755),
                                )
                            })
                            .map_err(nx_nix::Error::Spawn)?;
                    }
                }
                Ok(String::new())
            }
            _ => Ok(String::new()),
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

/// A tool-tier package new to the pin: the channel says which release,
/// its package list loads, `nix eval` gives its store path, the narinfos
/// give its closure and size, `nix build` realises it and roots it in
/// the generation, which becomes current. Recorded answers throughout.
#[test]
fn a_tool_is_evaluated_planned_realised_and_rooted() -> Result<()> {
    // From eval.json: re-recording changes these.
    const HELLO: &str = "/nix/store/sd779x86bjaqxj2lzh2jsir4h0zjz6d1-hello-2.12.3";
    const VERSION: &str = "2.12.3";

    let f = Fixture::new("tool")?;
    std::fs::write(f.dir.join("nx-shim"), "the shim")?;
    let (http, release, download) = recorded_nixpkgs()?;
    let store = f.dir.join("nix-root");
    std::fs::create_dir_all(store.join("nix/store"))?;
    let calls = std::sync::Arc::default();
    let env = Env {
        http: Box::new(http),
        nix: Box::new(FakeNix {
            root: store.clone(),
            eval: std::fs::read_to_string(nixpkgs("eval.json"))?,
            bins: vec![(HELLO, "hello")],
            calls: std::sync::Arc::clone(&calls),
        }),
        ..f.env()
    };
    let mut ui = ui();
    let asked = vec!["hello".to_owned()];
    let mut s = Session::open_in(env, &mut ui, &f.machine, Refresh::IfMissing, false, |m| {
        m.add_tool(&asked);
        Ok(())
    })?;
    assert!(s.tool_available());
    let catalog = s.catalog(&mut ui)?;
    assert_eq!(
        catalog.find("hello").map(|p| p.version.as_str()),
        Some(VERSION)
    );
    assert!(catalog.find("ripgrep").is_some());

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
            name: "hello".into(),
            version: format!("{VERSION} · nixpkgs"),
        }]
    );
    // hello and everything it references: nothing is in this store yet.
    assert_eq!(outcome.tool_download, download);
    assert!(
        store.join(HELLO.trim_start_matches('/')).is_dir(),
        "realised"
    );

    let built = s.install(&mut ui, &outcome.all)?;
    drop(s);
    let n = built.generation.number;
    assert_eq!(f.state().current_number()?, Some(n));
    let tool = built.generation.lock.tool.as_ref();
    assert_eq!(
        tool.map(|t| t.url.as_str()),
        Some(release.as_str()),
        "pinned to the channel's release"
    );
    let hello = tool.and_then(|t| t.package("hello"));
    assert_eq!(hello.map(|p| p.version.as_str()), Some(VERSION));
    assert_eq!(
        hello.map(|p| p.paths().collect::<Vec<_>>()),
        Some(vec![HELLO])
    );
    // Its command is exposed in the generation.
    let exposed = built.generation.dir.join("bin/hello");
    assert!(exposed.is_file(), "{exposed:?}");

    let calls = calls.lock().map(|c| c.clone()).unwrap_or_default();
    let verbs: Vec<&str> = calls
        .iter()
        .map(|c| match c.get(1).map(String::as_str) {
            Some("--no-link") => "realise",
            Some("--out-link") => "root",
            _ => c[0].as_str(),
        })
        .collect();
    assert_eq!(verbs, ["eval", "realise", "root"]);
    assert_eq!(calls[0][3], format!("{release}/nixexprs.tar.xz"));
    assert_eq!(calls[1].last().map(String::as_str), Some(HELLO));
    assert_eq!(
        calls[2][2],
        built
            .generation
            .gcroots()
            .join("root")
            .display()
            .to_string()
    );
    assert_eq!(calls[2].last().map(String::as_str), Some(HELLO));
    Ok(())
}

#[test]
fn a_change_refreshes_expired_metadata_and_a_read_mentions_it_after_a_day() -> Result<()> {
    let f = Fixture::new("expire")?;
    // As Fedora's updates repo: stale after 6 h.
    let repo_file = f.machine.root.join("etc/yum.repos.d/fedora.repo");
    let text = std::fs::read_to_string(&repo_file)?;
    std::fs::write(&repo_file, format!("{text}metadata_expire=6h\n"))?;
    let cache = f.dir.join("cache");
    let http = Canned::new(f.bodies.clone());
    let load = |refresh| -> Result<String> {
        let err = SharedBuf::default();
        let mut ui = Ui::plain(SharedBuf::default(), err.clone());
        crate::host::load_from(
            &mut ui,
            &f.machine,
            refresh,
            &nx_core::Manifest::default(),
            &http,
            &nx_core::Journal::off(),
            cache.clone(),
        )?;
        Ok(err.text())
    };
    let age = |hours: u64| -> Result<()> {
        let repomd = cache.join("repodata/fedora/repomd.xml");
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(hours * 3600);
        std::fs::File::options()
            .write(true)
            .open(repomd)?
            .set_modified(then)?;
        Ok(())
    };
    load(Refresh::IfMissing)?;
    http.clear();

    // Past its expiry but under a day: a read answers from the cache and
    // says nothing.
    age(7)?;
    assert_eq!(load(Refresh::IfMissing)?, "");
    assert!(http.hits().is_empty());
    // A change checks repomd.xml; primary is the same, so not fetched again.
    let err = load(Refresh::IfStale)?;
    assert_eq!(http.hits(), [format!("{REPO}repodata/repomd.xml")]);
    assert_eq!(err, "  ↓ fedora metadata\n");
    http.clear();
    // Checked just now: nothing to fetch.
    load(Refresh::IfStale)?;
    assert!(http.hits().is_empty());

    // Over a day (the timer's refreshes failed): a read says so.
    age(25)?;
    assert_eq!(
        load(Refresh::IfMissing)?,
        "  ! metadata for fedora is more than a day old; --refresh updates it\n"
    );
    // Offline, a change uses the cache and says why.
    let offline = Canned::default();
    let err = SharedBuf::default();
    let mut ui = Ui::plain(SharedBuf::default(), err.clone());
    crate::host::load_from(
        &mut ui,
        &f.machine,
        Refresh::IfStale,
        &nx_core::Manifest::default(),
        &offline,
        &nx_core::Journal::off(),
        cache.clone(),
    )?;
    assert!(
        err.text().contains("! using cached metadata for fedora:"),
        "{}",
        err.text()
    );
    Ok(())
}
