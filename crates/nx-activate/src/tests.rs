use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;

use nx_core::generation::EXTENSION_RELEASE;
use nx_core::{Lock, Manifest};

use super::*;

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nx-activate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The uid that owns what tests create.
fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(std::env::temp_dir()).map_or(0, |m| m.uid())
}

/// A layout in `tmp` with a fedora 45 root, and generations 1 and 2.
fn layout(tmp: &Tmp, selinux: bool) -> std::io::Result<Layout> {
    let root = tmp.0.join("root");
    std::fs::create_dir_all(root.join("usr/lib"))?;
    std::fs::write(
        root.join("usr/lib/os-release"),
        "ID=fedora\nVERSION_ID=45\n",
    )?;
    let state = State::new(tmp.0.join("state"));
    for n in 1..=2 {
        let dir = state.root().join(format!("generations/{n}"));
        let release = dir.join("sysext").join(EXTENSION_RELEASE);
        std::fs::create_dir_all(release.parent().unwrap_or(&dir))?;
        std::fs::write(release, "ID=fedora\nVERSION_ID=45\n")?;
        std::fs::create_dir_all(dir.join("sysext/usr/bin"))?;
        std::fs::write(dir.join("sysext/usr/bin/tool"), "x")?;
        std::fs::write(
            dir.join("manifest.toml"),
            Manifest::default().to_toml().unwrap_or_default(),
        )?;
        std::fs::write(
            dir.join("lock.toml"),
            Lock::new(None, "45", "x86_64")
                .to_toml()
                .unwrap_or_default(),
        )?;
    }
    let contexts = tmp.0.join("file_contexts");
    std::fs::write(&contexts, "")?;
    Ok(Layout {
        state,
        extensions: tmp.0.join("extensions"),
        root,
        file_contexts: selinux.then_some(contexts),
    })
}

/// A fake `setfiles` and `systemd-sysext`: logs every call, and fails the
/// ones `fail` names.
struct Fake {
    calls: RefCell<Vec<String>>,
    fail: Vec<&'static str>,
    /// What `status --json` says is merged.
    merged: &'static str,
}

impl Fake {
    fn new() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            fail: Vec::new(),
            merged: r#"[{"hierarchy":"/usr","extensions":["nx"],"since":1}]"#,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Runner for Fake {
    fn run(&self, program: &str, args: &[&OsStr]) -> std::io::Result<Output> {
        let first = args.first().and_then(|a| a.to_str()).unwrap_or("");
        let key = if program == "setfiles" {
            program
        } else {
            first
        };
        self.calls.borrow_mut().push(if program == "setfiles" {
            "setfiles".to_owned()
        } else {
            format!("{program} {first}")
        });
        if self.fail.contains(&key) {
            return Ok(Output {
                success: false,
                stdout: String::new(),
                stderr: format!("{key} exploded\n"),
            });
        }
        Ok(Output {
            success: true,
            stdout: if first == "status" {
                self.merged.to_owned()
            } else {
                String::new()
            },
            stderr: String::new(),
        })
    }
}

#[test]
fn the_first_activation_labels_links_merges_and_records_sysext() -> Result<()> {
    let tmp = Tmp::new("sysext");
    let l = layout(&tmp, true).map_err(|e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    })?;
    let fake = Fake::new();
    let got = activate(&l, &fake, 2, None, &[uid()])?;
    assert_eq!(
        got,
        Activated {
            backend: Backend::Sysext,
            fallback: None
        }
    );
    assert_eq!(
        fake.calls(),
        [
            "setfiles",
            "systemd-sysext refresh",
            "systemd-sysext status"
        ]
    );
    assert_eq!(l.state.current_number()?, Some(2));
    assert_eq!(l.state.backend()?, Some(Backend::Sysext));
    assert_eq!(
        std::fs::read_link(l.link()).ok(),
        Some(l.state.root().join("current/sysext"))
    );
    Ok(())
}

