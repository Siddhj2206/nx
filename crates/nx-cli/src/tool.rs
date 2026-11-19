//! The tool tier as the commands drive it (design, *Tool tier*): the
//! pinned nixpkgs release and its catalog, store paths by evaluation
//! (ADR-0009), the plan from the binary caches, realising through the
//! daemon, which commands a generation exposes, its shim and GC roots.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nx_core::{Bin, CacheEntry, Generation, LockedTool, Manifest, State, ToolOutput, ToolPackage};
use nx_nix::eval::{self, Evaluated};
use nx_nix::log::Note;
use nx_nix::narinfo::{self, Caches, Sum};
use nx_nix::{Catalog, Nix, Release, Web};

use crate::host;
use crate::ui::{self, Ui};

/// Whether the image has Nix: without `/nix/store` the tool tier is off.
pub fn available() -> bool {
    Path::new(nx_nix::STORE).is_dir()
}

/// Nix's name for the platform: `x86_64-linux`.
pub fn system(arch: &str) -> String {
    format!("{arch}-linux")
}

/// The release a lock is pinned to.
pub fn pinned(tool: &LockedTool) -> Release {
    Release {
        rev: tool.rev.clone(),
        name: tool.release.clone(),
        url: tool.url.clone(),
    }
}

/// The release, the catalog (loaded when first needed) and the means to
/// evaluate and realise.
pub struct Tier {
    web: Web,
    cache: PathBuf,
    pub release: Release,
    catalog: Option<Catalog>,
}

impl Tier {
    /// The tool tier at `pinned`'s release, or at the channel's current one
    /// when nothing is pinned yet or `latest` asks to move the pin.
    pub fn open(ui: &mut Ui, pinned_to: Option<&LockedTool>, latest: bool) -> Result<Self> {
        let web = Web::new();
        let release = match pinned_to {
            Some(t) if !latest && !t.rev.is_empty() => pinned(t),
            _ => {
                ui.step("asking the nixpkgs channel for its current release");
                nx_nix::channel::latest(&web).context("finding the current nixpkgs release")?
            }
        };
        Ok(Self {
            web,
            cache: host::cache_dir()?.join("nix"),
            release,
            catalog: None,
        })
    }

    /// The release's package list, built once per release.
    pub fn catalog(&mut self, ui: &mut Ui) -> Result<&Catalog> {
        if self.catalog.is_none() {
            let version = self.release.version().to_owned();
            let err = &mut ui.err;
            let catalog = Catalog::ensure(&self.web, &self.cache, &self.release, &mut || {
                let sym = err.style.cyan(ui::SYM_FETCH);
                err.line(format_args!("{sym} nixpkgs {version} package list"));
            })
            .context("loading the nixpkgs package list")?;
            self.catalog = Some(catalog);
        }
        self.catalog
            .as_ref()
            .context("the nixpkgs package list didn't load")
    }

    /// The tool tier `manifest` asks for at this release. A package
    /// `previous` locked at the same release is kept as it is; the others
    /// are evaluated, all in one `nix eval`.
    pub fn resolve(
        &self,
        ui: &mut Ui,
        nix: &dyn Nix,
        manifest: &Manifest,
        previous: Option<&LockedTool>,
        system: &str,
    ) -> Result<LockedTool> {
        self.resolve_holding(ui, nix, manifest, previous, None, system)
            .map(|(locked, _)| locked)
    }

    /// [`Tier::resolve`] for an upgrade: a package this release no longer
    /// has (or refuses) keeps its build from `held` instead of failing the
    /// whole upgrade. Returns the attributes kept that way.
    pub fn resolve_holding(
        &self,
        ui: &mut Ui,
        nix: &dyn Nix,
        manifest: &Manifest,
        previous: Option<&LockedTool>,
        held: Option<&LockedTool>,
        system: &str,
    ) -> Result<(LockedTool, Vec<String>)> {
        let same_pin = previous.filter(|p| p.rev == self.release.rev);
        let kept = |attr: &str| same_pin.and_then(|p| p.package(attr));
        let new: Vec<String> = manifest
            .tool
            .packages
            .iter()
            .filter(|a| kept(a).is_none())
            .cloned()
            .collect();
        let evaluated = self.evaluate(ui, nix, &new, &manifest.tool.unfree, system)?;
        let (packages, holding) = lock(
            manifest,
            &evaluated,
            &|attr| kept(attr),
            held,
            self.release.version(),
        )?;
        let locked = LockedTool {
            rev: self.release.rev.clone(),
            release: self.release.name.clone(),
            url: self.release.url.clone(),
            packages,
        };
        Ok((locked, holding))
    }

