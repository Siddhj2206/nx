//! `nx repo add|list|remove`: third-party RPM repos, recorded in the
//! manifest with their key pinned. Adding or removing one builds a new
//! generation, as any manifest change does.

use std::path::Path;

use anyhow::{Context, Result, bail};
use nx_core::{Generation, Manifest, RepoEntry, RpmOstree};
use nx_rpm::repo::{self, Added, Repo, Vars};
use nx_rpm::{Http, Refresh, Ureq};

use crate::apply::Session;
use crate::host;
use crate::ui::{self, Stream, Ui};

/// A repo the user named, its manifest entry, and its key file's text.
struct New {
    entry: RepoEntry,
    key: String,
    fingerprints: Vec<String>,
}

pub fn add(ui: &mut Ui, source: &str, key: Option<&str>, priority: Option<i32>) -> Result<()> {
    let opts = host::options(Path::new("/"), &RpmOstree, Refresh::IfMissing)?;
    let image_ids: Vec<String> = repo::load(&opts.root, &opts.vars)?
        .into_iter()
        .map(|r| r.id)
        .collect();
    let http = Ureq::new();
    let entries = entries(&http, source, key, priority)?;
    let mut manifest = host::current_manifest(&host::state())?;
    let mut new = Vec::new();
    for entry in entries {
        let repo = Repo::added(&added(&entry), &opts.vars)?;
        if image_ids.contains(&repo.id) || manifest.repo(&entry.name).is_some() {
            bail!("repo {} is already enabled", entry.name);
        }
        let url = repo
            .gpgkey
            .first()
            .filter(|k| k.starts_with("https://"))
            .with_context(|| {
                format!("{} needs a key served over https (--key <url>)", entry.name)
            })?;
        let key = String::from_utf8_lossy(&http.get(url)?).into_owned();
        let fingerprints =
            nx_librpm::fingerprints(&key).with_context(|| format!("reading the key at {url}"))?;
        new.push(New {
            entry,
            key,
            fingerprints,
        });
    }
    manifest.repos.extend(new.iter().map(|n| n.entry.clone()));

    let mut session = Session::open(ui, Refresh::IfMissing, false, Some(&manifest))?;
    let keys = host::keys_dir(&session.state);
    std::fs::create_dir_all(&keys).with_context(|| format!("creating {}", keys.display()))?;
    for n in &new {
        let id = n.entry.name.replace('/', ":");
        if let Some(s) = session.universe.skipped.iter().find(|s| s.repo == id) {
            bail!("can't add {}: {}", n.entry.name, s.error);
        }
        let path = keys.join(nx_rpm::packages::pinned_key(&id));
        std::fs::write(&path, &n.key).with_context(|| format!("writing {}", path.display()))?;
    }
    let specs = manifest.host.packages.clone();
    let resolved = session.resolve(ui, &specs)?;
    let built = session.install(ui, &manifest, &resolved.all)?;
    let st = ui.out.style;
    for n in &new {
        ui.out.line(format_args!(
            "{} {} {}",
            st.green(ui::SYM_ADDED),
            n.entry.name,
            st.dim(&format!("· key {}", n.fingerprints.join(", ")))
        ));
    }
    ui.done(format_args!(
        "{} added · generation {}",
        ui::plural(new.len(), "repo"),
        built.generation.number
    ));
    Ok(())
}

/// What `source` names: `copr:owner/project`, a `.repo` file's URL, or a
/// repo's base URL (which needs `--key`).
fn entries(
    http: &dyn Http,
    source: &str,
    key: Option<&str>,
    priority: Option<i32>,
) -> Result<Vec<RepoEntry>> {
    let entry = |name: &str| RepoEntry {
        name: name.to_owned(),
        baseurl: None,
        metalink: None,
        gpgkey: key.map(str::to_owned),
        priority,
    };
    if source.starts_with("copr:") {
        // Checked again by Repo::added; here for a clear message.
        let vars = Vars {
            releasever: "45".into(),
            basearch: "x86_64".into(),
        };
        if repo::copr(source, &vars).is_none() {
            bail!("{source} is not copr:owner/project");
        }
        return Ok(vec![entry(source)]);
    }
    if !source.starts_with("https://") {
        bail!("{source} is neither copr:owner/project nor an https:// URL");
    }
    if source.ends_with(".repo") {
        let text = String::from_utf8_lossy(&http.get(source)?).into_owned();
        // Keep $releasever and $basearch unexpanded, so the entry follows
        // the image to its next release.
        let raw = Vars {
            releasever: "$releasever".into(),
            basearch: "$basearch".into(),
        };
        let found: Vec<RepoEntry> = repo::parse(&text, Path::new(source), &raw)
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| RepoEntry {
                baseurl: r.baseurl.first().cloned(),
                metalink: r.metalink.clone(),
                gpgkey: key.map(str::to_owned).or_else(|| r.gpgkey.first().cloned()),
                ..entry(&r.id)
            })
            .collect();
        if found.is_empty() {
            bail!("{source} enables no repo");
        }
        return Ok(found);
    }
    if key.is_none() {
        bail!("a repo URL needs its key: nx repo add {source} --key <url>");
    }
    let name = source
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .replace('/', "-");
    Ok(vec![RepoEntry {
        baseurl: Some(source.to_owned()),
        ..entry(&name)
    }])
}

fn added(e: &RepoEntry) -> Added<'_> {
    Added {
        name: &e.name,
        baseurl: e.baseurl.as_deref(),
        metalink: e.metalink.as_deref(),
        gpgkey: e.gpgkey.as_deref(),
        priority: e.priority,
    }
}