#[test]
fn a_failed_merge_falls_back_to_prefix_and_keeps_going() -> Result<()> {
    let tmp = Tmp::new("fallback");
    let l = layout(&tmp, false).map_err(|e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    })?;
    l.state.set_current(1)?;
    let mut fake = Fake::new();
    fake.fail = vec!["refresh"];
    // refresh always fails here, so even unmerging cannot succeed: the
    // fallback reports that instead of hiding it.
    let err = activate(&l, &fake, 2, None, &[uid()]).err();
    assert!(
        matches!(
            err,
            Some(Error::Command {
                program: "systemd-sysext",
                ..
            })
        ),
        "{err:?}"
    );
    assert_eq!(
        l.state.current_number()?,
        Some(1),
        "the old generation is back"
    );

    // A merge that fails once (the first, new one) and then works: the
    // link goes, the prefix is recorded, `current` is the new generation.
    struct FailsOnce(Fake, RefCell<bool>);
    impl Runner for FailsOnce {
        fn run(&self, program: &str, args: &[&OsStr]) -> std::io::Result<Output> {
            let mut failed = self.1.borrow_mut();
            let refresh = args.first().is_some_and(|a| *a == "refresh");
            if refresh && !*failed {
                *failed = true;
                self.0
                    .calls
                    .borrow_mut()
                    .push("systemd-sysext refresh".into());
                return Ok(Output {
                    success: false,
                    stdout: String::new(),
                    stderr: "maximum fs stacking depth exceeded\n".into(),
                });
            }
            self.0.run(program, args)
        }
    }
    let runner = FailsOnce(Fake::new(), RefCell::new(false));
    let got = activate(&l, &runner, 2, None, &[uid()])?;
    assert_eq!(got.backend, Backend::Prefix);
    assert_eq!(
        got.fallback.as_deref(),
        Some("maximum fs stacking depth exceeded")
    );
    assert_eq!(l.state.current_number()?, Some(2));
    assert_eq!(l.state.backend()?, Some(Backend::Prefix));
    assert!(std::fs::symlink_metadata(l.link()).is_err(), "link removed");
    Ok(())
}

#[test]
fn a_forced_sysext_does_not_fall_back() -> Result<()> {
    let tmp = Tmp::new("forced");
    let l = layout(&tmp, false).map_err(|e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    })?;
    let mut fake = Fake::new();
    fake.merged = "[]";
    let err = activate(&l, &fake, 1, Some(Backend::Sysext), &[uid()]).err();
    assert!(
        matches!(&err, Some(Error::Command { detail, .. }) if detail.contains("not merged over /usr")),
        "{err:?}"
    );
    assert_eq!(l.state.backend()?, None);
    Ok(())
}

#[test]
fn prefix_unmerges_only_what_was_linked() -> Result<()> {
    let tmp = Tmp::new("prefix");
    let l = layout(&tmp, false).map_err(|e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    })?;
    let fake = Fake::new();
    activate(&l, &fake, 1, Some(Backend::Prefix), &[uid()])?;
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert_eq!(l.state.backend()?, Some(Backend::Prefix));

    // Switching from sysext to prefix removes the link and refreshes.
    activate(&l, &fake, 1, Some(Backend::Sysext), &[uid()])?;
    let fake = Fake::new();
    activate(&l, &fake, 2, Some(Backend::Prefix), &[uid()])?;
    assert_eq!(fake.calls(), ["systemd-sysext refresh"]);
    assert!(std::fs::symlink_metadata(l.link()).is_err());
    // Unforced, the next activation tries sysext again.
    let fake = Fake::new();
    let got = activate(&l, &fake, 1, None, &[uid()])?;
    assert_eq!(got.backend, Backend::Sysext);
    assert_eq!(l.state.backend()?, Some(Backend::Sysext));
    Ok(())
}

#[test]
fn a_label_failure_stops_before_anything_changes() -> Result<()> {
    let tmp = Tmp::new("label");
    let l = layout(&tmp, true).map_err(|e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    })?;
    let mut fake = Fake::new();
    fake.fail = vec!["setfiles"];
    assert!(activate(&l, &fake, 1, None, &[uid()]).is_err());
    assert_eq!(l.state.current_number()?, None);
    assert!(std::fs::symlink_metadata(l.link()).is_err());
    assert_eq!(fake.calls(), ["setfiles"]);
    Ok(())
}

#[test]
fn it_never_replaces_a_real_directory_named_nx() -> Result<()> {
    let tmp = Tmp::new("realdir");
    let l = layout(&tmp, false).map_err(|e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    })?;
    std::fs::create_dir_all(l.link()).map_err(|source| Error::Io {
        path: l.link(),
        source,
    })?;
    let err = activate(&l, &Fake::new(), 1, Some(Backend::Sysext), &[uid()]).err();
    assert!(matches!(err, Some(Error::Io { .. })), "{err:?}");
    Ok(())
}

