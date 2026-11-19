//! The `nx` command.
//!
//! clap parses; nx draws. Help, errors and results all go through [`ui`] so
//! every line looks like nx 0.x's: two-space indent, one symbol per kind,
//! results on stdout, everything else on stderr.

mod help;
mod status;
mod ui;

use std::path::Path;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};
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
/// group comes from [`help::group`].
#[derive(Subcommand)]
enum Command {
    /// the image nx resolves against: booted, staged and rollback
    Status,
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