    fn evaluate(
        &self,
        ui: &mut Ui,
        nix: &dyn Nix,
        attrs: &[String],
        unfree: &[String],
        system: &str,
    ) -> Result<BTreeMap<String, Evaluated>> {
        if attrs.is_empty() {
            return Ok(BTreeMap::new());
        }
        ui.step(format_args!(
            "evaluating {} at nixpkgs {}",
            attrs.join(", "),
            self.release.version()
        ));
        eval::evaluate(nix, &self.release.nixexprs(), system, attrs, unfree)
            .context("evaluating nixpkgs")
    }

    /// What realising `tool` takes: the closure of what isn't in the store
    /// yet, from the binary caches.
    pub fn plan(&self, tool: &LockedTool, caches: &[CacheEntry]) -> Result<Plan> {
        let roots: Vec<String> = tool
            .packages
            .iter()
            .flat_map(ToolPackage::paths)
            .filter(|p| !local(p))
            .map(str::to_owned)
            .collect();
        if roots.is_empty() {
            return Ok(Plan::default());
        }
        let lookup = Caches {
            web: &self.web,
            urls: substituters(caches),
        };
        let closure = narinfo::closure(&lookup, &roots, &|p| local(p))
            .context("reading the binary caches")?;
        Ok(plan_from(tool, &closure))
    }

    /// Realises what `plan` lists through the daemon, drawing each fetch
    /// and build. A derivation the store no longer has (collected since it
    /// was evaluated) is evaluated again first.
    pub fn realise(
        &self,
        ui: &mut Ui,
        nix: &dyn Nix,
        tool: &LockedTool,
        plan: &Plan,
        manifest: &Manifest,
        system: &str,
    ) -> Result<()> {
        if plan.is_empty() {
            return Ok(());
        }
        let lost: Vec<String> = tool
            .packages
            .iter()
            .filter(|p| plan.build.contains(&p.attr) && !Path::new(&p.drv).exists())
            .map(|p| p.attr.clone())
            .collect();
        self.evaluate(ui, nix, &lost, &manifest.tool.unfree, system)?;
        if plan.sum.paths > plan.sum.unbuilt {
            ui.step(format_args!(
                "fetching {} from the Nix caches · {} ({} unpacked)",
                ui::plural(plan.sum.paths - plan.sum.unbuilt, "store path"),
                ui::size(plan.sum.download),
                ui::size(plan.sum.unpacked)
            ));
        }
        if !plan.build.is_empty() {
            ui.step(format_args!(
                "building {}: no binary cache has it",
                plan.build.join(", ")
            ));
        }
        let urls: Vec<String> = manifest.caches.iter().map(|c| c.url.clone()).collect();
        nx_nix::nix::realise(nix, &plan.installables, &urls, &mut |note| {
            draw(ui, note);
        })
        .with_context(|| realise_failed(&plan.build))
    }
}

