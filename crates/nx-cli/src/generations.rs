//! `nx generations`: every generation on disk, grouped by the image it was
//! built for (booted first), which one is current, and what each changed
//! compared with the one it was built from, in both tiers.

use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use nx_core::{DeploymentSource, Deployments, Generation, Lock, State};

use crate::ui::{self, Stream, Ui};

/// One generation as `nx generations` shows it.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    number: u32,
    /// Seconds since the epoch; `None` when the file's time is unreadable.
    made: Option<u64>,
    host: usize,
    tools: usize,
    added: Vec<String>,
    removed: Vec<String>,
    /// In both, at another version.
    changed: Vec<String>,
    /// Built from this generation, which is gone or another image's:
    /// nothing of its own image to diff against, so no diff.
    from: Option<u32>,
    current: bool,
}

/// The generations built for one image, oldest first.
#[derive(Debug, PartialEq, Eq)]
struct Group {
    /// `Booted image`, `Staged image`…; `None` off a bootc system, where
    /// there is one group and no heading.
    title: Option<String>,
    /// The image's version and short digest.
    detail: String,
    rows: Vec<Row>,
}

pub fn run(ui: &mut Ui, source: &dyn DeploymentSource, state: &State) -> Result<()> {
    // Off a bootc system there is nothing to group by.
    let deployments = source.deployments().ok();
    let groups = groups(ui, state, deployments.as_ref())?;
    render(&mut ui.out, &groups);
    Ok(())
}

/// Every readable generation with its image digest and lock.
fn read_all(ui: &mut Ui, state: &State) -> Result<Vec<(Generation, Option<u64>)>> {
    let mut all = Vec::new();
    for number in state.generations()? {
        match Generation::read(state, number) {
            Ok(g) => {
                let made = std::fs::metadata(g.dir.join("lock.toml"))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                all.push((g, made));
            }
            Err(e) => ui.warn(format_args!("generation {number}: {e}")),
        }
    }
    Ok(all)
}

fn groups(ui: &mut Ui, state: &State, deployments: Option<&Deployments>) -> Result<Vec<Group>> {
    let current = state.current_number().context("reading `current`")?;
    let all = read_all(ui, state)?;
    let locks: BTreeMap<u32, &Lock> = all.iter().map(|(g, _)| (g.number, &g.lock)).collect();

    // Images in the order they are shown: booted, staged, rollback, then
    // any other an older generation was built for.
    let mut order: Vec<(Option<String>, Option<String>, String)> = Vec::new();
    if let Some(d) = deployments {
        for (title, dep) in [
            ("Booted image", d.booted()),
            ("Staged image", d.staged()),
            ("Rollback image", d.rollback()),
        ] {
            let Some(dep) = dep else { continue };
            if order.iter().any(|(digest, ..)| *digest == dep.image_digest) {
                continue;
            }
            let detail = image_detail(dep.version.as_deref(), dep.image_digest.as_deref());
            order.push((dep.image_digest.clone(), Some(title.into()), detail));
        }
    }
    for (g, _) in &all {
        if !order.iter().any(|(digest, ..)| *digest == g.lock.image) {
            let title = (deployments.is_some() || g.lock.image.is_some())
                .then(|| "Image no longer deployed".to_owned());
            let detail = image_detail(None, g.lock.image.as_deref());
            order.push((g.lock.image.clone(), title, detail));
        }
    }

    let mut groups = Vec::new();
    for (digest, title, detail) in order {
        let rows: Vec<Row> = all
            .iter()
            .filter(|(g, _)| g.lock.image == digest)
            .map(|(g, made)| {
                let base = base(g.number, &g.lock, &locks);
                row(g, *made, base, current == Some(g.number))
            })
            .collect();
        if !rows.is_empty() {
            groups.push(Group {
                title,
                detail,
                rows,
            });
        }
    }
    Ok(groups)
}

/// What a generation is diffed against: its parent when that was built for
/// the same image, else the newest earlier generation for it. A sync for
/// another image names the booted image's generation as its parent, and a
/// diff across images would show the images' differences, not the change.
fn base<'l>(number: u32, lock: &Lock, locks: &BTreeMap<u32, &'l Lock>) -> Option<&'l Lock> {
    if let Some(parent) = lock.parent.and_then(|p| locks.get(&p))
        && parent.image == lock.image
    {
        return Some(parent);
    }
    locks
        .range(..number)
        .rev()
        .map(|(_, l)| *l)
        .find(|l| l.image == lock.image)
}

/// Every package of both tiers by name, with its version.
fn versions(lock: &Lock) -> BTreeMap<&str, &str> {
    let host = lock
        .host
        .iter()
        .map(|p| (p.name.as_str(), p.nevra.as_str()));
    let tools = lock
        .tool
        .iter()
        .flat_map(|t| &t.packages)
        .map(|p| (p.attr.as_str(), p.version.as_str()));
    host.chain(tools).collect()
}

