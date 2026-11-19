//! `nx repo add|list|remove`: third-party RPM repos, recorded in the
//! manifest with their key pinned. Adding or removing one builds a new
//! generation, as any manifest change does.

use std::path::Path;

use anyhow::{Context, Result, bail};
use nx_core::{Generation, Manifest, RepoEntry};
use nx_rpm::repo::{self, Repo, Vars, id_of};
use nx_rpm::{Http, Refresh, Ureq};

use crate::apply::{How, Session};
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
    let fetched = entries(&http, &vars, source, key, priority)?
        .into_iter()
        .map(|entry| fetch(ui, &http, &vars, &image_ids, entry))
        .collect::<Result<Vec<_>>>()?;

    let mut session = Session::open(ui, machine, Refresh::IfMissing, false, |m| {
        for f in &fetched {
            let name = &f.entry.name;
            if let Some(r) = same_id(m, name) {
                bail!(
                    "repo {name} would share its id with {}; nx repo remove {} first",
                    r.name,
                    r.name
                );
            }
            if let Some(r) = m.repo(name)
                && (r.baseurl != f.entry.baseurl || r.metalink != f.entry.metalink)
            {
                bail!(
                    "repo {name} is already added, from {}; nx repo remove {name} first",
                    r.baseurl
                        .as_deref()
                        .or(r.metalink.as_deref())
                        .unwrap_or(name)
                );
            }
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
        if let Some(s) = session.universe().skipped.iter().find(|s| s.repo == *id) {
            bail!("can't add {}: {}", f.entry.name, s.error);
        }
    }
    // Under the lock, so nx gc can't take a pin before its generation names
    // it; one left by a change that fails goes with the next gc.
    pin_all(&mut session, &fetched)?;
    let outcome = session.plan(ui, &How::default())?;
    let built = session.install(ui, &outcome.all)?;
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

/// `entry`'s key, from its https URL, shown before anything changes so the
/// user sees what they trust.
fn fetch(
    ui: &mut Ui,
    http: &dyn Http,
    vars: &Vars,
    image_ids: &[String],
    entry: RepoEntry,
) -> Result<Fetched> {
    let repo = Repo::added(&host::added(&entry), vars)?;
    if image_ids.contains(&repo.id) {
        bail!("{} is one of the image's repos", entry.name);
    }
    let url = repo
        .gpgkey
        .first()
        .filter(|k| nx_core::Https::parse(k).is_some())
        .with_context(|| format!("{} needs a key served over https (--key <url>)", entry.name))?;
    let key = String::from_utf8_lossy(&http.get(url)?).into_owned();
    let fingerprints =
        nx_librpm::fingerprints(&key).with_context(|| format!("reading the key at {url}"))?;
    ui.step(format_args!(
        "{} is signed with {}",
        entry.name,
        fingerprints.join(", ")
    ));
    Ok(Fetched {
        entry,
        key,
        fingerprints,
    })
}

/// Pins each fetched key and has the session check its repo against it.
fn pin_all(session: &mut Session, fetched: &[Fetched]) -> Result<()> {
    let dir = crate::keys::dir(session.state());
    for f in fetched {
        let sha = crate::keys::pin(&dir, &f.key)?;
        session.choose_key(&id_of(&f.entry.name), sha);
    }
    Ok(())
}

/// The keys of `wanted`'s repos that the current generation has none
/// pinned for (`nx apply` on a new machine), fetched and shown as `nx repo
/// add` does. Refuses two repos with one id, and a repo the image has.
pub fn fetch_unpinned(
    ui: &mut Ui,
    machine: &Machine,
    wanted: &Manifest,
    current: Option<&Generation>,
) -> Result<Unpinned> {
    for (i, r) in wanted.repos.iter().enumerate() {
        if let Some(other) = wanted.repos[..i]
            .iter()
            .find(|o| id_of(&o.name) == id_of(&r.name))
        {
            bail!("repos {} and {} would share one id", other.name, r.name);
        }
    }
    let vars = machine.vars();
    let image_ids: Vec<String> = repo::load(&machine.root, &vars)?
        .into_iter()
        .map(|r| r.id)
        .collect();
    let dir = crate::keys::dir(&host::state());
    let pinned = |id: &str| {
        current
            .and_then(|g| g.lock.repo(id))
            .is_some_and(|r| r.key.is_some())
            || dir.join(nx_rpm::packages::legacy_pinned_key(id)).is_file()
    };
    let http = Ureq::new();
    let fetched = wanted
        .repos
        .iter()
        .filter(|r| !pinned(&id_of(&r.name)))
        .map(|r| fetch(ui, &http, &vars, &image_ids, r.clone()))
        .collect::<Result<Vec<_>>>()?;
    Ok(Unpinned(fetched))
}

/// Keys fetched for repos that had none pinned.
pub struct Unpinned(Vec<Fetched>);

impl Unpinned {
    /// Pins them for `session`'s resolve. Take the lock first.
    pub fn pin(&self, session: &mut Session) -> Result<()> {
        pin_all(session, &self.0)
    }
}

/// Another added repo with `name`'s id: `copr:a/b` and `copr:a:b` would
/// share the pool's repo, the cache and the pinned key.
fn same_id<'a>(m: &'a Manifest, name: &str) -> Option<&'a RepoEntry> {
    let id = id_of(name);
    m.repos
        .iter()
        .find(|r| r.name != name && id_of(&r.name) == id)
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
    let Some(url) = nx_core::Https::parse(source) else {
        bail!("{source} is neither copr:owner/project nor an https:// URL");
    };
    if url.path.ends_with(".repo") {
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
    let name = url.repo_id();
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
    let outcome = session
        .plan(ui, &How::default())
        .with_context(|| format!("packages still need {name}"))?;
    // Its pinned key stays while an earlier generation names it (a
    // rollback needs it); nx gc removes it after.
    let built = session.install(ui, &outcome.all)?;
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
        let token = entries(&file, &vars, "https://x.org/a.repo?token=1", None, None)?;
        assert_eq!(token[0].name, "code", "a .repo URL with a query");
        let plain = entries(&none, &vars, "https://x.org/b/c/", Some("https://k"), None)?;
        assert_eq!(plain[0].name, "x.org_b_c");
        assert_eq!(
            code[0].gpgkey.as_deref(),
            Some("https://packages.microsoft.com/keys/microsoft.asc")
        );
        Ok(())
    }

    #[test]
    fn two_names_for_one_id_are_refused() {
        let entry = |name: &str| RepoEntry {
            name: name.into(),
            baseurl: None,
            metalink: None,
            gpgkey: None,
            priority: None,
        };
        let m = Manifest {
            repos: vec![entry("copr:a/b")],
            ..Manifest::default()
        };
        assert_eq!(
            same_id(&m, "copr:a:b").map(|r| r.name.as_str()),
            Some("copr:a/b")
        );
        assert!(same_id(&m, "copr:a/b").is_none(), "the same repo again");
        assert!(same_id(&m, "copr:a/c").is_none());
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
