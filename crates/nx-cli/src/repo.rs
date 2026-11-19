//! `nx repo add|list|remove`: third-party RPM repos, recorded in the
//! manifest with their key pinned. Adding or removing one builds a new
//! generation, as any manifest change does.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nx_core::{Generation, Manifest, RepoEntry};
use nx_rpm::repo::{self, Repo, Vars, id_of};
use nx_rpm::{Http, Refresh, Ureq};

use crate::apply::Session;
use crate::host::{self, Machine};
use crate::ui::{self, Stream, Ui};

/// A repo the user named: its manifest entry and its key.
struct Fetched {
    entry: RepoEntry,
    key: String,
    fingerprints: Vec<String>,
}

/// `nx repo add`. With `new_key`, replaces the pinned key of a repo added
/// before, once the user has checked the new fingerprint.
pub fn add(
    ui: &mut Ui,
    machine: &Machine,
    source: &str,
    key: Option<&str>,
    priority: Option<i32>,
    new_key: bool,
) -> Result<()> {
    let vars = machine.vars();
    let image_ids: Vec<String> = repo::load(&machine.root, &vars)?
        .into_iter()
        .map(|r| r.id)
        .collect();
    let http = Ureq::new();
    let mut fetched = Vec::new();
    for entry in entries(&http, &vars, source, key, priority)? {
        let repo = Repo::added(&host::added(&entry), &vars)?;
        if image_ids.contains(&repo.id) {
            bail!("{} is one of the image's repos", entry.name);
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
        // Shown before anything changes, so the user sees what they trust.
        ui.step(format_args!(
            "{} is signed with {}",
            entry.name,
            fingerprints.join(", ")
        ));
        fetched.push(Fetched {
            entry,
            key,
            fingerprints,
        });
    }

    let mut session = Session::open(ui, machine, Refresh::IfMissing, false, |m| {
        for f in &fetched {
            let name = &f.entry.name;
            match (m.repo(name).is_some(), new_key) {
                (true, false) => {
                    bail!("repo {name} is already added (--new-key replaces its key)")
                }
                (false, true) => bail!("repo {name} is not added yet; drop --new-key"),
                (false, false) => m.repos.push(f.entry.clone()),
                (true, true) => {}
            }
        }
        Ok(())
    })?;
    let ids: Vec<String> = fetched.iter().map(|f| id_of(&f.entry.name)).collect();
    if new_key {
        session.trust(&ids);
    }
    for (f, id) in fetched.iter().zip(&ids) {
        if let Some(s) = session.universe.skipped.iter().find(|s| s.repo == *id) {
            bail!("can't add {}: {}", f.entry.name, s.error);
        }
    }
    let pins = Pins::write(&host::keys_dir(&session.state), &fetched)?;
    let built = (|| {
        let manifest = session.manifest.clone();
        let resolved = session.resolve_manifest(ui, &manifest.host.packages, &[])?;
        session.install(ui, &manifest, &resolved.all)
    })();
    let built = match built {
        Ok(b) => b,
        Err(e) => {
            pins.undo();
            return Err(e);
        }
    };
    let st = ui.out.style;
    for f in &fetched {
        ui.out.line(format_args!(
            "{} {} {}",
            st.green(ui::SYM_ADDED),
            f.entry.name,
            st.dim(&format!("· key {}", f.fingerprints.join(", ")))
        ));
    }
    let what = if new_key {
        format!("{} replaced", ui::plural(fetched.len(), "key"))
    } else {
        format!("{} added", ui::plural(fetched.len(), "repo"))
    };
    ui.done(format_args!(
        "{what} · generation {}",
        built.generation.number
    ));
    Ok(())
}

/// Key files written for a change, and what they replaced, so a change
/// that fails leaves the keys as they were.
struct Pins(Vec<(PathBuf, Option<String>)>);

impl Pins {
    fn write(dir: &Path, fetched: &[Fetched]) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut pins = Self(Vec::new());
        for f in fetched {
            let path = dir.join(nx_rpm::packages::pinned_key(&id_of(&f.entry.name)));
            let before = std::fs::read_to_string(&path).ok();
            if let Err(e) = std::fs::write(&path, &f.key) {
                pins.undo();
                return Err(e).with_context(|| format!("writing {}", path.display()));
            }
            pins.0.push((path, before));
        }
        Ok(pins)
    }

    fn undo(self) {
        for (path, before) in self.0 {
            let _ = match before {
                Some(text) => std::fs::write(&path, text),
                None => std::fs::remove_file(&path),
            };
        }
    }
}

/// What `source` names: `copr:owner/project`, a `.repo` file's URL, or a
/// repo's base URL (which needs `--key`).
fn entries(
    http: &dyn Http,
    vars: &Vars,
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
        if repo::copr(source, vars).is_none() {
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

pub fn remove(ui: &mut Ui, machine: &Machine, name: &str) -> Result<()> {
    let mut session = Session::open(ui, machine, Refresh::IfMissing, false, |m| {
        if m.repo(name).is_none() {
            bail!("{name} was not added with nx repo add (nx repo list shows the repos)");
        }
        m.repos.retain(|r| r.name != name);
        Ok(())
    })?;
    let manifest = session.manifest.clone();
    let resolved = session
        .resolve_manifest(ui, &manifest.host.packages, &[])
        .with_context(|| format!("packages still need {name}"))?;
    let built = session.install(ui, &manifest, &resolved.all)?;
    let _ = std::fs::remove_file(
        host::keys_dir(&session.state).join(nx_rpm::packages::pinned_key(&id_of(name))),
    );
    let st = ui.out.style;
    ui.out
        .line(format_args!("{} {name}", st.red(ui::SYM_REMOVED)));
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

pub fn list(ui: &mut Ui, machine: &Machine) -> Result<()> {
    let current = host::readable(ui, &host::state())?;
    let manifest = current
        .as_ref()
        .map(|g| g.manifest.clone())
        .unwrap_or_default();
    let mut rows: Vec<Listed> = repo::load(&machine.root, &machine.vars())?
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
            let id = id_of(&r.name);
            Listed {
                name: r.name.clone(),
                added: true,
                priority: r.priority.unwrap_or(repo::ADDED_PRIORITY),
                keys: current
                    .and_then(|g| g.lock.repo(&id))
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
        let vars = Vars {
            releasever: "45".into(),
            basearch: "x86_64".into(),
        };
        let copr = entries(&none, &vars, "copr:atim/lazygit", None, Some(50))?;
        assert_eq!(copr[0].name, "copr:atim/lazygit");
        assert_eq!(copr[0].priority, Some(50));
        assert!(entries(&none, &vars, "copr:../x", None, None).is_err());
        assert!(entries(&none, &vars, "http://x/", Some("https://k"), None).is_err());
        assert!(
            entries(&none, &vars, "https://x/repo/", None, None).is_err(),
            "needs --key"
        );

        let file = Canned(
            "[code]\nname=VS Code\nbaseurl=https://packages.microsoft.com/yumrepos/vscode\nenabled=1\ngpgcheck=1\ngpgkey=https://packages.microsoft.com/keys/microsoft.asc\n\n[off]\nbaseurl=https://x\nenabled=0\n",
        );
        let code = entries(
            &file,
            &vars,
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