/// The manifest's tool packages at a release: one `kept` from the same pin
/// as it was, the rest from `evaluated`. One the release lacks or refuses
/// takes its build from `held` when there is one (returned by attribute),
/// and is a problem otherwise.
fn lock<'a>(
    manifest: &Manifest,
    evaluated: &BTreeMap<String, Evaluated>,
    kept: &dyn Fn(&str) -> Option<&'a ToolPackage>,
    held: Option<&LockedTool>,
    version: &str,
) -> Result<(Vec<ToolPackage>, Vec<String>)> {
    let mut packages = Vec::new();
    let mut problems = Vec::new();
    let mut holding = Vec::new();
    for attr in &manifest.tool.packages {
        if let Some(p) = kept(attr) {
            packages.push(p.clone());
            continue;
        }
        let found = matches!(evaluated.get(attr), Some(Evaluated::Found { .. }));
        if let Some(p) = held.and_then(|h| h.package(attr)).filter(|_| !found) {
            packages.push(p.clone());
            holding.push(attr.clone());
            continue;
        }
        match evaluated.get(attr) {
            Some(Evaluated::Found {
                name,
                version,
                drv,
                outputs,
            }) => packages.push(ToolPackage {
                attr: attr.clone(),
                name: name.clone(),
                version: version.clone(),
                drv: drv.clone(),
                outputs: outputs
                    .iter()
                    .map(|o| ToolOutput {
                        name: o.name.clone(),
                        path: o.path.clone(),
                    })
                    .collect(),
                bins: Vec::new(),
                unfree: manifest.tool.unfree.contains(name),
            }),
            Some(Evaluated::Refused {
                unfree,
                broken,
                insecure,
            }) => problems.push(refused(attr, *unfree, *broken, *insecure)),
            Some(Evaluated::Missing) | None => {
                problems.push(format!("nixpkgs {version} has no package {attr}"));
            }
        }
    }
    if !problems.is_empty() {
        bail!(problems.join("\n"));
    }
    Ok((packages, holding))
}

/// What a failed realise says. A package no cache has was built here, and
/// for an unfree one that is the vendor's download repackaged: the vendor
/// may have an RPM repo or an installer that works without the build.
fn realise_failed(built: &[String]) -> String {
    if built.is_empty() {
        return "realising the tool tier".to_owned();
    }
    let names = built.join(", ");
    format!(
        "realising the tool tier, which builds {names} here since no binary cache has \
         {them}; if the vendor publishes an RPM repo, nx repo add <its .repo URL> and then \
         nx add --host <its package> take it from there; otherwise use the vendor's \
         installer",
        them = if built.len() == 1 { "it" } else { "them" }
    )
}

/// The catalog for a command that only reads (search, info): at the pinned
/// release, or the channel's current one. Missing it isn't fatal: those
/// commands still answer from the repos, with a warning.
pub fn catalog_to_read(ui: &mut Ui, pinned_to: Option<&LockedTool>) -> Option<Catalog> {
    if !available() {
        return None;
    }
    let loaded = Tier::open(ui, pinned_to, false).and_then(|mut t| {
        t.catalog(ui)?;
        Ok(t.catalog.take())
    });
    match loaded {
        Ok(c) => c,
        Err(e) => {
            ui.warn(format_args!("leaving nixpkgs out: {e:#}"));
            None
        }
    }
}

/// What a refused evaluation means for the user.
fn refused(attr: &str, unfree: bool, broken: bool, insecure: bool) -> String {
    if unfree {
        format!("{attr} is unfree; if its license is fine with you: nx add --allow-unfree {attr}")
    } else if broken {
        format!("{attr} is marked broken in nixpkgs")
    } else if insecure {
        format!("{attr} is marked insecure in nixpkgs; nx doesn't install it")
    } else {
        format!("nixpkgs refused to evaluate {attr}")
    }
}

fn draw(ui: &mut Ui, note: Note) {
    match note {
        Note::Fetched { name, size } => {
            let st = ui.err.style;
            ui.err.line(format_args!(
                "{} {name}  {}",
                st.cyan(ui::SYM_FETCH),
                st.dim(&ui::size(size))
            ));
        }
        Note::Building { name } => ui.service(format_args!("building {name}")),
        Note::Retry { what, why } => ui.warn(format_args!("{why}; retrying {what}")),
        Note::Warning(w) => ui.warn(w),
        // The failed run's error carries it.
        Note::Built { .. } | Note::Error(_) | Note::Raw(_) => {}
    }
}