pub fn remove(ui: &mut Ui, name: &str) -> Result<()> {
    let mut manifest = host::current_manifest(&host::state())?;
    if manifest.repo(name).is_none() {
        bail!("{name} was not added with nx repo add (nx repo list shows the repos)");
    }
    manifest.repos.retain(|r| r.name != name);
    let mut session = Session::open(ui, Refresh::IfMissing, false, Some(&manifest))?;
    let specs = manifest.host.packages.clone();
    let resolved = session
        .resolve(ui, &specs)
        .with_context(|| format!("packages still need {name}"))?;
    let built = session.install(ui, &manifest, &resolved.all)?;
    let id = name.replace('/', ":");
    let _ = std::fs::remove_file(
        host::keys_dir(&session.state).join(nx_rpm::packages::pinned_key(&id)),
    );
    let st = ui.out.style;
    ui.out.line(format_args!("{} {name}", st.red("-")));
    ui.done(format_args!(
        "repo removed · generation {}",
        built.generation.number
    ));
    Ok(())
}

/// One line of `nx repo list`.
struct Listed {
    name: String,
    added: bool,
    priority: i32,
    keys: Vec<String>,
}

pub fn list(ui: &mut Ui) -> Result<()> {
    let opts = host::options(Path::new("/"), &RpmOstree, Refresh::IfMissing)?;
    let state = host::state();
    let current = Generation::current(&state)?;
    let manifest = current
        .as_ref()
        .map(|g| g.manifest.clone())
        .unwrap_or_default();
    let mut rows: Vec<Listed> = repo::load(&opts.root, &opts.vars)?
        .into_iter()
        .filter(|r| r.enabled)
        .map(|r| Listed {
            name: r.id,
            added: false,
            priority: r.priority,
            keys: Vec::new(),
        })
        .collect();
    rows.extend(added_rows(&manifest, current.as_ref()));
    render(&mut ui.out, &rows);
    Ok(())
}

fn added_rows(manifest: &Manifest, current: Option<&Generation>) -> Vec<Listed> {
    manifest
        .repos
        .iter()
        .map(|r| {
            let id = r.name.replace('/', ":");
            Listed {
                name: r.name.clone(),
                added: true,
                priority: r.priority.unwrap_or(repo::ADDED_PRIORITY),
                keys: current
                    .and_then(|g| g.lock.repos.iter().find(|l| l.id == id))
                    .map(|l| l.keys.clone())
                    .unwrap_or_default(),
            }
        })
        .collect()
}

fn render(out: &mut Stream, rows: &[Listed]) {
    let style = out.style;
    let width = rows.iter().map(|r| r.name.len()).max().unwrap_or(0) + 2;
    for r in rows {
        let mut detail = format!(
            "{} · priority {}",
            if r.added { "added" } else { "image" },
            r.priority
        );
        if let Some(k) = r.keys.first() {
            detail.push_str(&format!(" · key {}", short(k)));
        }
        out.line(format_args!(
            "{}{}",
            ui::pad(&r.name, width),
            style.dim(&detail)
        ));
    }
}

/// The last 16 hex digits of a fingerprint: the long key ID.
fn short(fingerprint: &str) -> &str {
    fingerprint
        .get(fingerprint.len().saturating_sub(16)..)
        .unwrap_or(fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SharedBuf;

    struct Canned(&'static str);
    impl Http for Canned {
        fn get(&self, _url: &str) -> nx_rpm::Result<Vec<u8>> {
            Ok(self.0.as_bytes().to_vec())
        }
    }

    #[test]
    fn reads_what_a_source_names() -> Result<()> {
        let none = Canned("");
        let copr = entries(&none, "copr:atim/lazygit", None, Some(50))?;
        assert_eq!(copr[0].name, "copr:atim/lazygit");
        assert_eq!(copr[0].priority, Some(50));
        assert!(entries(&none, "copr:../x", None, None).is_err());
        assert!(entries(&none, "http://x/", Some("https://k"), None).is_err());
        assert!(
            entries(&none, "https://x/repo/", None, None).is_err(),
            "needs --key"
        );

        let file = Canned(
            "[code]\nname=VS Code\nbaseurl=https://packages.microsoft.com/yumrepos/vscode\nenabled=1\ngpgcheck=1\ngpgkey=https://packages.microsoft.com/keys/microsoft.asc\n\n[off]\nbaseurl=https://x\nenabled=0\n",
        );
        let code = entries(
            &file,
            "https://packages.microsoft.com/yumrepos/vscode.repo",
            None,
            None,
        )?;
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].name, "code");
        assert_eq!(
            code[0].gpgkey.as_deref(),
            Some("https://packages.microsoft.com/keys/microsoft.asc")
        );
        Ok(())
    }

    #[test]
    fn lists_image_and_added_repos() {
        let rows = [
            Listed {
                name: "fedora".into(),
                added: false,
                priority: 99,
                keys: vec![],
            },
            Listed {
                name: "copr:atim/lazygit".into(),
                added: true,
                priority: 100,
                keys: vec!["4f50a6114cd5c6976a7f1179655a4b02f577861e".into()],
            },
        ];
        let buf = SharedBuf::default();
        let mut ui = Ui::plain(buf.clone(), SharedBuf::default());
        render(&mut ui.out, &rows);
        assert_eq!(
            buf.text(),
            "  fedora             image · priority 99\n\
             \x20 copr:atim/lazygit  added · priority 100 · key 655a4b02f577861e\n"
        );
    }
}
