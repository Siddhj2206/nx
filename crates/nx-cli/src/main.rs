//! The `nx` command.
//!
//! clap parses; nx draws. Help, errors and results all go through [`ui`] so
//! every line looks like nx 0.x's: two-space indent, one symbol per kind,
//! results on stdout, everything else on stderr.

mod add;
mod apply;
mod boot;
mod cache;
mod compat;
mod converge;
mod doctor;
mod gc;
mod generations;
mod help;
mod host;
mod info;
mod list;
mod live;
mod migrate;
mod notify;
mod remove;
mod repo;
mod rollback;
mod route;
mod search;
mod shell;
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
        #[arg(value_name = "package", required = true)]
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
        /// allow these nixpkgs packages despite an unfree license
        #[arg(long)]
        allow_unfree: bool,
        /// let this package have a command another one has
        #[arg(long = "override", value_name = "command")]
        overrides: Vec<String>,
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
    /// extra Nix binary caches for the tool tier: add, list, remove
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Cache {
        #[command(subcommand)]
        action: CacheAction,
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
    /// move nx 0.x's packages (the Nix profile) into nx
    Migrate {
        /// list what would move; change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// give every deployed image a host tier that matches the manifest
    Sync {
        #[command(flatten)]
        metadata: Metadata,
    },
    /// remove generations and packages nothing needs
    Gc {
        /// list what would be removed; change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// at boot: make the generation built for the booted image current
    #[command(hide = true)]
    Select {
        /// after the boot-time merge: use the prefix backend if it didn't happen
        #[arg(long)]
        after_merge: bool,
    },
    /// in the session: notify about what nx's background jobs couldn't do
    #[command(hide = true)]
    Notify,
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

#[derive(Subcommand)]
enum CacheAction {
    /// trust a binary cache and the key it signs with
    Add {
        #[arg(value_name = "url")]
        url: String,
        /// as the cache publishes it: name-1:base64…
        #[arg(value_name = "key")]
        key: String,
    },
    /// cache.nixos.org and the caches nx added
    List,
    /// stop trusting a cache nx added
    Remove {
        #[arg(value_name = "url")]
        url: String,
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

    /// The universe for the repos of the generation nx can read, and that
    /// generation.
    fn load(&self, ui: &mut Ui) -> anyhow::Result<(nx_rpm::Universe, Option<nx_core::Generation>)> {
        let machine = machine()?;
        let current = host::readable(ui, &host::state())?;
        let manifest = current
            .as_ref()
            .map(|g| g.manifest.clone())
            .unwrap_or_default();
        let universe = host::load(ui, &machine, self.refresh(), &manifest)?;
        Ok((universe, current))
    }
}

/// nx 0.x's profile and registry, under `$HOME`.
fn legacy() -> Option<nx_nix::profile::Legacy> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    let xdg = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    };
    Some(nx_nix::profile::Legacy::new(
        Path::new(&home),
        xdg("XDG_STATE_HOME").as_deref(),
        xdg("XDG_CONFIG_HOME").as_deref(),
    ))
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
            host,
            tool,
            allow_unfree,
            overrides,
            allow_scriptlets,
            metadata,
        } => {
            let opts = add::Options {
                dry_run,
                allow_scriptlets,
                refresh: Some(metadata.refresh()),
                forced: match (host, tool) {
                    (true, _) => Some(route::Forced::Host),
                    (_, true) => Some(route::Forced::Tool),
                    _ => None,
                },
                allow_unfree,
                overrides,
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
            let result = machine()
                .and_then(|m| upgrade::run(&mut ui, &m, (host, tool), dry_run, metadata.refresh()));
            if timer {
                let failure = result.as_ref().err().map(|e| format!("{e:#}"));
                nx_core::select::record_upgrade_failure(&host::state(), failure.as_deref());
            }
            result
        }
        Command::Cache { action } => match action {
            CacheAction::Add { url, key } => {
                machine().and_then(|m| cache::add(&mut ui, &m, &url, &key))
            }
            CacheAction::List => cache::list(&mut ui),
            CacheAction::Remove { url } => machine().and_then(|m| cache::remove(&mut ui, &m, &url)),
        },
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
        Command::Apply {
            file,
            dry_run,
            metadata,
        } => machine()
            .and_then(|m| converge::run(&mut ui, &m, file.as_deref(), dry_run, metadata.refresh())),
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
        Command::Search { terms, metadata } => metadata.load(&mut ui).map(|(u, g)| {
            let pinned = g.and_then(|g| g.lock.tool);
            let catalog = tool::catalog_to_read(&mut ui, pinned.as_ref());
            search::run(&mut ui, &u, catalog.as_ref(), &terms);
        }),
        Command::Info { package, metadata } => metadata.load(&mut ui).and_then(|(u, g)| {
            let pinned = g.as_ref().and_then(|g| g.lock.tool.clone());
            let manifest = g.map(|g| g.manifest).unwrap_or_default();
            let catalog = tool::catalog_to_read(&mut ui, pinned.as_ref());
            info::run(&mut ui, &u, catalog.as_ref(), &manifest, &package)
        }),
        Command::Sync { metadata } => {
            machine().and_then(|m| sync::run(&mut ui, &m, metadata.refresh()))
        }
        Command::Gc { dry_run } => machine().and_then(|m| gc::run(&mut ui, &m, dry_run)),
        Command::Select { after_merge: false } => boot::run(&mut ui),
        Command::Select { after_merge: true } => boot::settle(&mut ui),
        Command::Notify => notify::seen_file().and_then(|seen| {
            notify::run(&mut ui, &host::state(), &RpmOstree, &notify::Busctl, &seen)
        }),
        Command::Doctor => {
            let machine = machine().ok();
            let deployments = nx_core::DeploymentSource::deployments(&RpmOstree).ok();
            let env = |k: &str| std::env::var(k).ok();
            let legacy = legacy();
            let state = host::state();
            doctor::run(
                &mut ui,
                &doctor::Inputs {
                    state: &state,
                    root: Path::new("/"),
                    image_root: machine.and_then(|m| m.image_root().ok()),
                    deployments,
                    env: &env,
                    legacy,
                    label: &doctor::label,
                },
            )
        }
        Command::Migrate { dry_run } => match legacy() {
            Some(l) => {
                machine().and_then(|m| migrate::run(&mut ui, &m, &l, &nx_nix::System, dry_run))
            }
            None => Err(anyhow::anyhow!(
                "HOME is not set, so there is no Nix profile to read"
            )),
        },
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