/// cache.nixos.org, then the manifest's extra caches.
fn substituters(caches: &[CacheEntry]) -> Vec<String> {
    std::iter::once(nx_nix::CACHE.to_owned())
        .chain(caches.iter().map(|c| c.url.clone()))
        .collect()
}

fn local(path: &str) -> bool {
    Path::new(path).exists()
}

/// What to realise.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Store paths to substitute, and `<drv>^<outputs>` to build.
    pub installables: Vec<String>,
    /// What isn't local yet.
    pub sum: Sum,
    /// Attributes no cache has: built from their derivation.
    pub build: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.installables.is_empty()
    }
}

/// A package whose outputs a cache has is substituted by store path; one
/// whose outputs no cache has is built.
fn plan_from(tool: &LockedTool, closure: &BTreeMap<String, narinfo::Info>) -> Plan {
    let mut plan = Plan {
        sum: narinfo::total(closure),
        ..Plan::default()
    };
    for p in &tool.packages {
        let wanted: Vec<&ToolOutput> = p.outputs.iter().filter(|o| !local(&o.path)).collect();
        if wanted.is_empty() {
            continue;
        }
        let uncached = wanted
            .iter()
            .any(|o| closure.get(&o.path).is_none_or(|i| i.missing));
        if uncached {
            plan.installables.push(from_drv(p));
            plan.build.push(p.attr.clone());
        } else {
            plan.installables
                .extend(wanted.iter().map(|o| o.path.clone()));
        }
    }
    plan
}

/// `<drv>^<outputs>`: what Nix builds a package's outputs from when no
/// cache has them.
fn from_drv(p: &ToolPackage) -> String {
    let names: Vec<&str> = p.outputs.iter().map(|o| o.name.as_str()).collect();
    format!("{}^{}", p.drv, names.join(","))
}

/// The commands an output has: the executables in its `bin/`, without
/// the dot-files wrappers keep their originals in.
pub fn commands(out: &str) -> Vec<Bin> {
    let Ok(entries) = std::fs::read_dir(Path::new(out).join("bin")) else {
        return Vec::new();
    };
    let mut bins: Vec<Bin> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let meta = std::fs::metadata(e.path()).ok()?;
            (!name.starts_with('.') && meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .then(|| Bin {
                    path: format!("{out}/bin/{name}"),
                    name,
                })
        })
        .collect();
    bins.sort_by(|a, b| a.name.cmp(&b.name));
    bins
}

/// Which names a generation's tool tier leaves out.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Exposure {
    /// A command the image or the host tier has (they come first on
    /// `PATH`): `(name, attr, owner)`. Not exposed.
    pub hidden: Vec<(String, String, String)>,
    /// A command two tool packages have and no override settles:
    /// `(name, the attr that keeps it, the attr refused it)`.
    pub clashes: Vec<(String, String, String)>,
}

/// Fills each package's `bins`: every command of its outputs that nothing
/// else owns. Between tool packages the first in the manifest keeps a
/// name, unless it is in `overrides`, when the later one takes it.
pub fn expose(
    tool: &mut LockedTool,
    owner: &dyn Fn(&str) -> Option<String>,
    overrides: &[String],
    commands: &dyn Fn(&str) -> Vec<Bin>,
) -> Exposure {
    let mut exposure = Exposure::default();
    let mut bins: Vec<Vec<Bin>> = vec![Vec::new(); tool.packages.len()];
    let mut taken: BTreeMap<String, usize> = BTreeMap::new();
    for (i, p) in tool.packages.iter().enumerate() {
        let mut seen = HashSet::new();
        for b in p.outputs.iter().flat_map(|o| commands(&o.path)) {
            if !seen.insert(b.name.clone()) {
                continue;
            }
            if let Some(who) = owner(&b.name) {
                exposure.hidden.push((b.name, p.attr.clone(), who));
                continue;
            }
            match taken.get(&b.name).copied() {
                Some(j) if overrides.contains(&b.name) => {
                    bins[j].retain(|x| x.name != b.name);
                    taken.insert(b.name.clone(), i);
                    bins[i].push(b);
                }
                Some(j) => {
                    exposure
                        .clashes
                        .push((b.name, tool.packages[j].attr.clone(), p.attr.clone()))
                }
                None => {
                    taken.insert(b.name.clone(), i);
                    bins[i].push(b);
                }
            }
        }
    }
    for (p, b) in tool.packages.iter_mut().zip(bins) {
        p.bins = b;
    }
    exposure
}

