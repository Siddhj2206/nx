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

/// Trusts root's files and the test's own.
fn only_uid() -> impl Fn(u32) -> bool {
    let me = uid();
    move |u| u == me
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
        image: Image::None,
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
    let got = activate(&l, &fake, 2, None, &only_uid())?;
    assert_eq!(
        got,
        Activated {
            backend: Backend::Sysext,
            fallback: None,
            services: Services::default()
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
    let err = activate(&l, &fake, 2, None, &only_uid()).err();
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
    let got = activate(&l, &runner, 2, None, &only_uid())?;
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
    let err = activate(&l, &fake, 1, Some(Backend::Sysext), &only_uid()).err();
    assert!(
        matches!(&err, Some(Error::Command { detail, .. }) if detail.contains("not merged over /usr")),
        "{err:?}"
    );
    assert_eq!(l.state.backend()?, None);
    // Nothing was live before, so nothing is now: no current, no link.
    assert_eq!(l.state.current_number()?, None);
    assert!(std::fs::symlink_metadata(l.link()).is_err());
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
    activate(&l, &fake, 1, Some(Backend::Prefix), &only_uid())?;
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert_eq!(l.state.backend()?, Some(Backend::Prefix));

    // Switching from sysext to prefix removes the link and refreshes.
    activate(&l, &fake, 1, Some(Backend::Sysext), &only_uid())?;
    let fake = Fake::new();
    activate(&l, &fake, 2, Some(Backend::Prefix), &only_uid())?;
    assert_eq!(fake.calls(), ["systemd-sysext refresh"]);
    assert!(std::fs::symlink_metadata(l.link()).is_err());
    // Unforced, the next activation tries sysext again.
    let fake = Fake::new();
    let got = activate(&l, &fake, 1, None, &only_uid())?;
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
    assert!(activate(&l, &fake, 1, None, &only_uid()).is_err());
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
    let err = activate(&l, &Fake::new(), 1, Some(Backend::Sysext), &only_uid()).err();
    assert!(matches!(err, Some(Error::Io { .. })), "{err:?}");
    Ok(())
}

fn refused(l: &Layout, allowed: &dyn Fn(u32) -> bool) -> String {
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
    validate(&l, 1, &only_uid())?;

    // Someone else's files (root's are always fine, so a root test run
    // can't show this).
    if uid() != 0 {
        assert!(refused(&l, &|u| u == uid() + 1).contains("owned by uid"));
    }
    // setuid, world-writable.
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o4755)).map_err(io)?;
    assert!(refused(&l, &only_uid()).contains("setuid"));
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o666)).map_err(io)?;
    assert!(refused(&l, &only_uid()).contains("writable by everyone"));
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
    assert!(refused(&l, &only_uid()).contains("special file"));
    std::fs::remove_file(&fifo).map_err(io)?;
    validate(&l, 1, &only_uid())?;

    // The extension must extend this system.
    let release = l
        .state
        .root()
        .join("generations/1/sysext")
        .join(EXTENSION_RELEASE);
    std::fs::write(&release, "ID=fedora\nVERSION_ID=44\n").map_err(io)?;
    assert!(refused(&l, &only_uid()).contains("extends fedora 44"));

    // A generation that doesn't exist, or whose tree is a symlink.
    assert!(matches!(
        validate(&l, 9, &only_uid()),
        Err(Error::Refused { .. })
    ));
    let real = l.state.root().join("generations/2/sysext");
    let moved = l.state.root().join("generations/2/real");
    std::fs::rename(&real, &moved).map_err(io)?;
    std::os::unix::fs::symlink("/etc", &real).map_err(io)?;
    assert!(matches!(
        validate(&l, 2, &only_uid()),
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
    validate(&l, 1, &only_uid())?;
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

#[test]
fn wheel_members_are_listed_and_primary() -> Result<()> {
    let tmp = Tmp::new("wheel");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    std::fs::create_dir_all(tmp.0.join("etc")).map_err(io)?;
    std::fs::write(
        tmp.0.join("etc/group"),
        "root:x:0:\nwheel:x:10:alice,bob\nusers:x:100:\n",
    )
    .map_err(io)?;
    std::fs::write(
        tmp.0.join("etc/passwd"),
        "root:x:0:0::/root:/bin/sh\nalice:x:1000:1000::/h:/bin/sh\nbob:x:1001:100::/h:/bin/sh\n\
         carol:x:1002:100::/h:/bin/sh\ndave:x:1003:10::/h:/bin/sh\n",
    )
    .map_err(io)?;
    let mut uids = wheel_uids(&tmp.0);
    uids.sort_unstable();
    assert_eq!(uids, [1000, 1001, 1003]);
    assert!(wheel_uids(&tmp.0.join("nowhere")).is_empty());
    Ok(())
}

#[test]
fn the_image_is_checked_again_at_activation() -> Result<()> {
    let tmp = Tmp::new("image");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let mut l = layout(&tmp, false).map_err(io)?;
    let image = tmp.0.join("image");
    std::fs::create_dir_all(image.join("usr/bin")).map_err(io)?;
    l.image = Image::Root(image.clone());
    validate(&l, 1, &only_uid())?;
    // The image now ships the file generation 1 carries.
    std::fs::write(image.join("usr/bin/tool"), "image").map_err(io)?;
    assert!(refused(&l, &only_uid()).contains("the image already has /usr/bin/tool"));
    let fake = Fake::new();
    assert!(activate(&l, &fake, 1, None, &only_uid()).is_err());
    assert!(fake.calls().is_empty(), "nothing ran");
    l.image = Image::Unreadable("rpm-ostree is broken".into());
    assert!(refused(&l, &only_uid()).contains("can't read the booted image"));
    Ok(())
}

type Answer = Box<dyn Fn(&str) -> Option<Output>>;

/// Answers by command line and logs every call as `program arg arg`.
struct Scripted {
    calls: RefCell<Vec<String>>,
    answer: Answer,
}

impl Scripted {
    fn new(answer: impl Fn(&str) -> Option<Output> + 'static) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            answer: Box::new(answer),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

fn said(stdout: &str) -> Output {
    Output {
        success: true,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

impl Runner for Scripted {
    fn run(&self, program: &str, args: &[&OsStr]) -> std::io::Result<Output> {
        let line = std::iter::once(program)
            .chain(args.iter().filter_map(|a| a.to_str()))
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.borrow_mut().push(line.clone());
        if let Some(out) = (self.answer)(&line) {
            return Ok(out);
        }
        Ok(said(if line.starts_with("systemd-sysext status") {
            r#"[{"hierarchy":"/usr","extensions":["nx"]}]"#
        } else {
            ""
        }))
    }
}

/// Gives generation `n` a package `name` at `nevra` with a unit file and
/// intents, as `nx add` would have recorded.
fn with_service(l: &Layout, n: u32, name: &str, nevra: &str, unit: &str) -> std::io::Result<()> {
    let dir = l.state.root().join(format!("generations/{n}"));
    let units = dir.join("sysext/usr/lib/systemd/system");
    std::fs::create_dir_all(&units)?;
    std::fs::write(units.join(unit), "[Service]\nExecStart=/usr/bin/tool\n")?;
    let sysusers = dir.join("sysext/usr/lib/sysusers.d");
    std::fs::create_dir_all(&sysusers)?;
    std::fs::write(sysusers.join("svc.conf"), "u svc - \"Svc\" - -\n")?;
    std::fs::write(
        dir.join("activation.toml"),
        format!(
            "version = 1\n\n[[package]]\nname = \"{name}\"\nnevra = \"{nevra}\"\n\
             sysusers_files = [\"svc.conf\"]\n\n\
             [[package.preset]]\nscope = \"system\"\nname = \"{unit}\"\n\n\
             [[package.remove]]\nscope = \"system\"\nname = \"{unit}\"\n\n\
             [[package.restart]]\nscope = \"system\"\nname = \"{unit}\"\n"
        ),
    )
}

fn index(calls: &[String], prefix: &str) -> Option<usize> {
    calls.iter().position(|c| c.starts_with(prefix))
}

#[test]
fn a_package_that_appears_gets_users_and_presets_after_the_merge() -> Result<()> {
    let tmp = Tmp::new("appears");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let l = layout(&tmp, false).map_err(io)?;
    with_service(&l, 2, "svc", "svc-1-1.fc45.x86_64", "svc.service").map_err(io)?;
    let run = Scripted::new(|line| match line {
        "systemctl is-enabled svc.service" => Some(said("enabled\n")),
        "systemctl is-active svc.service" => Some(said("active\n")),
        _ => None,
    });
    let got = activate(&l, &run, 2, None, &only_uid())?;
    let calls = run.calls();
    let refresh = index(&calls, "systemd-sysext refresh");
    let users = index(&calls, "systemd-sysusers /usr/lib/sysusers.d/svc.conf");
    let preset = index(&calls, "systemctl preset svc.service");
    assert!(refresh < users && users < preset, "{calls:?}");
    assert_eq!(
        got.services.events,
        [
            "created users and groups from svc.conf",
            "enabled and started svc.service"
        ]
    );
    assert_eq!(got.services.warnings, Vec::<String>::new());
    Ok(())
}

#[test]
fn a_package_that_goes_is_stopped_before_its_files_do() -> Result<()> {
    let tmp = Tmp::new("goes");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let l = layout(&tmp, false).map_err(io)?;
    with_service(&l, 1, "svc", "svc-1-1.fc45.x86_64", "svc.service").map_err(io)?;
    let run = Scripted::new(|_| None);
    activate(&l, &run, 1, None, &only_uid())?;
    let run = Scripted::new(|_| None);
    let got = activate(&l, &run, 2, None, &only_uid())?;
    let calls = run.calls();
    let stop = index(&calls, "systemctl stop --no-block svc.service");
    let disable = index(
        &calls,
        "systemctl --no-reload disable --now --no-warn svc.service",
    );
    let refresh = index(&calls, "systemd-sysext refresh");
    assert!(stop < disable && disable < refresh, "{calls:?}");
    assert!(index(&calls, "systemctl preset").is_none(), "{calls:?}");
    assert_eq!(got.services.events, ["stopped and disabled svc.service"]);
    Ok(())
}

#[test]
fn a_new_version_is_restarted_not_enabled_again() -> Result<()> {
    let tmp = Tmp::new("upgrade");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let l = layout(&tmp, false).map_err(io)?;
    with_service(&l, 1, "svc", "svc-1-1.fc45.x86_64", "svc.service").map_err(io)?;
    with_service(&l, 2, "svc", "svc-2-1.fc45.x86_64", "svc.service").map_err(io)?;
    activate(&l, &Scripted::new(|_| None), 1, None, &only_uid())?;
    let run = Scripted::new(|_| None);
    activate(&l, &run, 2, None, &only_uid())?;
    let calls = run.calls();
    assert!(
        index(&calls, "systemctl try-restart svc.service").is_some(),
        "{calls:?}"
    );
    assert!(index(&calls, "systemctl preset").is_none(), "{calls:?}");
    assert!(index(&calls, "systemctl stop").is_none(), "{calls:?}");
    Ok(())
}

#[test]
fn only_units_the_extension_ships_are_acted_on() -> Result<()> {
    let tmp = Tmp::new("shipped");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let l = layout(&tmp, false).map_err(io)?;
    // The record names sshd.service, which generation 2 doesn't ship, and a
    // name that isn't a unit at all.
    with_service(&l, 2, "svc", "svc-1-1.fc45.x86_64", "svc.service").map_err(io)?;
    let path = l.state.root().join("generations/2/activation.toml");
    let text = std::fs::read_to_string(&path).map_err(io)?;
    std::fs::write(
        &path,
        text.replace("name = \"svc.service\"", "name = \"sshd.service\"")
            + "\n[[package.preset]]\nscope = \"system\"\nname = \"../../etc/x\"\n",
    )
    .map_err(io)?;
    let run = Scripted::new(|_| None);
    let got = activate(&l, &run, 2, None, &only_uid())?;
    let calls = run.calls();
    assert!(index(&calls, "systemctl preset").is_none(), "{calls:?}");
    assert!(
        got.services
            .warnings
            .iter()
            .any(|w| w.contains("sshd.service")),
        "{:?}",
        got.services.warnings
    );
    assert!(
        got.services
            .warnings
            .iter()
            .any(|w| w.contains("not a unit name")),
        "{:?}",
        got.services.warnings
    );
    Ok(())
}

#[test]
fn the_prefix_backend_sets_nothing_up_and_says_so() -> Result<()> {
    let tmp = Tmp::new("prefix-units");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let l = layout(&tmp, false).map_err(io)?;
    with_service(&l, 2, "svc", "svc-1-1.fc45.x86_64", "svc.service").map_err(io)?;
    let run = Scripted::new(|_| None);
    let got = activate(&l, &run, 2, Some(Backend::Prefix), &only_uid())?;
    assert!(
        index(&run.calls(), "systemctl").is_none(),
        "{:?}",
        run.calls()
    );
    assert!(
        got.services.warnings[0].contains("prefix backend"),
        "{:?}",
        got.services
    );
    Ok(())
}

#[test]
fn wheel_members_that_only_nss_knows_are_found() {
    let tmp = Tmp::new("nss");
    let run = Scripted::new(|line| match line {
        "getent group wheel" => Some(said("wheel:x:10:\n")),
        "id -G 5001" => Some(said("5001 10 100\n")),
        "id -G 5002" => Some(said("5002 100\n")),
        _ => Some(Output {
            success: false,
            ..Output::default()
        }),
    });
    let wheel = Wheel::new(&run, &tmp.0);
    assert!(wheel.is_member(5001));
    assert!(!wheel.is_member(5002));
    assert!(!wheel.is_member(5003));
    // Each uid is asked about once.
    assert!(wheel.is_member(5001));
    let asked = run.calls().iter().filter(|c| *c == "id -G 5001").count();
    assert_eq!(asked, 1);
}

#[test]
fn removing_a_template_disables_its_enabled_instances() -> Result<()> {
    let tmp = Tmp::new("template");
    let io = |e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    };
    let l = layout(&tmp, false).map_err(io)?;
    with_service(&l, 1, "svc", "svc-1-1.fc45.x86_64", "svc@.service").map_err(io)?;
    let path = l.state.root().join("generations/1/activation.toml");
    let text = std::fs::read_to_string(&path).map_err(io)?;
    // Presets name the template, removals its instances.
    let text = text.replace(
        "[[package.remove]]\nscope = \"system\"\nname = \"svc@.service\"",
        "[[package.remove]]\nscope = \"system\"\nname = \"svc@*.service\"",
    );
    std::fs::write(&path, text).map_err(io)?;
    let wants = l.root.join("etc/systemd/system/multi-user.target.wants");
    std::fs::create_dir_all(&wants).map_err(io)?;
    std::os::unix::fs::symlink(
        "/usr/lib/systemd/system/svc@.service",
        wants.join("svc@a.service"),
    )
    .map_err(io)?;
    std::os::unix::fs::symlink(
        "/usr/lib/systemd/system/other.service",
        wants.join("other.service"),
    )
    .map_err(io)?;
    activate(&l, &Scripted::new(|_| None), 1, None, &only_uid())?;
    let run = Scripted::new(|_| None);
    activate(&l, &run, 2, None, &only_uid())?;
    let calls = run.calls();
    assert!(
        index(&calls, "systemctl stop --no-block svc@*.service").is_some(),
        "{calls:?}"
    );
    assert!(
        index(
            &calls,
            "systemctl --no-reload disable --now --no-warn svc@a.service"
        )
        .is_some(),
        "{calls:?}"
    );
    Ok(())
}

#[test]
fn a_boot_that_did_not_merge_falls_back_to_prefix() -> Result<()> {
    let tmp = Tmp::new("settle");
    let l = layout(&tmp, false).map_err(|e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    })?;
    // No current generation, nothing linked: nothing to settle.
    assert_eq!(settle_at_boot(&l, &Fake::new())?, Settled::NothingToMerge);

    activate(&l, &Fake::new(), 1, Some(Backend::Sysext), &only_uid())?;
    assert_eq!(settle_at_boot(&l, &Fake::new())?, Settled::Merged);
    assert_eq!(l.state.backend()?, Some(Backend::Sysext));

    // The boot's merge failed: nothing of nx is listed.
    let mut unmerged = Fake::new();
    unmerged.merged = "[]";
    let got = settle_at_boot(&l, &unmerged)?;
    assert!(matches!(got, Settled::FellBack(_)), "{got:?}");
    assert_eq!(l.state.backend()?, Some(Backend::Prefix));
    assert!(std::fs::symlink_metadata(l.link()).is_err(), "link removed");
    assert_eq!(l.state.current_number()?, Some(1), "current is kept");

    // Settled: a second run has nothing linked, and the next activation
    // tries sysext again.
    assert_eq!(settle_at_boot(&l, &unmerged)?, Settled::NothingToMerge);
    let got = activate(&l, &Fake::new(), 1, None, &only_uid())?;
    assert_eq!(got.backend, Backend::Sysext);
    Ok(())
}

#[test]
fn settling_leaves_placeholders_and_forced_sysext_alone() -> Result<()> {
    let tmp = Tmp::new("settle-skip");
    let l = layout(&tmp, false).map_err(|e| Error::Io {
        path: tmp.0.clone(),
        source: e,
    })?;
    activate(&l, &Fake::new(), 1, Some(Backend::Sysext), &only_uid())?;
    let mut unmerged = Fake::new();
    unmerged.merged = "[]";

    let dir = l.state.root().join("generations/1");
    let mut manifest = Manifest::default();
    manifest.host.backend = Some(Backend::Sysext);
    let io = |source| Error::Io {
        path: dir.clone(),
        source,
    };
    std::fs::write(
        dir.join("manifest.toml"),
        manifest.to_toml().unwrap_or_default(),
    )
    .map_err(io)?;
    let got = settle_at_boot(&l, &unmerged)?;
    assert!(matches!(got, Settled::ForcedAndUnmerged(_)), "{got:?}");
    assert_eq!(l.state.backend()?, Some(Backend::Sysext));
    assert!(std::fs::symlink_metadata(l.link()).is_ok(), "link kept");

    let mut lock = Lock::new(None, "45", "x86_64");
    lock.pending = true;
    std::fs::write(dir.join("lock.toml"), lock.to_toml().unwrap_or_default()).map_err(io)?;
    assert_eq!(settle_at_boot(&l, &unmerged)?, Settled::NothingToMerge);
    Ok(())
}
