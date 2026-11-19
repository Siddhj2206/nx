//! `nx info`: one host-tier package in the `info` layout.

use anyhow::{Result, bail};
use nx_rpm::Universe;

use crate::ui::{self, Ui};

pub fn run(ui: &mut Ui, universe: &Universe, spec: &str) -> Result<()> {
    let Some(p) = universe.info(spec)? else {
        bail!("no package matches {spec} (try nx search {spec})");
    };
    let description = universe.pool.description(p.id);
    let s = &mut ui.out;
    let st = s.style;
    s.blank();
    let mut head = format!("{} {}", st.bold(&p.name), st.dim(&p.evr));
    if p.installed {
        head.push_str(&format!(
            "  {}",
            st.green(&format!("{} in the image", ui::SYM_DONE))
        ));
    }
    s.line(head);
    if !p.summary.is_empty() {
        s.line(&p.summary);
    }
    if !p.url.is_empty() {
        s.line(st.cyan(&p.url));
    }
    s.blank();
    if p.name != spec {
        s.row(
            "Matched",
            &format!("{spec} {}", st.dim(&format!("(provided by {})", p.name))),
        );
    }
    s.row(
        "Tier",
        &format!("host {}", st.dim("· Fedora RPM on the image's /usr")),
    );
    s.row("Repo", if p.installed { "the image" } else { &p.repo });
    s.row("Source", &p.source);
    s.row("License", &p.license);
    s.row("Arch", &p.arch);
    if !p.installed {
        s.row(
            "Size",
            &format!(
                "{} {}",
                ui::size(p.download_size),
                st.dim(&format!("· {} installed", ui::size(p.install_size)))
            ),
        );
    }
    if !description.is_empty() && description != p.summary {
        s.blank();
        for line in description.lines().take(8) {
            s.line(st.dim(line.trim_end()));
        }
    }
    s.blank();
    Ok(())
}
