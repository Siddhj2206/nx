//! The corpus: real scriptlets recorded from Fedora 45 packages and
//! tailscale's own repo (`tests/fixtures/scriptlets/record.sh`), and what
//! the translator makes of each.

use std::path::{Path, PathBuf};

use super::*;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/scriptlets")
}

/// Reads a recorded package the way nx-librpm reads a header.
fn load(name: &str) -> Scripts {
    let dir = fixtures().join(name);
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
    let script = |file: &str| {
        let body = read(file).unwrap_or_default();
        let interpreter = read(&format!("{file}.prog")).unwrap_or_default();
        let interpreter = interpreter.trim().to_owned();
        (!body.is_empty() || !interpreter.is_empty()).then_some(Script {
            interpreter: if interpreter == "/bin/sh" {
                String::new()
            } else {
                interpreter
            },
            body,
        })
    };
    let files = [
        (Phase::Pretrans, "pretrans"),
        (Phase::Pre, "pre"),
        (Phase::Post, "post"),
        (Phase::Preun, "preun"),
        (Phase::Postun, "postun"),
        (Phase::Posttrans, "posttrans"),
        (Phase::Preuntrans, "preuntrans"),
        (Phase::Postuntrans, "postuntrans"),
    ];
    let phases = files
        .iter()
        .filter_map(|(p, f)| Some((*p, script(f)?)))
        .collect();
    let triggers = |list: &str, prefix: &str| -> Vec<Trigger> {
        read(list)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                Some(Trigger {
                    target: f.first()?.to_string(),
                    kind: f.get(1)?.to_string(),
                    script: script(&format!("{prefix}.{}", f.get(3)?))?,
                })
            })
            .collect()
    };
    Scripts {
        phases,
        triggers: triggers("triggers.tsv", "trigger"),
        file_triggers: triggers("filetriggers.tsv", "filetrigger"),
    }
}

fn nothing_installed() -> Context<'static> {
    Context {
        present: &|_| false,
        removable: &|_| false,
    }
}

fn unit(scope: Scope, name: &str) -> UnitRef {
    UnitRef {
        scope,
        name: name.into(),
    }
}

fn sources(t: &Translation) -> Vec<&str> {
    t.leftovers.iter().map(|l| l.source.as_str()).collect()
}

#[test]
fn syncthing_is_presets_removals_and_restarts() {
    let t = translate(&load("syncthing"), &nothing_installed());
    assert_eq!(t.leftovers, []);
    assert_eq!(
        t.intents,
        Intents {
            preset: vec![
                unit(Scope::System, "syncthing@.service"),
                unit(Scope::User, "syncthing.service")
            ],
            remove: vec![
                unit(Scope::System, "syncthing@*.service"),
                unit(Scope::User, "syncthing.service")
            ],
            restart: vec![
                unit(Scope::System, "syncthing@*.service"),
                unit(Scope::User, "syncthing.service")
            ],
            ..Intents::default()
        }
    );
}

#[test]
fn plain_service_packages_translate() {
    for (pkg, unit_name) in [
        ("bluez", "bluetooth.service"),
        ("polkit", "polkit.service"),
        ("geoclue2", "geoclue.service"),
        ("dbus-broker", "dbus-broker.service"),
    ] {
        let t = translate(&load(pkg), &nothing_installed());
        assert_eq!(t.leftovers, [], "{pkg}");
        assert!(
            t.intents.preset.contains(&unit(Scope::System, unit_name)),
            "{pkg}: {:?}",
            t.intents
        );
        assert!(
            t.intents.remove.contains(&unit(Scope::System, unit_name)),
            "{pkg}"
        );
    }
}

#[test]
fn a_package_without_scriptlets_is_nothing() {
    let t = translate(&load("podman"), &nothing_installed());
    assert_eq!(t, Translation::default());
}

#[test]
fn scriptlets_that_edit_the_system_are_refused_with_their_text() {
    for (pkg, source) in [
        ("avahi", "%post"),
        ("chrony", "%post"),
        ("openssh-server", "%post"),
        ("firewalld", "%posttrans"),
        ("tuned", "%post"),
        ("libvirt-daemon", "%pre"),
        ("systemd-resolved", "%post"),
        ("tailscale", "%post"),
    ] {
        let t = translate(&load(pkg), &nothing_installed());
        assert!(sources(&t).contains(&source), "{pkg}: {:?}", t.leftovers);
    }
    // What was recognised around the leftover is still understood.
    let t = translate(&load("avahi"), &nothing_installed());
    assert_eq!(sources(&t), ["%post"]);
    assert!(
        t.leftovers[0].lines[0].starts_with("/usr/bin/dbus-send"),
        "{:?}",
        t.leftovers
    );
    assert_eq!(t.intents.preset.len(), 2);
}

#[test]
fn tailscales_hand_written_removal_scriptlets_translate() {
    let t = translate(&load("tailscale"), &nothing_installed());
    assert_eq!(sources(&t), ["%post"]);
    assert_eq!(
        t.intents.remove,
        [unit(Scope::System, "tailscaled.service")]
    );
    assert_eq!(
        t.intents.restart,
        [unit(Scope::System, "tailscaled.service")]
    );
}

#[test]
fn a_trigger_on_a_package_that_cannot_fire_is_inert() {
    // dbus-broker's trigger watches the removal of dbus-daemon, which is
    // in the image and which nx never removes.
    let t = translate(&load("dbus-broker"), &nothing_installed());
    assert_eq!(t.leftovers, []);
    let present = Context {
        present: &|_| true,
        removable: &|_| false,
    };
    assert_eq!(translate(&load("dbus-broker"), &present).leftovers, []);
    // If nx could remove it, the trigger would run, and it isn't known.
    let removable = Context {
        present: &|_| true,
        removable: &|n| n == "dbus-daemon",
    };
    let t = translate(&load("dbus-broker"), &removable);
    assert_eq!(sources(&t), ["%triggerpostun -- dbus-daemon"]);
}

