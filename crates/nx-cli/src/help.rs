//! `nx help`, drawn like nx 0.x's: commands in groups, names in cyan, one
//! line each. The commands, their arguments and their descriptions come from
//! clap's definitions, so there is one place to change them.

use clap::CommandFactory;

use crate::Cli;
use crate::ui::{Problem, Stream, pad};

const TAGLINE: &str = "· Fedora RPMs on the image's /usr, sealed nixpkgs tools beside it";
const GROUPS: [&str; 3] = ["Packages", "Maintenance", "System"];

/// Which group each command is listed under, in `nx help`'s order within a
/// group. The test below fails for a command left out.
const PLACEMENT: &[(&str, &str)] = &[
    ("add", "Packages"),
    ("remove", "Packages"),
    ("upgrade", "Packages"),
    ("list", "Packages"),
    ("search", "Packages"),
    ("info", "Packages"),
    ("why", "Packages"),
    ("generations", "Maintenance"),
    ("rollback", "Maintenance"),
    ("apply", "Maintenance"),
    ("repo", "Maintenance"),
    ("sync", "Maintenance"),
    ("gc", "Maintenance"),
    ("status", "System"),
    ("doctor", "System"),
];

pub fn group(name: &str) -> &'static str {
    PLACEMENT
        .iter()
        .find(|(command, _)| *command == name)
        .map_or("", |(_, group)| group)
}

pub fn main(s: &mut Stream) {
    let cli = Cli::command();
    let st = s.style;
    s.raw(format_args!(
        "\n  {} {}\n\n",
        st.bold("nx"),
        st.dim(TAGLINE)
    ));
    s.raw(format_args!(
        "  {} nx <command> [args]\n",
        st.bold("Usage:")
    ));
    // One column for every description: the longest usage, plus two.
    let width = cli
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| c.get_name().len() + usage_args(c).len() + 2)
        .max()
        .unwrap_or(0)
        .max(22);
    for heading in GROUPS {
        let commands: Vec<_> = cli
            .get_subcommands()
            .filter(|c| !c.is_hide_set() && group(c.get_name()) == heading)
            .collect();
        if commands.is_empty() {
            continue;
        }
        s.raw(format_args!("\n  {}\n", st.dim(heading)));
        for c in commands {
            let name = c.get_name();
            let args = usage_args(c);
            let left = format!("{}{}", st.cyan(name), args);
            let about = c.get_about().map(ToString::to_string).unwrap_or_default();
            s.raw(format_args!("    {}{about}\n", pad(&left, width)));
        }
    }
    s.raw(format_args!(
        "\n  {}\n\n",
        st.dim("nx <command> --help for one command; -v shows every step; nx --version for the version.")
    ));
}

/// Help for one command: usage, what it does, its arguments.
pub fn command(s: &mut Stream, name: &str) {
    let cli = Cli::command();
    let Some(c) = cli.find_subcommand(name) else {
        main(s);
        return;
    };
    let st = s.style;
    let about = c.get_about().map(ToString::to_string).unwrap_or_default();
    s.raw(format_args!(
        "\n  {} {}\n\n",
        st.bold(&format!("nx {name}")),
        st.dim(&format!("· {about}"))
    ));
    s.raw(format_args!(
        "  {} nx {}{}\n",
        st.bold("Usage:"),
        name,
        if c.has_subcommands() {
            " <command> [args]".to_owned()
        } else {
            usage_args(c)
        }
    ));
    let args: Vec<_> = c
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !matches!(a.get_id().as_str(), "help" | "version"))
        .collect();
    let subcommands: Vec<_> = c.get_subcommands().filter(|c| !c.is_hide_set()).collect();
    if !subcommands.is_empty() {
        s.raw(format_args!("\n  {}\n", st.dim("Commands")));
        for sub in subcommands {
            let left = format!("{}{}", st.cyan(sub.get_name()), usage_args(sub));
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            s.raw(format_args!("    {}{about}\n", pad(&left, 22)));
        }
    }
    if !args.is_empty() {
        s.raw(format_args!("\n  {}\n", st.dim("Options")));
        for a in args {
            let left = match (a.get_short(), a.get_long()) {
                (Some(short), Some(long)) => format!("-{short}, --{long}"),
                (None, Some(long)) => format!("--{long}"),
                (Some(short), None) => format!("-{short}"),
                (None, None) => format!("<{}>", a.get_id()),
            };
            let help = a.get_help().map(ToString::to_string).unwrap_or_default();
            s.raw(format_args!("    {}{help}\n", pad(&st.cyan(&left), 22)));
        }
    }
    s.blank();
}

/// ` <name>...` for positionals, ` [options]` when there are flags.
fn usage_args(c: &clap::Command) -> String {
    let mut out = String::new();
    let mut has_flags = false;
    for a in c.get_arguments() {
        if a.is_hide_set() || matches!(a.get_id().as_str(), "help" | "version") {
            continue;
        }
        if a.is_positional() {
            let name = a
                .get_value_names()
                .and_then(|v| v.first())
                .map_or_else(|| a.get_id().to_string(), ToString::to_string);
            let many = a.get_num_args().is_some_and(|n| n.max_values() > 1);
            let dots = if many { "..." } else { "" };
            if a.is_required_set() {
                out.push_str(&format!(" <{name}>{dots}"));
            } else {
                out.push_str(&format!(" [{name}]{dots}"));
            }
        } else {
            has_flags = true;
        }
    }
    if has_flags {
        out.push_str(" [options]");
    }
    out
}

