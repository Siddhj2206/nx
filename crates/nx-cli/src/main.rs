//! The `nx` command.
//!
//! clap parses; nx draws. Help, errors and results all go through [`ui`] so
//! every line looks like nx 0.x's: two-space indent, one symbol per kind,
//! results on stdout, everything else on stderr.

mod add;
mod apply;
mod generations;
mod help;
mod host;
mod info;
mod list;
mod live;
mod remove;
mod repo;
mod rollback;
mod search;
mod shell;
mod status;
mod ui;
mod why;

use std::path::Path;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};
use nx_core::RpmOstree;

use crate::ui::Ui;

/// Exit codes, as in nx 0.x. Interrupted (130) is the shell's own for SIGINT.
const EXIT_FAILED: u8 = 1;
const EXIT_MISUSE: u8 = 2;

/// Packages for a bootc system, live, without rebuilding the image.
#[derive(Parser)]
#[command(name = "nx", version, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// One variant per command. The doc comment is the line in `nx help`; the
/// group is in `help::PLACEMENT`.
#[derive(Subcommand)]
enum Command {
    /// add packages to the host tier, live
    Add {
        /// names, provides (pkgconfig(xkbcommon)) or files (/usr/bin/rg);
        /// name@repo takes it from that repo only
        #[arg(value_name = "package", required = true)]
        packages: Vec<String>,
        /// resolve and list what would be added; change nothing
        #[arg(long)]
        dry_run: bool,
        /// install despite scriptlets nx can't translate, after you reviewed
        /// them: package=digest, as the refusal prints it
        #[arg(long, value_name = "package=digest")]
        allow_scriptlets: Vec<String>,
        #[command(flatten)]
        metadata: Metadata,
    },
    /// remove packages and what only they needed, live
    Remove {
        /// what `nx list` shows, as it was added
        #[arg(value_name = "package", required = true)]
        packages: Vec<String>,
        /// list what would be removed; change nothing
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        metadata: Metadata,
    },
    /// installed packages
    #[command(visible_alias = "ls")]
    List,
    /// every generation, what each changed, and the current one
    Generations,
    /// make an earlier generation current again
    Rollback {
        /// the generation to go to (default: the one before the current)
        #[arg(value_name = "N")]
        generation: Option<u32>,
        /// say which generation it would activate; change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// a shell (or command) with the host tier on its paths
    Shell {
        #[arg(value_name = "command", trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// who provides a package, command or file, and why
    Why {
        #[arg(value_name = "name|path")]
        what: String,
    },
    /// third-party RPM repos: add, list, remove
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },
    /// find packages by name or summary
    Search {
        #[arg(value_name = "term", required = true)]
        terms: Vec<String>,
        #[command(flatten)]
        metadata: Metadata,
    },
    /// show one package: version, repo, source, size
    Info {
        #[arg(value_name = "package")]
        package: String,
        #[command(flatten)]
        metadata: Metadata,
    },
    /// the image nx resolves against: booted, staged and rollback
    Status,
}

#[derive(Subcommand)]
enum RepoAction {
    /// add copr:owner/project, a .repo file's URL, or a repo URL with --key
    Add {
        #[arg(value_name = "repo")]
        source: String,
        /// the repo's signing key (https), when the source doesn't name one
        #[arg(long, value_name = "url")]
        key: Option<String>,
        /// dnf priority, lower wins; Fedora's repos are 99, added ones 100
        #[arg(long)]
        priority: Option<i32>,
        /// replace the pinned key of a repo added before, once checked
        #[arg(long)]
        new_key: bool,
    },
    /// the image's repos and the ones nx added
    List,
    /// remove a repo nx added
    Remove {
        #[arg(value_name = "repo")]
        name: String,
    },
}

/// How the host-tier commands treat cached repo metadata.
#[derive(Args)]
struct Metadata {
    /// download fresh repo metadata first
    #[arg(long)]
    refresh: bool,
}

impl Metadata {
    /// Commands answer from the cache; only a repo with nothing cached, or
    /// `--refresh`, waits on the network.
    fn refresh(&self) -> nx_rpm::Refresh {
        if self.refresh {
            nx_rpm::Refresh::Always
        } else {
            nx_rpm::Refresh::IfMissing
        }
    }