#[test]
fn a_trigger_on_an_installed_package_is_matched_like_any_script() {
    let t = translate(&load("cups"), &nothing_installed());
    assert_eq!(t.leftovers, []);
    let with_samba = Context {
        present: &|n| n == "samba-client",
        removable: &|n| n == "samba-client",
    };
    let t = translate(&load("cups"), &with_samba);
    assert_eq!(
        sources(&t),
        ["%triggerin -- samba-client", "%triggerun -- samba-client"]
    );
}

#[test]
fn the_digest_follows_the_leftover_text() {
    let a = translate(&load("avahi"), &nothing_installed());
    let b = translate(&load("chrony"), &nothing_installed());
    assert_eq!(a.digest(), a.digest());
    assert_ne!(a.digest(), b.digest());
    assert_eq!(a.digest().len(), 64);
}

fn one(body: &str) -> Translation {
    let scripts = Scripts {
        phases: vec![(
            Phase::Post,
            Script {
                interpreter: String::new(),
                body: body.into(),
            },
        )],
        ..Scripts::default()
    };
    translate(&scripts, &nothing_installed())
}

#[test]
fn old_style_macro_bodies_translate() {
    let t = one(
        "if [ $1 -eq 1 ] ; then \n    # Initial installation \n    systemctl preset foo.service >/dev/null 2>&1 || : \nfi",
    );
    assert_eq!(t.leftovers, []);
    assert_eq!(t.intents.preset, [unit(Scope::System, "foo.service")]);
    let t =
        one("if [ $1 -eq 1 ] ; then\n systemctl --no-reload --global preset foo.service || :\nfi");
    assert_eq!(t.intents.preset, [unit(Scope::User, "foo.service")]);
}

#[test]
fn a_guard_that_does_not_match_the_command_is_refused() {
    // A stop on every run, upgrades included, is not %systemd_preun.
    assert!(
        !one("systemctl --no-reload disable --now foo.service")
            .leftovers
            .is_empty()
    );
    // A preset only on removal never happens.
    assert!(
        !one("if [ $1 -eq 0 ]; then\n systemctl preset foo.service\nfi")
            .leftovers
            .is_empty()
    );
}

#[test]
fn legacy_user_creation_becomes_sysusers_lines() {
    let t = one("getent group foo >/dev/null || groupadd -r foo\n\
         getent passwd foo >/dev/null || \\\n    useradd -r -g foo -d /var/lib/foo -s /sbin/nologin -c \"Foo daemon\" foo\n");
    assert_eq!(t.leftovers, []);
    assert_eq!(
        t.intents.sysusers,
        [
            "g foo -",
            "u foo -:foo \"Foo daemon\" /var/lib/foo /sbin/nologin"
        ]
    );
}

#[test]
fn anything_with_shell_logic_is_left_over() {
    for body in [
        "echo \"foo\" >> /etc/foo.conf",
        "sed -i s/a/b/ /etc/foo",
        "update-alternatives --install /usr/bin/x x /usr/bin/y 10",
        "for f in /etc/*; do rm $f; done",
        "if [ $1 -eq 1 ]; then\n systemctl enable foo.service\nfi",
        "useradd -m foo",
        "semodule -i foo.pp",
        "depmod -a",
    ] {
        assert!(!one(body).leftovers.is_empty(), "{body}");
    }
}

#[test]
fn caches_and_the_linker_are_skipped_not_run() {
    let t =
        one("/sbin/ldconfig\ngtk-update-icon-cache -f /usr/share/icons/hicolor &>/dev/null || :\n");
    assert_eq!(t.leftovers, []);
    assert_eq!(t.intents.skipped, ["ldconfig", "gtk-update-icon-cache"]);
}

#[test]
fn another_interpreter_means_the_whole_body_is_left_over() {
    let scripts = Scripts {
        phases: vec![(
            Phase::Post,
            Script {
                interpreter: "<lua>".into(),
                body: "posix.exec('x')".into(),
            },
        )],
        ..Scripts::default()
    };
    let t = translate(&scripts, &nothing_installed());
    assert_eq!(sources(&t), ["%post (-p <lua>)"]);
}

#[test]
fn unit_names_are_checked() {
    for good in [
        "a.service",
        "a@.service",
        "a@*.service",
        "a-b_c.socket",
        "x.timer",
    ] {
        assert!(valid_unit(good), "{good}");
    }
    for bad in [
        "",
        ".service",
        "../x.service",
        "a/b.service",
        "a b.service",
        "*.service",
        "a*.service",
        "a.mount",
        "-a.service",
        "a@*b.service",
    ] {
        assert!(!valid_unit(bad), "{bad}");
    }
}

#[test]
fn more_service_packages_translate_and_migration_scripts_do_not() {
    for pkg in [
        "memcached",
        "nginx",
        "mosquitto",
        "haproxy",
        "dnsmasq",
        "lighttpd",
        "fail2ban-server",
    ] {
        let t = translate(&load(pkg), &nothing_installed());
        assert_eq!(t.leftovers, [], "{pkg}");
    }
    let t = translate(&load("valkey-compat-redis"), &nothing_installed());
    assert_eq!(sources(&t), ["%post"]);
    assert_eq!(
        t.leftovers[0].lines,
        ["/usr/libexec/migrate_redis_to_valkey.sh"]
    );
}