fn row(g: &Generation, made: Option<u64>, base: Option<&Lock>, current: bool) -> Row {
    let from = g.lock.parent.filter(|_| base.is_none());
    let now = if from.is_some() {
        BTreeMap::new()
    } else {
        versions(&g.lock)
    };
    let before = base.map(versions).unwrap_or_default();
    let names = |f: &dyn Fn(&&str) -> bool, from: &BTreeMap<&str, &str>| -> Vec<String> {
        from.keys()
            .filter(|n| f(n))
            .map(|n| (*n).to_owned())
            .collect()
    };
    Row {
        number: g.number,
        made,
        host: g.lock.host.len(),
        tools: g.lock.tool.as_ref().map_or(0, |t| t.packages.len()),
        added: names(&|n| !before.contains_key(n), &now),
        removed: names(&|n| !now.contains_key(n), &before),
        changed: names(
            &|n| before.get(n).is_some_and(|v| Some(v) != now.get(n)),
            &now,
        ),
        from,
        current,
    }
}

/// `45.20261118 · sha256:92409d3fcf84`.
fn image_detail(version: Option<&str>, digest: Option<&str>) -> String {
    [version, digest.map(ui::short_digest)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Each image under its heading, newest generation first, like a log.
fn render(out: &mut Stream, groups: &[Group]) {
    let style = out.style;
    if groups.is_empty() {
        out.line(style.dim("No generations yet · nx add <name> makes the first"));
        return;
    }
    let size = |r: &Row| {
        let mut s = format!("{} host", r.host);
        if r.tools > 0 {
            s.push_str(&format!(" · {}", ui::plural(r.tools, "tool")));
        }
        s
    };
    let width = groups
        .iter()
        .flat_map(|g| &g.rows)
        .map(|r| size(r).chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    for (i, group) in groups.iter().enumerate() {
        let indent = match &group.title {
            Some(title) => {
                if i > 0 {
                    out.blank();
                }
                out.line(format_args!(
                    "{} {}",
                    style.bold(title),
                    style.dim(&group.detail)
                ));
                "  "
            }
            None => "",
        };
        for r in group.rows.iter().rev() {
            let date = r.made.map_or_else(|| "unknown".to_owned(), ui::date);
            let mut change: Vec<String> = r
                .added
                .iter()
                .map(|n| style.green(&format!("+{n}")))
                .collect();
            change.extend(r.changed.iter().map(|n| style.cyan(&format!("↑{n}"))));
            change.extend(r.removed.iter().map(|n| style.red(&format!("-{n}"))));
            let change = if let Some(n) = r.from {
                style.dim(&format!("from generation {n}"))
            } else if change.is_empty() {
                style.dim("no change")
            } else {
                summarize(&change)
            };
            let mark = if r.current {
                format!("  {}", style.green("current"))
            } else {
                String::new()
            };
            out.line(format_args!(
                "{indent}{}{}{}{change}{mark}",
                ui::pad(&style.bold(&r.number.to_string()), 6),
                ui::pad(&style.dim(&date), 18),
                ui::pad(&style.dim(&size(r)), width),
            ));
        }
    }
}

/// The first few changes, then how many more.
fn summarize(change: &[String]) -> String {
    const SHOWN: usize = 4;
    let mut text = change[..change.len().min(SHOWN)].join(" ");
    if change.len() > SHOWN {
        text.push_str(&format!(" and {} more", change.len() - SHOWN));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;
    use nx_core::{Deployment, LockedPackage, LockedTool, Manifest, ToolPackage};

    const MADE: u64 = 1_795_076_040;

    fn lock(image: Option<&str>, parent: Option<u32>, host: &[&str], tools: &[&str]) -> Lock {
        let mut lock = Lock::new(image.map(str::to_owned), "45", "x86_64");
        lock.parent = parent;
        lock.host = host
            .iter()
            .map(|nevra| {
                let name = nevra.split('-').next().unwrap_or(nevra);
                LockedPackage {
                    name: name.into(),
                    nevra: (*nevra).into(),
                    repo: "fedora".into(),
                    sha256: String::new(),
                    requested: vec![name.into()],
                }
            })
            .collect();
        if !tools.is_empty() {
            lock.tool = Some(LockedTool {
                packages: tools
                    .iter()
                    .map(|t| {
                        let (attr, version) = t.split_once('@').unwrap_or((t, "1.0"));
                        ToolPackage {
                            attr: attr.into(),
                            version: version.into(),
                            ..ToolPackage::default()
                        }
                    })
                    .collect(),
                ..LockedTool::default()
            });
        }
        lock
    }

    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A state directory with these generations (numbered from 1).
    fn state(tag: &str, locks: &[Lock], current: u32) -> Result<(Fixture, State)> {
        let dir = std::env::temp_dir().join(format!("nx-generations-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = State::new(&dir);
        for (i, lock) in locks.iter().enumerate() {
            let g = state.generation_dir(i as u32 + 1);
            std::fs::create_dir_all(&g)?;
            std::fs::write(g.join("lock.toml"), lock.to_toml()?)?;
            std::fs::write(g.join("manifest.toml"), Manifest::default().to_toml()?)?;
        }
        state.set_current(current)?;
        Ok((Fixture(dir), state))
    }

    fn deployment(digest: &str, version: &str, booted: bool, staged: bool) -> Deployment {
        Deployment {
            id: digest.into(),
            osname: "default".into(),
            checksum: String::new(),
            serial: 0,
            booted,
            staged,
            pinned: false,
            image_reference: None,
            image_digest: Some(digest.into()),
            version: Some(version.into()),
        }
    }

    fn text(groups: &[Group]) -> String {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, groups);
        buf.text()
    }

    /// Dates are the files' times; the tests pin them.
    fn pin_dates(groups: &mut [Group]) {
        for r in groups.iter_mut().flat_map(|g| &mut g.rows) {
            r.made = Some(MADE);
        }
    }

    #[test]
    fn groups_by_image_and_diffs_both_tiers_against_the_parent() -> Result<()> {
        let (new, old) = ("sha256:aaaaaaaaaaaaaaaa", "sha256:bbbbbbbbbbbbbbbb");
        let locks = [
            // 1: the first, on the old image.
            lock(Some(old), None, &["wayland-1.0"], &[]),
            // 2: a tool added on the old image.
            lock(Some(old), Some(1), &["wayland-1.0"], &["ripgrep@14"]),
            // 3: the new image's, built by sync from 2: nothing of its own
            // image to diff against.
            lock(Some(new), Some(2), &["wayland-1.1"], &["ripgrep@14"]),
            // 4: the tool upgraded on the new image.
            lock(Some(new), Some(3), &["wayland-1.1"], &["ripgrep@15"]),
            // 5: the old image's copy of that change, parent 4 (the new
            // image's): diffed against 2 instead.
            lock(Some(old), Some(4), &["wayland-1.0"], &["ripgrep@15"]),
        ];
        let (_fixture, state) = state("group", &locks, 4)?;
        let deployments = Deployments::new(vec![
            deployment(new, "45.20261118", true, false),
            deployment(old, "45.20261117", false, false),
        ]);
        let mut ui = Ui::plain(SharedBuf::default(), SharedBuf::default());
        let mut groups = groups(&mut ui, &state, Some(&deployments))?;
        pin_dates(&mut groups);
        let date = ui::date(MADE);
        assert_eq!(
            text(&groups),
            format!(
                "  Booted image 45.20261118 · sha256:aaaaaaaaaaaa\n\
                 \x20   4     {date}  1 host · 1 tool  ↑ripgrep  current\n\
                 \x20   3     {date}  1 host · 1 tool  from generation 2\n\
                 \n\
                 \x20 Rollback image 45.20261117 · sha256:bbbbbbbbbbbb\n\
                 \x20   5     {date}  1 host · 1 tool  ↑ripgrep\n\
                 \x20   2     {date}  1 host · 1 tool  +ripgrep\n\
                 \x20   1     {date}  1 host           +wayland\n"
            )
        );
        Ok(())
    }

    #[test]
    fn off_bootc_there_is_one_group_without_a_heading() -> Result<()> {
        let locks = [
            lock(None, None, &["wayland-1.0"], &[]),
            lock(None, Some(1), &["wayland-1.0", "libxkbcommon-1.0"], &[]),
            lock(None, Some(2), &["wayland-1.0"], &[]),
        ];
        let (_fixture, state) = state("plain", &locks, 3)?;
        let mut ui = Ui::plain(SharedBuf::default(), SharedBuf::default());
        let mut groups = groups(&mut ui, &state, None)?;
        pin_dates(&mut groups);
        let date = ui::date(MADE);
        assert_eq!(
            text(&groups),
            format!(
                "  3     {date}  1 host  -libxkbcommon  current\n\
                 \x20 2     {date}  2 host  +libxkbcommon\n\
                 \x20 1     {date}  1 host  +wayland\n"
            )
        );
        assert_eq!(
            text(&[]),
            "  No generations yet · nx add <name> makes the first\n"
        );
        Ok(())
    }

    #[test]
    fn long_changes_are_cut() {
        let change: Vec<String> = (1..=6).map(|n| format!("+p{n}")).collect();
        assert_eq!(summarize(&change), "+p1 +p2 +p3 +p4 and 2 more");
    }
}