    /// The universe for the repos of the generation nx can read.
    fn load(&self, ui: &mut Ui) -> anyhow::Result<nx_rpm::Universe> {
        let machine = machine()?;
        let manifest = host::readable(ui, &host::state())?
            .map(|g| g.manifest)
            .unwrap_or_default();
        host::load(ui, &machine, self.refresh(), &manifest)
    }
}

fn machine() -> anyhow::Result<host::Machine> {
    host::Machine::read(Path::new("/"), &RpmOstree)
}

fn main() -> ExitCode {
    let mut ui = Ui::new();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return parse_error(&mut ui, &err),
    };
    let Some(command) = cli.command else {
        help::main(&mut ui.out);
        return ExitCode::SUCCESS;
    };
    let result = match command {
        Command::Add {
            packages,
            dry_run,
            allow_scriptlets,
            metadata,
        } => machine().and_then(|m| {
            add::run(
                &mut ui,
                &m,
                &packages,
                dry_run,
                &allow_scriptlets,
                metadata.refresh(),
            )
        }),
        Command::Remove {
            packages,
            dry_run,
            metadata,
        } => {
            machine().and_then(|m| remove::run(&mut ui, &m, &packages, dry_run, metadata.refresh()))
        }
        Command::List => list::run(&mut ui),
        Command::Generations => generations::run(&mut ui),
        Command::Rollback {
            generation,
            dry_run,
        } => machine().and_then(|m| rollback::run(&mut ui, &m, generation, dry_run)),
        Command::Shell { command } => shell::run(&mut ui, &command),
        Command::Why { what } => machine().and_then(|m| why::run(&mut ui, &m, &what)),
        Command::Repo { action } => match action {
            RepoAction::Add {
                source,
                key,
                priority,
                new_key,
            } => machine()
                .and_then(|m| repo::add(&mut ui, &m, &source, key.as_deref(), priority, new_key)),
            RepoAction::List => machine().and_then(|m| repo::list(&mut ui, &m)),
            RepoAction::Remove { name } => machine().and_then(|m| repo::remove(&mut ui, &m, &name)),
        },
        Command::Search { terms, metadata } => metadata
            .load(&mut ui)
            .map(|u| search::run(&mut ui, &u, &terms)),
        Command::Info { package, metadata } => metadata
            .load(&mut ui)
            .and_then(|u| info::run(&mut ui, &u, &package)),
        Command::Status => status::run(
            &mut ui,
            &RpmOstree,
            &host::state(),
            Path::new("/"),
            Path::new("/sysroot"),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            ui.error(format_args!("{err:#}"));
            ExitCode::from(EXIT_FAILED)
        }
    }
}

/// Help and version are clap "errors" too; everything else is misuse,
/// reworded into one nx error line with clap's suggestion kept.
fn parse_error(ui: &mut Ui, err: &clap::Error) -> ExitCode {
    match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            match help::subcommand_from_args(std::env::args().skip(1)) {
                Some(name) => help::command(&mut ui.out, &name),
                None => help::main(&mut ui.out),
            }
            // Asked for help: success. Help because something was missing
            // (`nx repo`): misuse, as clap itself would say.
            if err.kind() == ErrorKind::DisplayHelp {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(EXIT_MISUSE)
            }
        }
        ErrorKind::DisplayVersion => {
            ui.out
                .raw(format_args!("nx {}\n", env!("CARGO_PKG_VERSION")));
            ExitCode::SUCCESS
        }
        _ => {
            let (message, tips) = help::reword(&err.render().to_string());
            let mut text = format!("{message} {}", ui.err.style.dim("(see nx help)"));
            for tip in tips {
                text.push('\n');
                text.push_str(&ui.err.style.dim(&tip));
            }
            ui.error(text);
            ExitCode::from(EXIT_MISUSE)
        }
    }
}