/// Refuses what a change would expose badly, and warns about the rest:
/// a package just added (`fresh`) must not lose a command to another tool
/// package without `--override`, and must expose something.
pub fn check(
    ui: &mut Ui,
    tool: &LockedTool,
    exposure: &Exposure,
    fresh: &[String],
    previous: Option<&LockedTool>,
    overrides: &[String],
) -> Result<()> {
    let mut problems = Vec::new();
    for (name, kept, refused) in &exposure.clashes {
        if fresh.contains(refused) {
            problems.push(format!(
                "{refused} has the command {name}, which {kept} exposes; \
                 nx add {refused} --override {name} lets {refused} have it"
            ));
        } else {
            ui.warn(format_args!(
                "{refused} and {kept} both have {name}; {kept}'s is exposed"
            ));
        }
    }
    // A new package whose command the image or the host tier has is
    // refused (design, *Name ownership*) unless every such name is
    // overridden: then it is added without them, since what comes first on
    // PATH would run anyway.
    for attr in fresh {
        let Some(package) = tool.package(attr) else {
            continue;
        };
        let hidden: Vec<&str> = exposure
            .hidden
            .iter()
            .filter(|(_, a, _)| a == attr)
            .map(|(n, _, _)| n.as_str())
            .collect();
        let owned: Vec<&str> = hidden
            .iter()
            .copied()
            .filter(|n| !overrides.iter().any(|o| o == n))
            .collect();
        // The tool tier exposes commands and nothing else.
        let lost = exposure.clashes.iter().any(|(_, _, r)| r == attr);
        if package.bins.is_empty() && !lost {
            problems.push(match hidden.len() {
                0 => format!("{attr} has no commands, and the tool tier exposes only commands"),
                1 => format!("{} already exists on this system", hidden[0]),
                _ => format!("{} already exist on this system", hidden.join(", ")),
            });
            continue;
        }
        if hidden.is_empty() {
            continue;
        }
        if owned.is_empty() {
            ui.warn(format_args!(
                "{attr}: {} not exposed; the image or the host tier has {}, as you allowed",
                hidden.join(", "),
                if hidden.len() == 1 { "it" } else { "them" }
            ));
        } else {
            problems.push(format!(
                "{attr} has {}, which the image or the host tier already has (it comes first on \
                 PATH, so nx can't expose {}); nx add {attr} --override {} adds it without {}",
                owned.join(", "),
                if owned.len() == 1 { "it" } else { "them" },
                owned.join(" --override "),
                if owned.len() == 1 { "it" } else { "them" },
            ));
        }
    }
    // A command the image took over since the last generation.
    if let Some(prev) = previous {
        for (name, attr, who) in &exposure.hidden {
            let was = prev
                .package(attr)
                .is_some_and(|p| p.bins.iter().any(|b| &b.name == name));
            if was && !fresh.contains(attr) {
                ui.warn(format_args!(
                    "{name} is {who}'s now; {attr}'s is no longer exposed"
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        bail!(problems.join("\n"))
    }
}

/// Whether two locks hold the same tool tier: the same release and
/// packages with the same store paths.
pub fn same(a: Option<&LockedTool>, b: Option<&LockedTool>) -> bool {
    let key = |t: Option<&LockedTool>| {
        t.filter(|t| !t.packages.is_empty()).map(|t| {
            (
                t.rev.clone(),
                t.packages
                    .iter()
                    .map(|p| (p.attr.clone(), p.outputs.clone()))
                    .collect::<Vec<_>>(),
            )
        })
    };
    key(a) == key(b)
}

/// The shim every exposed command is a hard link to, copied into the
/// state directory once per build of it, so generations can link to it
/// (`NX_SHIM` names another, for a VM test).
pub fn shim(state: &State) -> Result<PathBuf> {
    let source = find_shim()?;
    let sha = nx_rpm::packages::sha256_file(&source)
        .with_context(|| format!("reading {}", source.display()))?;
    let dir = state.root().join("store/shim");
    let dest = dir.join(&sha);
    if dest.is_file() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{sha}.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(&source, &tmp).with_context(|| format!("copying {}", source.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .and_then(|()| std::fs::File::open(&tmp)?.sync_all())
        .and_then(|()| std::fs::rename(&tmp, &dest))
        .with_context(|| format!("installing {}", dest.display()))?;
    Ok(dest)
}

/// `nx-shim` beside `nx`, or in `../libexec/nx` from it.
fn find_shim() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("NX_SHIM").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().context("finding the nx binary")?;
    let dir = exe.parent().context("the nx binary has no directory")?;
    let candidates = [
        dir.join("nx-shim"),
        dir.join("../libexec/nx/nx-shim"),
        PathBuf::from("/usr/libexec/nx/nx-shim"),
    ];
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .with_context(|| {
            format!(
                "nx-shim is missing (looked in {}); the tool tier needs it",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Registers `g`'s tool-tier outputs as GC roots, in `gcroots/` under its
/// final name. A generation whose paths aren't rooted must not go live: a
/// plain `nix store gc` would delete what its commands run.
pub fn add_roots(nix: &dyn Nix, g: &Generation) -> Result<()> {
    let Some(tool) = g.lock.tool.as_ref().filter(|t| !t.packages.is_empty()) else {
        return Ok(());
    };
    root(nix, g, tool).with_context(|| {
        format!(
            "generation {} is built, but its Nix paths are not GC roots, so it was not made live",
            g.number
        )
    })
}

/// Whether some output of `tool` isn't in the store (collected since it
/// was realised).
pub fn incomplete(tool: &LockedTool) -> bool {
    tool.packages
        .iter()
        .flat_map(ToolPackage::paths)
        .any(|p| !local(p))
}

/// Realises again the outputs of `tool` the store lost: from their
/// derivation when it is still there (Nix substitutes what a cache has and
/// builds the rest), else by store path from the caches.
pub fn restore(ui: &mut Ui, nix: &dyn Nix, tool: &LockedTool, caches: &[CacheEntry]) -> Result<()> {
    let mut installables = Vec::new();
    for p in &tool.packages {
        let lost: Vec<&ToolOutput> = p.outputs.iter().filter(|o| !local(&o.path)).collect();
        if lost.is_empty() {
            continue;
        }
        if Path::new(&p.drv).exists() {
            installables.push(from_drv(p));
        } else {
            installables.extend(lost.iter().map(|o| o.path.clone()));
        }
    }
    if installables.is_empty() {
        return Ok(());
    }
    ui.step("fetching tool-tier paths the Nix store no longer has");
    let urls: Vec<String> = caches.iter().map(|c| c.url.clone()).collect();
    nx_nix::nix::realise(nix, &installables, &urls, &mut |note| draw(ui, note))
        .context("realising the tool tier again")
}

/// Roots `tool`'s outputs that are still in the store at `g`'s
/// `gcroots/root`.
pub fn root(nix: &dyn Nix, g: &Generation, tool: &LockedTool) -> Result<()> {
    let paths: Vec<String> = tool
        .packages
        .iter()
        .flat_map(ToolPackage::paths)
        .filter(|p| local(p))
        .map(str::to_owned)
        .collect();
    let dir = g.gcroots();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    nx_nix::nix::add_roots(nix, &paths, &dir.join("root")).context("registering GC roots")
}

/// Whether `g` holds its roots already.
pub fn rooted(g: &Generation) -> bool {
    std::fs::symlink_metadata(g.gcroots().join("root")).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(attr: &str, outputs: &[&str]) -> ToolPackage {
        ToolPackage {
            attr: attr.into(),
            name: attr.into(),
            version: "1".into(),
            drv: format!("/nix/store/00000000000000000000000000000000-{attr}.drv"),
            outputs: outputs
                .iter()
                .map(|o| ToolOutput {
                    name: "out".into(),
                    path: (*o).into(),
                })
                .collect(),
            ..ToolPackage::default()
        }
    }

    fn bins(names: &[&str], out: &str) -> Vec<Bin> {
        names
            .iter()
            .map(|n| Bin {
                name: (*n).into(),
                path: format!("{out}/bin/{n}"),
            })
            .collect()
    }

    /// Three packages: ripgrep (`rg`), git (`git`, `scalar`; the image has
    /// `git`) and gitAndTools (`git`, `tig`).
    fn three() -> LockedTool {
        LockedTool {
            packages: vec![
                package("ripgrep", &["/nix/store/a-rg"]),
                package("gitFull", &["/nix/store/b-git"]),
                package("tig", &["/nix/store/c-tig"]),
            ],
            ..LockedTool::default()
        }
    }

    fn listing(out: &str) -> Vec<Bin> {
        match out {
            "/nix/store/a-rg" => bins(&["rg"], out),
            "/nix/store/b-git" => bins(&["git", "scalar"], out),
            "/nix/store/c-tig" => bins(&["scalar", "tig"], out),
            _ => Vec::new(),
        }
    }

    fn image(name: &str) -> Option<String> {
        (name == "git").then(|| "the image".to_owned())
    }

    #[test]
    fn exposes_what_nothing_else_owns() {
        let mut tool = three();
        let e = expose(&mut tool, &image, &[], &listing);
        let names = |t: &LockedTool, i: usize| -> Vec<String> {
            t.packages[i].bins.iter().map(|b| b.name.clone()).collect()
        };
        assert_eq!(names(&tool, 0), ["rg"]);
        assert_eq!(names(&tool, 1), ["scalar"]);
        assert_eq!(names(&tool, 2), ["tig"]);
        assert_eq!(
            e.hidden,
            [("git".into(), "gitFull".into(), "the image".into())]
        );
        assert_eq!(
            e.clashes,
            [("scalar".into(), "gitFull".into(), "tig".into())]
        );
        // An override hands the name to the later package.
        let e = expose(&mut tool, &image, &["scalar".into()], &listing);
        assert!(e.clashes.is_empty());
        assert_eq!(tool.packages[1].bins.len(), 0);
        assert_eq!(names(&tool, 2), ["scalar", "tig"]);
    }

    #[test]
    fn a_failed_build_points_to_the_vendor() {
        assert_eq!(realise_failed(&[]), "realising the tool tier");
        assert_eq!(
            realise_failed(&["ngrok".into()]),
            "realising the tool tier, which builds ngrok here since no binary cache has it; \
             if the vendor publishes an RPM repo, nx repo add <its .repo URL> and then nx add \
             --host <its package> take it from there; otherwise use the vendor's installer"
        );
    }

    #[test]
    fn a_new_package_may_not_take_or_lose_a_command() {
        let buf = crate::ui::SharedBuf::default();
        let mut ui = Ui::plain(crate::ui::SharedBuf::default(), buf.clone());
        let mut tool = three();
        let e = expose(&mut tool, &image, &[], &listing);
        let err = check(&mut ui, &tool, &e, &["tig".into()], None, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "tig has the command scalar, which gitFull exposes; nx add tig --override scalar lets tig have it"
        );
        // Added before: a warning only.
        check(&mut ui, &tool, &e, &[], None, &[]).ok();
        assert_eq!(
            buf.text(),
            "  ! tig and gitFull both have scalar; gitFull's is exposed\n"
        );
        // One of its commands is the image's: refused, unless overridden.
        let err = check(&mut ui, &tool, &e, &["gitFull".into()], None, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "gitFull has git, which the image or the host tier already has (it comes first on \
             PATH, so nx can't expose it); nx add gitFull --override git adds it without it"
        );
        assert!(
            check(
                &mut ui,
                &tool,
                &e,
                &["gitFull".into()],
                None,
                &["git".into()]
            )
            .is_ok()
        );
        // Everything it has, the image has.
        let mut tool = LockedTool {
            packages: vec![package("git", &["/nix/store/d-git"])],
            ..LockedTool::default()
        };
        let only_git = |out: &str| bins(&["git"], out);
        let e = expose(&mut tool, &image, &[], &only_git);
        for overrides in [vec![], vec!["git".to_owned()]] {
            let err = check(&mut ui, &tool, &e, &["git".into()], None, &overrides)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert_eq!(err, "git already exists on this system");
        }
        // Nothing to expose at all.
        let mut tool = LockedTool {
            packages: vec![package("requests", &["/nix/store/e-requests"])],
            ..LockedTool::default()
        };
        let e = expose(&mut tool, &image, &[], &|out: &str| bins(&[], out));
        let err = check(&mut ui, &tool, &e, &["requests".into()], None, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "requests has no commands, and the tool tier exposes only commands"
        );
        assert!(check(&mut ui, &tool, &e, &[], None, &[]).is_ok());
    }

    #[test]
    fn substitutes_what_a_cache_has_and_builds_the_rest() {
        let tool = LockedTool {
            packages: vec![
                package(
                    "ripgrep",
                    &["/nix/store/11111111111111111111111111111111-rg"],
                ),
                package(
                    "ngrok",
                    &["/nix/store/22222222222222222222222222222222-ngrok"],
                ),
            ],
            ..LockedTool::default()
        };
        let mut closure = BTreeMap::new();
        closure.insert(
            "/nix/store/11111111111111111111111111111111-rg".to_owned(),
            narinfo::Info {
                file_size: 10,
                nar_size: 40,
                ..narinfo::Info::default()
            },
        );
        closure.insert(
            "/nix/store/22222222222222222222222222222222-ngrok".to_owned(),
            narinfo::Info {
                missing: true,
                ..narinfo::Info::default()
            },
        );
        let plan = plan_from(&tool, &closure);
        assert_eq!(
            plan.installables,
            [
                "/nix/store/11111111111111111111111111111111-rg",
                "/nix/store/00000000000000000000000000000000-ngrok.drv^out"
            ]
        );
        assert_eq!(plan.build, ["ngrok"]);
        assert_eq!(plan.sum.download, 10);
        assert_eq!(plan.sum.unbuilt, 1);
    }

    #[test]
    fn same_tier_ignores_what_is_exposed() {
        let a = three();
        let mut b = three();
        b.packages[0].bins = bins(&["rg"], "/nix/store/a-rg");
        assert!(same(Some(&a), Some(&b)));
        b.packages.pop();
        assert!(!same(Some(&a), Some(&b)));
        assert!(same(None, Some(&LockedTool::default())));
    }

    #[test]
    fn an_upgrade_keeps_a_package_the_release_dropped() {
        let mut manifest = Manifest::default();
        manifest.tool.packages = vec!["ripgrep".into(), "tig".into()];
        let evaluated = BTreeMap::from([
            (
                "ripgrep".to_owned(),
                Evaluated::Found {
                    name: "ripgrep".into(),
                    version: "15.2.0".into(),
                    drv: "/nix/store/x-ripgrep.drv".into(),
                    outputs: Vec::new(),
                },
            ),
            ("tig".to_owned(), Evaluated::Missing),
        ]);
        let none = |_: &str| None;
        let was = three();
        let (packages, held) = match lock(&manifest, &evaluated, &none, Some(&was), "26.11") {
            Ok(v) => v,
            Err(e) => panic!("{e:#}"),
        };
        assert_eq!(held, ["tig"]);
        assert_eq!(packages[0].version, "15.2.0");
        assert_eq!(packages[1], was.packages[2]);
        // Without a build to keep (nx add), it is an error.
        let err = lock(&manifest, &evaluated, &none, None, "26.11").map(|_| ());
        assert_eq!(
            err.map_err(|e| e.to_string()),
            Err("nixpkgs 26.11 has no package tig".to_owned())
        );
    }
}
