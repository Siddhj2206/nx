//! `nx cache`: extra Nix binary caches for the tool tier (design, *Unfree
//! and uncached packages*). They are recorded in the manifest; when the
//! generation goes live, nxd lists them in the daemon's trusted
//! substituters, and nx passes them on its own `nix` calls.

use anyhow::{Result, bail};
use nx_core::CacheEntry;
use nx_rpm::Refresh;

use crate::apply::Session;
use crate::host::{self, Machine};
use crate::ui::{self, Stream, Ui};

/// Checks what the user typed: an https URL and a `name:key`, neither with
/// whitespace (nxd refuses anything else, so say it here first).
fn entry(url: &str, key: &str) -> Result<CacheEntry> {
    let url = url.trim_end_matches('/');
    let plain = |s: &str| !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control());
    if !plain(url) || !url.starts_with("https://") {
        bail!("a cache is an https URL, not {url}");
    }
    if !plain(key)
        || !key
            .split_once(':')
            .is_some_and(|(n, k)| !n.is_empty() && !k.is_empty())
    {
        bail!(
            "a cache's public key looks like name-1:base64…, as the cache publishes it; not {key}"
        );
    }
    Ok(CacheEntry {
        url: url.to_owned(),
        key: key.to_owned(),
    })
}

pub fn add(ui: &mut Ui, machine: &Machine, url: &str, key: &str) -> Result<()> {
    let wanted = entry(url, key)?;
    let mut session = Session::open(ui, machine, Refresh::IfMissing, false, |m| {
        if m.caches.contains(&wanted) {
            bail!("{} is trusted already", wanted.url);
        }
        m.caches.retain(|c| c.url != wanted.url);
        m.caches.push(wanted.clone());
        Ok(())
    })?;
    let manifest = session.manifest.clone();
    let resolved = session.resolve_manifest(ui, &manifest.host.packages, &[])?;
    let built = session.install(ui, &manifest, &resolved.all)?;
    let style = ui.out.style;
    ui.out.line(format_args!(
        "{} {} {}",
        style.green(ui::SYM_ADDED),
        wanted.url,
        style.dim("· trusted for the tool tier")
    ));
    ui.done(format_args!("generation {}", built.generation.number));
    Ok(())
}

pub fn remove(ui: &mut Ui, machine: &Machine, url: &str) -> Result<()> {
    let url = url.trim_end_matches('/').to_owned();
    let mut session = Session::open(ui, machine, Refresh::IfMissing, false, |m| {
        let before = m.caches.len();
        m.caches.retain(|c| c.url != url);
        if m.caches.len() == before {
            bail!("{url} is not an added cache (nx cache list shows them)");
        }
        Ok(())
    })?;
    let manifest = session.manifest.clone();
    let resolved = session.resolve_manifest(ui, &manifest.host.packages, &[])?;
    let built = session.install(ui, &manifest, &resolved.all)?;
    let style = ui.out.style;
    ui.out
        .line(format_args!("{} {url}", style.red(ui::SYM_REMOVED)));
    ui.done(format_args!("generation {}", built.generation.number));
    Ok(())
}

pub fn list(ui: &mut Ui) -> Result<()> {
    let caches = host::readable(ui, &host::state())?
        .map(|g| g.manifest.caches)
        .unwrap_or_default();
    render(&mut ui.out, &caches);
    Ok(())
}

fn render(out: &mut Stream, caches: &[CacheEntry]) {
    let style = out.style;
    let width = caches
        .iter()
        .map(|c| c.url.len())
        .chain([nx_nix::CACHE.len()])
        .max()
        .unwrap_or(0)
        + 2;
    out.line(format_args!(
        "{}{}",
        ui::pad(&style.bold(nx_nix::CACHE), width),
        style.dim("· always")
    ));
    for c in caches {
        out.line(format_args!(
            "{}{}",
            ui::pad(&style.bold(&c.url), width),
            style.dim(&c.key)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    #[test]
    fn checks_what_it_is_given() {
        assert!(entry("http://x.org", "x:y").is_err());
        assert!(entry("https://x.org", "nokey").is_err());
        assert!(entry("https://x.org", "x: y").is_err());
        let e = entry("https://x.org/", "x.org-1:abc=").ok();
        assert_eq!(e.map(|e| e.url), Some("https://x.org".to_owned()));
    }

    #[test]
    fn lists_the_default_cache_first() {
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(
            &mut ui.out,
            &[CacheEntry {
                url: "https://nix-community.cachix.org".into(),
                key: "nix-community.cachix.org-1:mB9F".into(),
            }],
        );
        assert_eq!(
            buf.text(),
            "  https://cache.nixos.org           · always\n\
             \x20 https://nix-community.cachix.org  nix-community.cachix.org-1:mB9F\n"
        );
    }
}
