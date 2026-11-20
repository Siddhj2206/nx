//! `nx migrate`: moves nx 0.x's packages (the user's Nix profile) into nx,
//! then takes them out of the profile and drops nx 0.x's nixpkgs pin from
//! the user registry. Each package is routed as `nx add` would, so a
//! compiler goes to the host tier. A package nx can't add stays in the
//! profile, named; one that isn't from nixpkgs is left alone.

use anyhow::Result;
use nx_nix::Nix;
use nx_nix::profile::{self, Element, Legacy};

use crate::add;
use crate::host::Machine;
use crate::ui::{self, Stream, Ui};

pub fn run(
    ui: &mut Ui,
    machine: &Machine,
    legacy: &Legacy,
    nix: &dyn Nix,
    dry_run: bool,
) -> Result<()> {
    let elements = legacy.elements()?;
    let (nixpkgs, other): (Vec<Element>, Vec<Element>) =
        elements.into_iter().partition(|e| e.nixpkgs);
    for e in &other {
        ui.warn(format_args!(
            "{} isn't from nixpkgs ({}); it stays in the Nix profile",
            e.name, e.attr
        ));
    }
    let pinned = legacy.pins_nixpkgs();
    if nixpkgs.is_empty() && !pinned {
        ui.done("nothing to migrate from nx 0.x");
        return Ok(());
    }
    if dry_run {
        render_plan(&mut ui.out, &nixpkgs, pinned);
        return Ok(());
    }
    let moved = add_all(ui, machine, &nixpkgs);
    let names: Vec<String> = moved.iter().map(|e| e.name.clone()).collect();
    if !names.is_empty() {
        ui.step(format_args!(
            "removing {} from the Nix profile",
            names.join(", ")
        ));
        profile::remove(nix, &names)?;
    }
    if pinned && moved.len() == nixpkgs.len() {
        ui.step("removing nx 0.x's nixpkgs pin from the user registry");
        profile::unpin(nix)?;
    }
    let left = nixpkgs.len() - moved.len();
    if left > 0 {
        ui.warn(format_args!(
            "{} stayed in the Nix profile (above says why); nx migrate again once that is fixed",
            ui::plural(left, "package")
        ));
    }
    ui.done(format_args!(
        "{} moved to nx{}",
        ui::plural(moved.len(), "package"),
        if moved.is_empty() {
            ""
        } else {
            " · nix profile rollback brings the profile back"
        }
    ));
    Ok(())
}

/// `nx add` for every package at once; when that fails, one at a time, so
/// one package doesn't hold back the rest. Returns the ones nx has now.
fn add_all<'e>(ui: &mut Ui, machine: &Machine, elements: &'e [Element]) -> Vec<&'e Element> {
    if elements.is_empty() {
        return Vec::new();
    }
    // They were installed with nx 0.x already: an unfree one was accepted.
    let opts = add::Options {
        allow_unfree: true,
        ..add::Options::default()
    };
    let attrs: Vec<String> = elements.iter().map(|e| e.attr.clone()).collect();
    match add::run(ui, machine, &attrs, &opts) {
        Ok(()) => return elements.iter().collect(),
        Err(e) => ui.warn(format_args!(
            "adding them together failed ({e:#}); adding one at a time"
        )),
    }
    let mut moved = Vec::new();
    for e in elements {
        match add::run(ui, machine, std::slice::from_ref(&e.attr), &opts) {
            Ok(()) => moved.push(e),
            Err(err) => ui.warn(format_args!("{} stays in the Nix profile: {err:#}", e.name)),
        }
    }
    moved
}

/// `--dry-run`: what would move.
fn render_plan(out: &mut Stream, elements: &[Element], pinned: bool) {
    let st = out.style;
    for e in elements {
        out.line(format_args!(
            "{} {} {}",
            st.green(ui::SYM_ADDED),
            e.attr,
            st.dim("· from the Nix profile")
        ));
    }
    let mut what = format!("{} to move", ui::plural(elements.len(), "package"));
    if pinned {
        what.push_str(", the registry pin to drop");
    }
    out.line(st.dim(&format!("{what} · dry run, nothing changed")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    #[test]
    fn a_dry_run_lists_what_would_move() {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        let e = Element {
            name: "ripgrep".into(),
            attr: "ripgrep".into(),
            nixpkgs: true,
        };
        render_plan(&mut ui.out, &[e], true);
        assert_eq!(
            buf.text(),
            "  + ripgrep · from the Nix profile\n\
             \x20 1 package to move, the registry pin to drop · dry run, nothing changed\n"
        );
    }
}