/// The command `--help` was asked about, if any: the first argument that
/// names a command (`nx status --help`, `nx help status`).
pub fn subcommand_from_args(args: impl Iterator<Item = String>) -> Option<String> {
    let cli = Cli::command();
    args.into_iter().find(|a| cli.find_subcommand(a).is_some())
}

/// clap's rendered error as nx's problem: drops the `error: ` prefix, the
/// usage block and clap's own "for more information" line, and turns
/// "a similar subcommand exists: 'add'" into "did you mean nx add?".
pub fn reword(rendered: &str) -> Problem {
    let mut lines = rendered.lines().map(str::trim).filter(|l| !l.is_empty());
    let message = lines
        .next()
        .map(|l| l.strip_prefix("error: ").unwrap_or(l))
        .unwrap_or("invalid arguments")
        .replace('\'', "")
        .replace("unrecognized subcommand", "unknown command:");
    let mut problem = Problem::new(message);
    let mut suggested = false;
    for tip in lines
        .filter_map(|l| l.strip_prefix("tip: "))
        .filter(|l| !l.starts_with("to pass"))
        .map(|l| l.replace('\'', ""))
    {
        if let Some(name) = tip.strip_prefix("a similar subcommand exists: ") {
            problem = problem.reason(format!("did you mean nx {name}?"));
            suggested = true;
        } else if let Some(arg) = tip.strip_prefix("a similar argument exists: ") {
            problem = problem.reason(format!("did you mean {arg}?"));
            suggested = true;
        } else {
            problem = problem.reason(tip);
        }
    }
    if !suggested {
        problem = problem.fix("Every command", "nx help");
    }
    problem
}

/// clap's "the following required arguments were not provided" as nx
/// says it: `nx add needs a package: nx add <package>...`. `None` for any
/// other error.
pub fn missing(err: &clap::Error) -> Option<String> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    if err.kind() != ErrorKind::MissingRequiredArgument {
        return None;
    }
    let Some(ContextValue::Strings(args)) = err.get(ContextKind::InvalidArg) else {
        return None;
    };
    let usage = match err.get(ContextKind::Usage) {
        Some(ContextValue::StyledStr(u)) => u.to_string(),
        _ => return None,
    };
    Some(missing_message(&usage, args))
}

/// [`missing`] from clap's usage line and the missing arguments as clap
/// names them (`<PACKAGE>...`).
fn missing_message(usage: &str, args: &[String]) -> String {
    let path: Vec<&str> = usage
        .trim()
        .trim_start_matches("Usage:")
        .split_whitespace()
        .take_while(|w| !w.starts_with(['[', '<', '-']))
        .collect();
    let path = path.join(" ");
    let names: Vec<String> = args
        .iter()
        .map(|a| {
            let name = a
                .trim_end_matches("...")
                .trim_matches(['<', '>'])
                .to_lowercase()
                .replace(['_', '-'], " ")
                .replace('|', " or ");
            let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
                "an"
            } else {
                "a"
            };
            format!("{article} {name}")
        })
        .collect();
    let example: Vec<String> = args.iter().map(|a| a.to_lowercase()).collect();
    format!(
        "{path} needs {}: {path} {}",
        names.join(" and "),
        example.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{SharedBuf, Ui};

    #[test]
    fn every_command_is_in_a_listed_group() {
        for c in Cli::command()
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
        {
            assert!(GROUPS.contains(&group(c.get_name())), "{}", c.get_name());
        }
    }

    #[test]
    fn main_help_lists_commands_in_groups() {
        let out = SharedBuf::default();
        let mut ui = Ui::plain(out.clone(), SharedBuf::default());
        main(&mut ui.out);
        let text = out.text();
        assert!(text.contains("  Usage: nx <command> [args]\n"));
        assert!(text.contains("\n  System\n    status"));
        assert!(text.contains("the image nx resolves against"));
    }

    fn missing_for(args: &[&str]) -> Option<String> {
        Cli::command()
            .try_get_matches_from(args)
            .err()
            .and_then(|e| missing(&e))
    }

    #[test]
    fn says_what_a_command_needs() {
        assert_eq!(
            missing_for(&["nx", "add"]).as_deref(),
            Some("nx add needs a package: nx add <package>...")
        );
        assert_eq!(
            missing_for(&["nx", "why"]).as_deref(),
            Some("nx why needs a name or path: nx why <name|path>")
        );
        assert_eq!(missing_for(&["nx", "statsu"]), None);
        assert_eq!(
            missing_message(
                "Usage: nx repo add [OPTIONS] <REPO> <KEY_FILE>",
                &["<REPO>".into(), "<KEY_FILE>".into()]
            ),
            "nx repo add needs a repo and a key file: nx repo add <repo> <key_file>"
        );
    }

    #[test]
    fn rewords_unknown_command_with_suggestion() {
        let err = Cli::command()
            .try_get_matches_from(["nx", "statsu"])
            .err()
            .map(|e| e.render().to_string())
            .unwrap_or_default();
        assert_eq!(
            reword(&err),
            Problem::new("unknown command: statsu").reason("did you mean nx status?")
        );
        let err = Cli::command()
            .try_get_matches_from(["nx", "frobnicate"])
            .err()
            .map(|e| e.render().to_string())
            .unwrap_or_default();
        assert_eq!(
            reword(&err),
            Problem::new("unknown command: frobnicate").fix("Every command", "nx help")
        );
    }

    #[test]
    fn finds_the_command_help_was_asked_about() {
        let args = ["help", "status"].map(String::from);
        assert_eq!(
            subcommand_from_args(args.into_iter()),
            Some("status".into())
        );
        assert_eq!(subcommand_from_args(std::iter::empty()), None);
    }
}
