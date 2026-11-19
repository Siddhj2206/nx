//! `nx add`: M1 resolves only (`--dry-run`); unpacking and activation come
//! with M2 and M3.

use anyhow::{Result, bail};
use nx_rpm::{Plan, Universe};

use crate::ui::{self, Ui};

pub fn run(ui: &mut Ui, universe: &mut Universe, specs: &[String], dry_run: bool) -> Result<()> {
    if !dry_run {
        bail!("nx add can only plan for now: run nx add --dry-run (installing lands in M3)");
    }
    let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
    let packages = match universe.plan(&specs)? {
        Plan::Install(p) => p,
        Plan::Problems(mut problems) => {
            // A missing repo, not a missing package, is the likelier cause.
            if !universe.skipped.is_empty() {
                let repos: Vec<&str> = universe.skipped.iter().map(|s| s.repo.as_str()).collect();
                problems.push(format!(
                    "{} could not be loaded (see above); try again",
                    repos.join(", ")
                ));
            }
            bail!("can't add {}\n{}", specs.join(" "), problems.join("\n"));
        }
    };
    let s = &mut ui.out;
    let st = s.style;
    if packages.is_empty() {
        s.line(format_args!(
            "{} {} already in the image",
            st.green(ui::SYM_DONE),
            specs.join(", ")
        ));
        return Ok(());
    }
    s.blank();
    let name_w = packages.iter().map(|p| p.name.len()).max().unwrap_or(0) + 2;
    let evr_w = packages.iter().map(|p| p.evr.len()).max().unwrap_or(0) + 2;
    let (mut download, mut installed) = (0, 0);
    for p in &packages {
        download += p.download_size;
        installed += p.install_size;
        s.line(format_args!(
            "{} {}{}{}  {}",
            st.green(ui::SYM_ADDED),
            ui::pad(&p.name, name_w),
            st.dim(&ui::pad(&p.evr, evr_w)),
            st.dim(&ui::pad(&p.repo, 10)),
            st.dim(&ui::size(p.download_size)),
        ));
    }
    s.blank();
    s.line(st.dim(&format!(
        "{} · {} download, {} installed · dry run, nothing changed",
        ui::plural(packages.len(), "package"),
        ui::size(download),
        ui::size(installed)
    )));
    Ok(())
}
