//! The `nx` command.
//!
//! clap parses; nx draws. Help, errors and results all go through [`ui`] so
//! every line looks like nx 0.x's: two-space indent, one symbol per kind,
//! results on stdout, everything else on stderr.

mod add;
mod apply;
mod boot;
mod changes;
mod compat;
mod complete;
mod doctor;
mod gc;
mod generations;
mod help;
mod host;
mod info;
mod keys;
mod list;
mod live;
mod notify;
mod problems;
mod remove;
mod repo;
mod rollback;
mod route;
mod search;
mod session;
mod status;
mod sync;
mod tool;
mod ui;
mod upgrade;
mod why;

use std::path::Path;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};
use clap_complete::engine::{ArgValueCandidates, ArgValueCompleter};
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
    /// add packages, live: tools from nixpkgs, the rest as Fedora RPMs
    Add {
        /// names, commands (rg), provides (pkgconfig(xkbcommon)) or files
        /// (/usr/bin/rg); name@repo takes it from that repo only
        #[arg(value_name = "package", required = true,
              add = ArgValueCompleter::new(complete::packages))]
        packages: Vec<String>,
        /// resolve and list what would be added; change nothing
        #[arg(long)]
        dry_run: bool,
        /// a Fedora RPM in the host tier, wherever nx would put it
        #[arg(long, conflicts_with = "tool")]
        host: bool,
        /// a nixpkgs package in the tool tier, wherever nx would put it
        #[arg(long)]
        tool: bool,
        /// let this package have a command another tool package has
        #[arg(long = "override", value_name = "command")]
        overrides: Vec<String>,
        /// add it without a command the image or the host tier already has
        #[arg(long, value_name = "command")]
        without: Vec<String>,
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
        #[arg(value_name = "package", required = true,
              add = ArgValueCandidates::new(complete::installed))]
        packages: Vec<String>,
        /// list what would be removed; change nothing
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        metadata: Metadata,
    },
    /// move packages to newer builds and nixpkgs to its current release
    Upgrade {
        /// only the host tier's packages
        #[arg(long, conflicts_with = "tool")]
        host: bool,
        /// only the tool tier: move the nixpkgs pin
        #[arg(long)]
        tool: bool,
        /// list what would change; change nothing
        #[arg(long)]
        dry_run: bool,
        /// run by nx-upgrade.timer: record a failure for nx notify
        #[arg(long, hide = true)]
        timer: bool,
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
    /// who provides a package, command or file, and why
    Why {
        #[arg(value_name = "name|path")]
        what: String,
    },
    /// make the system match a manifest: this file, or the current one
    Apply {
        #[arg(value_name = "manifest.toml")]
        file: Option<std::path::PathBuf>,
        /// list what would change; change nothing
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        metadata: Metadata,
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
    /// check that what nx set up is what the system runs
    Doctor,
    /// give every deployed image a host tier that matches the manifest
    Sync {
        #[command(flatten)]
        metadata: Metadata,
        /// build every deployed image's generation again, even one that
        /// matches the manifest
        #[arg(long)]
        rebuild: bool,
    },
    /// remove generations and packages nothing needs
    Gc {
        /// list what would be removed; change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// at boot: make the generation built for the booted image current
    #[command(hide = true)]
    Select,
    /// in the session: notify about what nx's background jobs couldn't do
    #[command(hide = true)]
    Notify,
    /// write nx's man pages into a directory (for packaging)
    #[command(hide = true)]
    Manpages { dir: std::path::PathBuf },
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
        /// show the key and resolve with the repo; change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// the image's repos and the ones nx added
    List,
    /// remove a repo nx added
    Remove {
        #[arg(value_name = "repo", add = ArgValueCandidates::new(complete::repos))]
        name: String,
        /// resolve without the repo; change nothing
        #[arg(long)]
        dry_run: bool,
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
    /// `--refresh` checks every repo; otherwise `mode`. Reads (search,
    /// info, sync) pass `IfMissing`: they answer from the cache and wait on
    /// the network only for a repo with nothing cached. Changes (add,
    /// remove, upgrade, apply) pass `IfStale`: a repo past its
    /// `metadata_expire` costs one `repomd.xml` fetch, and `primary` only
    /// when it changed; offline, the cache answers, with a warning.
    fn refresh(&self, mode: nx_rpm::Refresh) -> nx_rpm::Refresh {
        if self.refresh {
            nx_rpm::Refresh::Always
        } else {
            mode
        }
    }

    /// The universe for the repos of the generation nx can read, and that
    /// generation.
    fn load(&self, ui: &mut Ui) -> anyhow::Result<(nx_rpm::Universe, Option<nx_core::Generation>)> {
        let machine = machine()?;
        let current = host::readable(ui, &host::state())?;
        let manifest = current
            .as_ref()
            .map(|g| g.manifest.clone())
            .unwrap_or_default();
        let universe = host::load(
            ui,
            &machine,
            self.refresh(nx_rpm::Refresh::IfMissing),
            &manifest,
        )?;
        Ok((universe, current))
    }
}

fn machine() -> anyhow::Result<host::Machine> {
    host::Machine::read(Path::new("/"), &RpmOstree)
}

fn main() -> ExitCode {
    complete::answer();
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
            host,
            tool,
            overrides,
            without,
            allow_scriptlets,
            metadata,
        } => {
            let opts = add::Options {
                dry_run,
                allow_scriptlets,
                refresh: Some(metadata.refresh(nx_rpm::Refresh::IfStale)),
                forced: match (host, tool) {
                    (true, _) => Some(route::Forced::Host),
                    (_, true) => Some(route::Forced::Tool),
                    _ => None,
                },
                overrides,
                without,
            };
            machine().and_then(|m| add::run(&mut ui, &m, &packages, &opts))
        }
        Command::Upgrade {
            host,
            tool,
            dry_run,
            timer,
            metadata,
        } => {
            let result = machine().and_then(|m| {
                upgrade::run(
                    &mut ui,
                    &m,
                    (host, tool),
                    dry_run,
                    metadata.refresh(nx_rpm::Refresh::IfStale),
                )
            });
            if timer {
                let failure = result.as_ref().err().map(|e| format!("{e:#}"));
                nx_core::select::record_upgrade_failure(&host::state(), failure.as_deref());
            }
            result
        }
        Command::Remove {
            packages,
            dry_run,
            metadata,
        } => machine().and_then(|m| {
            remove::run(
                &mut ui,
                &m,
                &packages,
                dry_run,
                metadata.refresh(nx_rpm::Refresh::IfStale),
            )
        }),
        Command::List => list::run(&mut ui),
        Command::Generations => generations::run(&mut ui, &RpmOstree, &host::state()),
        Command::Rollback {
            generation,
            dry_run,
        } => machine().and_then(|m| rollback::run(&mut ui, &m, generation, dry_run)),
        Command::Apply {
            file,
            dry_run,
            metadata,
        } => machine().and_then(|m| {
            apply::run(
                &mut ui,
                &m,
                file.as_deref(),
                dry_run,
                metadata.refresh(nx_rpm::Refresh::IfStale),
            )
        }),
        Command::Why { what } => machine().and_then(|m| why::run(&mut ui, &m, &what)),
        Command::Repo { action } => match action {
            RepoAction::Add {
                source,
                key,
                priority,
                new_key,
                dry_run,
            } => machine().and_then(|m| {
                repo::add(
                    &mut ui,
                    &m,
                    &source,
                    key.as_deref(),
                    priority,
                    new_key,
                    dry_run,
                )
            }),
            RepoAction::List => machine().and_then(|m| repo::list(&mut ui, &m)),
            RepoAction::Remove { name, dry_run } => {
                machine().and_then(|m| repo::remove(&mut ui, &m, &name, dry_run))
            }
        },
        Command::Search { terms, metadata } => metadata.load(&mut ui).map(|(u, g)| {
            let pinned = g.and_then(|g| g.lock.tool);
            let catalog = tool::catalog_to_read(
                &mut ui,
                &nx_rpm::Ureq::new(),
                &nx_nix::System,
                pinned.as_ref(),
            );
            search::run(&mut ui, &u, catalog.as_ref(), &terms);
        }),
        Command::Info { package, metadata } => metadata.load(&mut ui).and_then(|(u, g)| {
            let pinned = g.as_ref().and_then(|g| g.lock.tool.clone());
            let manifest = g.map(|g| g.manifest).unwrap_or_default();
            let catalog = tool::catalog_to_read(
                &mut ui,
                &nx_rpm::Ureq::new(),
                &nx_nix::System,
                pinned.as_ref(),
            );
            info::run(&mut ui, &u, catalog.as_ref(), &manifest, &package)
        }),
        Command::Sync { metadata, rebuild } => {
            let rebuild = if rebuild {
                sync::Rebuild::All
            } else {
                sync::Rebuild::Outdated
            };
            machine().and_then(|m| {
                sync::run(
                    &mut ui,
                    &m,
                    &nx_nix::System,
                    metadata.refresh(nx_rpm::Refresh::IfMissing),
                    rebuild,
                )
            })
        }
        Command::Gc { dry_run } => {
            machine().and_then(|m| gc::run(&mut ui, &m, &nx_nix::System, dry_run))
        }
        Command::Select => boot::run(&mut ui),
        Command::Notify => notify::seen_file().and_then(|seen| {
            notify::run(&mut ui, &host::state(), &RpmOstree, &notify::Busctl, &seen)
        }),
        Command::Doctor => {
            let machine = machine().ok();
            let deployments = nx_core::DeploymentSource::deployments(&RpmOstree).ok();
            let env = |k: &str| std::env::var(k).ok();
            let state = host::state();
            doctor::run(
                &mut ui,
                &doctor::Inputs {
                    state: &state,
                    root: Path::new("/"),
                    image_root: machine.and_then(|m| m.image_root().ok()),
                    deployments,
                    env: &env,
                    label: &doctor::label,
                },
            )
        }
        Command::Manpages { dir } => complete::manpages(&dir),
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
        ErrorKind::MissingRequiredArgument if let Some(message) = help::missing(err) => {
            ui.error(message);
            ExitCode::from(EXIT_MISUSE)
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
