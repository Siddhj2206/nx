//! The `nx` command.
//!
//! clap parses; nx draws. Help, errors and results all go through [`ui`] so
//! every line looks like nx 0.x's: two-space indent, one symbol per kind,
//! results on stdout, everything else on stderr.

mod add;
mod help;
mod host;
mod info;
mod search;
mod status;
mod ui;

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
    /// add packages (for now, --dry-run only: what would be installed)
    Add {
        /// names, provides (pkgconfig(xkbcommon)) or files (/usr/bin/rg)
        #[arg(value_name = "package", required = true)]
        packages: Vec<String>,
        /// resolve and list what would be added; change nothing
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        metadata: Metadata,
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
    fn load(&self, ui: &mut Ui) -> anyhow::Result<nx_rpm::Universe> {
        let refresh = if self.refresh {
            nx_rpm::Refresh::Always
        } else {
            nx_rpm::Refresh::IfMissing
        };
        host::load(ui, refresh)
    }
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
            metadata,
        } => add::check(dry_run)
            .and_then(|()| metadata.load(&mut ui))
            .and_then(|mut u| add::run(&mut ui, &mut u, &packages)),
        Command::Search { terms, metadata } => metadata
            .load(&mut ui)
            .map(|u| search::run(&mut ui, &u, &terms)),
        Command::Info { package, metadata } => metadata
            .load(&mut ui)
            .and_then(|u| info::run(&mut ui, &u, &package)),
        Command::Status => status::run(&mut ui, &RpmOstree, Path::new("/"), Path::new("/sysroot")),
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
            ExitCode::SUCCESS
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
