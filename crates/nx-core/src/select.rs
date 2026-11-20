//! Which generation belongs to which image, and which ones to keep.
//!
//! A generation is bound to the image digest it was resolved for (design
//! invariant 2), and every deployed digest keeps one (invariant 5). The
//! questions here need only the lockfiles (and the record of why a sync failed), so they are cheap enough to ask
//! at boot, with no network and no libsolv.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::lockfile::Lock;
use crate::state::{self, State};
use crate::{Error, Result};

/// One generation's lockfile, for the questions that don't need the rest.
pub fn read_lock(state: &State, number: u32) -> Result<Lock> {
    let path = state.generation_dir(number).join("lock.toml");
    let text = std::fs::read_to_string(&path).map_err(|source| Error::Read {
        path: path.clone(),
        source,
    })?;
    Lock::parse(&text, &path)
}

/// Every generation this nx can read with its lockfile, oldest first. One a
/// newer nx wrote is left out: it is not ours to select or delete.
pub fn locks(state: &State) -> Result<Vec<(u32, Lock)>> {
    let mut out = Vec::new();
    for number in state.generations()? {
        match read_lock(state, number) {
            Ok(lock) => out.push((number, lock)),
            Err(Error::Newer { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// The newest generation built for `digest`.
pub fn newest_for(state: &State, digest: &str) -> Result<Option<u32>> {
    Ok(locks(state)?
        .into_iter()
        .rev()
        .find(|(_, lock)| lock.image.as_deref() == Some(digest))
        .map(|(number, _)| number))
}

/// Why a deployed image has no host tier, by digest: what the last `nx sync`
/// could not resolve. `nx status` shows it; the journal has the same line.
pub fn failures(state: &State) -> BTreeMap<String, String> {
    read_record(state, SYNC_FAILURES)
}

/// Replaces the failure record. Best effort: whoever cannot write the state
/// directory cannot have run a sync that matters.
pub fn record_failures(state: &State, failures: &BTreeMap<String, String>) {
    write_record(state, SYNC_FAILURES, failures);
}

const SYNC_FAILURES: &str = "sync-failures";
const UPGRADE_FAILURE: &str = "upgrade-failure";

/// Why the last `nx upgrade` the weekly timer ran failed, if it did.
pub fn upgrade_failure(state: &State) -> Option<String> {
    read_record(state, UPGRADE_FAILURE).remove("tool")
}

/// Records (or with `None` clears) the timer's upgrade failure.
pub fn record_upgrade_failure(state: &State, failure: Option<&str>) {
    let record: BTreeMap<String, String> = failure
        .map(|f| ("tool".to_owned(), f.to_owned()))
        .into_iter()
        .collect();
    write_record(state, UPGRADE_FAILURE, &record);
}

/// A record in the state directory, `<name>.json`. A missing or unreadable
/// one is the same as an empty one.
fn read_record(state: &State, name: &str) -> BTreeMap<String, String> {
    std::fs::read_to_string(state.root().join(format!("{name}.json")))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Replaces `<name>.json`; an empty record removes it. Best effort.
fn write_record(state: &State, name: &str, record: &BTreeMap<String, String>) {
    use std::io::Write;
    let path = state.root().join(format!("{name}.json"));
    if record.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    // A fresh name each time (`create_new` never follows a planted link),
    // flushed before it gets its final one.
    let tmp = state
        .root()
        .join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let written = serde_json::to_string(record).ok().and_then(|text| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .ok()?;
        f.write_all(text.as_bytes()).ok()?;
        f.sync_all().ok()
    });
    if written.is_some() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// What `nx gc` would do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcPlan {
    pub keep: Vec<u32>,
    pub drop: Vec<u32>,
}

/// Generations to keep: the current one and its parent (where `nx
/// rollback` goes), the newest for each deployed digest (booted, staged
/// and rollback), and the `keep` newest (the manifest's `gc.keep`). Those a
/// newer nx wrote are never dropped.
pub fn gc_plan(state: &State, deployed: &[String], keep_newest: usize) -> Result<GcPlan> {
    let all = locks(state)?;
    let mut keep: BTreeSet<u32> = state.generations()?.into_iter().collect();
    let readable: HashSet<u32> = all.iter().map(|(n, _)| *n).collect();
    keep.retain(|n| !readable.contains(n));
    if let Some(current) = state.current_number()? {
        keep.insert(current);
        keep.extend(
            all.iter()
                .find(|(n, _)| *n == current)
                .and_then(|(_, lock)| lock.parent),
        );
    }
    for digest in deployed {
        keep.extend(
            all.iter()
                .rev()
                .find(|(_, lock)| lock.image.as_deref() == Some(digest))
                .map(|(n, _)| *n),
        );
    }
    keep.extend(all.iter().rev().take(keep_newest).map(|(n, _)| *n));
    let drop = all
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !keep.contains(n))
        .collect();
    Ok(GcPlan {
        keep: keep.into_iter().collect(),
        drop,
    })
}

/// The store entries no kept generation names, with their sizes' directory
/// names (SHA-256s).
pub fn unreferenced(state: &State, keep: &[u32]) -> Result<Vec<String>> {
    let mut used: HashSet<String> = HashSet::new();
    for n in keep {
        match read_lock(state, *n) {
            Ok(lock) => used.extend(lock.host.into_iter().map(|p| p.sha256)),
            // A generation a newer nx wrote may need anything: keep it all.
            Err(Error::Newer { .. }) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        }
    }
    Ok(state
        .store()
        .entries()?
        .into_iter()
        .filter(|sha| !used.contains(sha))
        .collect())
}

/// Removes generation `number`'s directory. Needs the state lock.
pub fn remove_generation(state: &State, _lock: &state::Lock, number: u32) -> Result<()> {
    if state.current_number()? == Some(number) {
        return Err(Error::Invalid(format!(
            "generation {number} is current and cannot be removed"
        )));
    }
    state::discard(&state.generations_dir().join(number.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::TempDir;

    fn generation(state: &State, n: u32, image: Option<&str>, packages: &[&str]) -> Result<()> {
        let mut lock = Lock::new(image.map(str::to_owned), "45", "x86_64");
        for sha in packages {
            lock.host.push(crate::LockedPackage {
                name: (*sha).to_owned(),
                nevra: format!("{sha}-1-1.x86_64"),
                repo: "fedora".into(),
                sha256: (*sha).to_owned(),
                requested: vec![],
            });
        }
        let dir = state.generations_dir().join(n.to_string());
        let write = |source| Error::Write {
            path: dir.clone(),
            source,
        };
        std::fs::create_dir_all(&dir).map_err(write)?;
        std::fs::write(dir.join("lock.toml"), lock.to_toml()?).map_err(write)
    }

    #[test]
    fn failures_are_recorded_and_cleared() {
        let dir = TempDir::new("failures");
        let state = State::new(&dir.0);
        assert!(failures(&state).is_empty());
        let one = BTreeMap::from([("sha256:a".to_owned(), "no such package".to_owned())]);
        record_failures(&state, &one);
        assert_eq!(failures(&state), one);
        record_failures(&state, &BTreeMap::new());
        assert!(failures(&state).is_empty());
        record_upgrade_failure(&state, Some("no network"));
        assert_eq!(upgrade_failure(&state).as_deref(), Some("no network"));
        assert!(failures(&state).is_empty());
        record_upgrade_failure(&state, None);
        assert_eq!(upgrade_failure(&state), None);
    }

    #[test]
    fn finds_the_newest_generation_for_a_digest() -> Result<()> {
        let tmp = TempDir::new("select-newest");
        let state = State::new(&tmp.0);
        generation(&state, 1, Some("sha256:a"), &[])?;
        generation(&state, 2, Some("sha256:b"), &[])?;
        generation(&state, 3, Some("sha256:a"), &[])?;
        assert_eq!(newest_for(&state, "sha256:a")?, Some(3));
        assert_eq!(newest_for(&state, "sha256:b")?, Some(2));
        assert_eq!(newest_for(&state, "sha256:c")?, None);
        Ok(())
    }

    #[test]
    fn gc_keeps_what_deployed_images_and_rollback_need() -> Result<()> {
        let tmp = TempDir::new("select-gc");
        let state = State::new(&tmp.0);
        // Generation 1 is the only one for the rollback image; 2..=9 are for
        // the booted one.
        generation(&state, 1, Some("sha256:old"), &[])?;
        for n in 2..=9 {
            generation(&state, n, Some("sha256:new"), &[])?;
        }
        state.set_current(9)?;
        let plan = gc_plan(&state, &["sha256:new".into(), "sha256:old".into()], 5)?;
        assert_eq!(plan.keep, vec![1, 5, 6, 7, 8, 9]);
        assert_eq!(plan.drop, vec![2, 3, 4]);
        // With the old image no longer deployed, 1 goes too.
        let plan = gc_plan(&state, &["sha256:new".into()], 5)?;
        assert_eq!(plan.drop, vec![1, 2, 3, 4]);
        Ok(())
    }

    #[test]
    fn gc_never_drops_current() -> Result<()> {
        let tmp = TempDir::new("select-current");
        let state = State::new(&tmp.0);
        for n in 1..=8 {
            generation(&state, n, Some("sha256:x"), &[])?;
        }
        state.set_current(1)?;
        let plan = gc_plan(&state, &[], 5)?;
        assert!(plan.keep.contains(&1));
        assert!(!plan.drop.contains(&1));
        Ok(())
    }

    #[test]
    fn gc_keeps_where_rollback_goes() -> Result<()> {
        let tmp = TempDir::new("select-parent");
        let state = State::new(&tmp.0);
        for n in 1..=3 {
            generation(&state, n, Some("sha256:x"), &[])?;
        }
        let path = state.generations_dir().join("3").join("lock.toml");
        let mut lock = read_lock(&state, 3)?;
        lock.parent = Some(1);
        std::fs::write(&path, lock.to_toml()?).map_err(|source| Error::Write { path, source })?;
        state.set_current(3)?;
        let plan = gc_plan(&state, &["sha256:x".into()], 0)?;
        assert_eq!(plan.keep, vec![1, 3]);
        assert_eq!(plan.drop, vec![2]);
        Ok(())
    }

    #[test]
    fn the_store_keeps_what_kept_generations_use() -> Result<()> {
        let tmp = TempDir::new("select-store");
        let state = State::new(&tmp.0);
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        generation(&state, 1, Some("sha256:x"), &[&a, &b])?;
        generation(&state, 2, Some("sha256:x"), &[&b, &c])?;
        for sha in [&a, &b, &c] {
            std::fs::create_dir_all(state.store().path(sha)).map_err(|source| Error::Write {
                path: tmp.0.clone(),
                source,
            })?;
        }
        assert_eq!(unreferenced(&state, &[2])?, vec![a.clone()]);
        assert!(unreferenced(&state, &[1, 2])?.is_empty());
        Ok(())
    }
}