fn refused(l: &Layout, allowed: &[u32]) -> String {
    match validate(l, 1, allowed) {
        Err(Error::Refused { reason, .. }) => reason,
        other => format!("not refused: {other:?}"),
    }
}

#[test]
fn validation_refuses_what_root_must_not_merge() -> Result<()> {
    let tmp = Tmp::new("validate");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let l = layout(&tmp, false).map_err(io)?;
    let tool = l.state.root().join("generations/1/sysext/usr/bin/tool");
    validate(&l, 1, &[uid()])?;

    // Someone else's files (root's are always fine, so a root test run
    // can't show this).
    if uid() != 0 {
        assert!(refused(&l, &[uid() + 1]).contains("owned by uid"));
    }
    // setuid, world-writable.
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o4755)).map_err(io)?;
    assert!(refused(&l, &[uid()]).contains("setuid"));
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o666)).map_err(io)?;
    assert!(refused(&l, &[uid()]).contains("writable by everyone"));
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
    // A FIFO.
    let fifo = l.state.root().join("generations/1/sysext/usr/bin/fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .map_err(|e| io(e.into()))?;
    assert!(refused(&l, &[uid()]).contains("special file"));
    std::fs::remove_file(&fifo).map_err(io)?;
    validate(&l, 1, &[uid()])?;

    // The extension must extend this system.
    let release = l
        .state
        .root()
        .join("generations/1/sysext")
        .join(EXTENSION_RELEASE);
    std::fs::write(&release, "ID=fedora\nVERSION_ID=44\n").map_err(io)?;
    assert!(refused(&l, &[uid()]).contains("extends fedora 44"));

    // A generation that doesn't exist, or whose tree is a symlink.
    assert!(matches!(
        validate(&l, 9, &[uid()]),
        Err(Error::Refused { .. })
    ));
    let real = l.state.root().join("generations/2/sysext");
    let moved = l.state.root().join("generations/2/real");
    std::fs::rename(&real, &moved).map_err(io)?;
    std::os::unix::fs::symlink("/etc", &real).map_err(io)?;
    assert!(matches!(
        validate(&l, 2, &[uid()]),
        Err(Error::Refused { .. })
    ));
    Ok(())
}

#[test]
fn symlinks_in_the_tree_are_left_alone() -> Result<()> {
    let tmp = Tmp::new("links");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let l = layout(&tmp, false).map_err(io)?;
    let dir = l.state.root().join("generations/1/sysext/usr");
    std::os::unix::fs::symlink("/etc", dir.join("x")).map_err(io)?;
    std::os::unix::fs::symlink("../../../../../../etc/shadow", dir.join("y")).map_err(io)?;
    validate(&l, 1, &[uid()])?;
    Ok(())
}

#[test]
fn only_nx_under_usr_counts_as_merged() {
    assert!(merged_over_usr(
        r#"[{"hierarchy":"/opt","extensions":[]},{"hierarchy":"/usr","extensions":["x","nx"]}]"#
    ));
    assert!(!merged_over_usr(
        r#"[{"hierarchy":"/opt","extensions":["nx"]},{"hierarchy":"/usr","extensions":[]}]"#
    ));
    assert!(!merged_over_usr("not json"));
}

#[test]
fn finds_the_policys_file_contexts() -> Result<()> {
    let tmp = Tmp::new("contexts");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    assert_eq!(file_contexts(&tmp.0), None);
    std::fs::create_dir_all(tmp.0.join("sys/fs/selinux")).map_err(io)?;
    std::fs::write(tmp.0.join("sys/fs/selinux/enforce"), "1").map_err(io)?;
    std::fs::create_dir_all(tmp.0.join("etc/selinux/mls/contexts/files")).map_err(io)?;
    std::fs::write(
        tmp.0.join("etc/selinux/mls/contexts/files/file_contexts"),
        "",
    )
    .map_err(io)?;
    assert_eq!(file_contexts(&tmp.0), None, "targeted is the default");
    std::fs::write(tmp.0.join("etc/selinux/config"), "SELINUXTYPE=mls\n").map_err(io)?;
    assert!(file_contexts(&tmp.0).is_some());
    Ok(())
}
