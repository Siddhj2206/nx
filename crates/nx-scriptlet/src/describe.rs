//! What a script nx refused does, in words, so a refusal can say why it
//! matters ("makes ld.mold the system linker") instead of quoting shell.
//! Only a reading for people: the translator never acts on it, and a
//! command it doesn't know is "runs <command>".

use crate::Leftover;

/// What `leftover` does, one phrase per distinct action, in script order.
pub fn describe(leftover: &Leftover) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(interp) = interpreter(&leftover.source) {
        out.push(format!("runs a {interp} script"));
        return out;
    }
    for line in &leftover.lines {
        for command in commands(line) {
            if let Some(what) = action(&command)
                && !out.contains(&what)
            {
                out.push(what);
            }
        }
    }
    out
}

/// `%post (-p /usr/bin/lua)` → `Lua`.
fn interpreter(source: &str) -> Option<String> {
    let path = source.split_once("(-p ")?.1.trim_end_matches(')');
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    Some(match name {
        "lua" => "Lua".to_owned(),
        "python" => "Python".to_owned(),
        "perl" => "Perl".to_owned(),
        other => other.to_owned(),
    })
}

/// The simple commands of a line, as words: split at `&&`, `||`, `;` and
/// `|`, quotes dropped, redirections and assignments left out.
fn commands(line: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for part in line.split(['&', '|', ';']) {
        let words: Vec<String> = part
            .split_whitespace()
            .map(|w| w.trim_matches(['"', '\'', '(', ')', '`']).to_owned())
            .filter(|w| !w.is_empty() && !w.contains('>') && !w.contains('<'))
            .skip_while(|w| {
                w.contains('=')
                    || matches!(
                        w.as_str(),
                        "if" | "then" | "else" | "elif" | "fi" | "do" | "done" | "!" | "exec"
                    )
            })
            .collect();
        if !words.is_empty() {
            out.push(words);
        }
    }
    out
}

/// One command's action, or `None` for shell plumbing (tests, `:`, `exit`).
fn action(words: &[String]) -> Option<String> {
    let name = words[0].rsplit('/').next().unwrap_or(&words[0]);
    let args = &words[1..];
    let into = |verb: &str| -> String {
        // The last argument that is a path names the destination.
        match args.iter().rev().find(|a| a.starts_with('/')) {
            Some(dest) => format!("{verb} files into {}", parent(dest)),
            None => format!("{verb} files"),
        }
    };
    Some(match name {
        "[" | "[[" | "test" | ":" | "true" | "false" | "exit" | "echo" | "printf" | "return"
        | "selinuxenabled" | "command" | "type" | "which" => return None,
        "update-alternatives" | "alternatives" => alternatives(args),
        "fixfiles" | "restorecon" | "chcon" | "setfiles" => "relabels files for SELinux".into(),
        "semodule" if args.iter().any(|a| a == "-r" || a == "--remove") => {
            "removes an SELinux policy module".into()
        }
        "semodule" => "installs an SELinux policy module".into(),
        "semanage" | "setsebool" => "changes the SELinux policy".into(),
        "depmod" | "weak-modules" => "rebuilds the kernel module index".into(),
        "dracut" => "rebuilds the initramfs".into(),
        "grub2-mkconfig" | "grubby" | "kernel-install" | "bootctl" => {
            "changes the boot configuration".into()
        }
        "useradd" | "groupadd" | "usermod" | "groupmod" => {
            "creates or changes a user or group".into()
        }
        "chown" | "chgrp" | "chmod" | "setfacl" | "setcap" => {
            "changes the owner or mode of files".into()
        }
        "cp" | "install" => into("copies"),
        "mv" => into("moves"),
        "ln" => into("links"),
        "rm" | "rmdir" => "removes files".into(),
        "mkdir" | "touch" => "creates files or directories".into(),
        "sed" | "tee" | "cat" if args.iter().any(|a| a.starts_with("/etc")) => {
            "edits files in /etc".into()
        }
        "firewall-cmd" | "firewall-offline-cmd" => "changes the firewall".into(),
        "update-ca-trust" | "certutil" => "changes the trusted certificates".into(),
        "update-mime-database"
        | "update-desktop-database"
        | "gtk-update-icon-cache"
        | "glib-compile-schemas"
        | "fc-cache"
        | "mandb" => "updates a system cache".into(),
        "systemctl" => format!("runs systemctl {}", args.first().map_or("", String::as_str))
            .trim_end()
            .to_owned(),
        _ => format!("runs {name}"),
    })
}

/// `update-alternatives --install /usr/bin/ld ld /usr/bin/ld.mold 1` →
/// "makes ld.mold the system linker".
fn alternatives(args: &[String]) -> String {
    let at = |flag: &str| args.iter().position(|a| a == flag);
    if let Some(i) = at("--install")
        && let (Some(name), Some(path)) = (args.get(i + 2), args.get(i + 3))
    {
        let program = path.rsplit('/').next().unwrap_or(path);
        return format!("makes {program} the system {}", role(name));
    }
    if let Some(i) = at("--remove").or_else(|| at("--set"))
        && let Some(name) = args.get(i + 1)
    {
        return format!("changes which program is the system {}", role(name));
    }
    "changes the system's alternatives".into()
}

/// What an alternatives group is, for the names that read badly alone.
fn role(name: &str) -> String {
    match name {
        "ld" => "linker".into(),
        "cc" => "C compiler".into(),
        "c++" => "C++ compiler".into(),
        "java" | "javac" => format!("{name} (Java)"),
        _ => name.to_owned(),
    }
}

/// `/etc/grub.d/00_tuned` → `/etc/grub.d`; a directory stays as it is.
fn parent(path: &str) -> &str {
    if path.ends_with('/') {
        return path.trim_end_matches('/');
    }
    match path.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((dir, _)) => dir,
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leftover(source: &str, lines: &[&str]) -> Leftover {
        Leftover {
            source: source.into(),
            lines: lines.iter().map(|l| (*l).to_owned()).collect(),
        }
    }

    #[test]
    fn says_what_alternatives_change() {
        let l = leftover(
            "%post",
            &["/usr/sbin/update-alternatives --install /usr/bin/ld ld /usr/bin/ld.mold 1"],
        );
        assert_eq!(describe(&l), ["makes ld.mold the system linker"]);
        let l = leftover("%postun", &["alternatives --remove ld /usr/bin/ld.mold"]);
        assert_eq!(describe(&l), ["changes which program is the system linker"]);
    }

    #[test]
    fn reads_tuned_posttrans() -> std::io::Result<()> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/scriptlets/tuned/posttrans");
        let body = std::fs::read_to_string(dir)?;
        let l = Leftover {
            source: "%posttrans".into(),
            lines: crate::shell::lines(&body),
        };
        assert_eq!(
            describe(&l),
            [
                "copies files into /etc/grub.d",
                "relabels files for SELinux"
            ]
        );
        Ok(())
    }

    #[test]
    fn names_other_interpreters_and_unknown_commands() {
        let l = leftover("%post (-p /usr/bin/lua)", &["posix.mkdir('/x')"]);
        assert_eq!(describe(&l), ["runs a Lua script"]);
        let l = leftover("%post", &["/usr/libexec/frobnicate --all || :"]);
        assert_eq!(describe(&l), ["runs frobnicate"]);
    }
}
