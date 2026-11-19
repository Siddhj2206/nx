//! What realising store paths will cost, from the binary caches' `.narinfo`
//! files, many at once (ported from nx 0.x's `cache`). It only reads: the
//! daemon downloads and checks signatures and hashes itself.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Condvar, Mutex};

use crate::web::Web;
use crate::{Result, STORE, store_hash};

/// Requests in flight at once.
const PARALLEL: usize = 16;

/// One store path as a cache describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    /// Compressed download size.
    pub file_size: u64,
    /// Unpacked size.
    pub nar_size: u64,
    pub references: Vec<String>,
    /// Already in the local store: nothing to fetch, and its closure is
    /// there too.
    pub local: bool,
    /// No cache has it, so Nix would build it.
    pub missing: bool,
}

/// Reads a `.narinfo`.
pub fn parse(text: &str) -> Info {
    let mut info = Info::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        match key {
            "FileSize" => info.file_size = value.trim().parse().unwrap_or(0),
            "NarSize" => info.nar_size = value.trim().parse().unwrap_or(0),
            "References" => {
                info.references = value
                    .split_whitespace()
                    .map(|r| format!("{STORE}{r}"))
                    .collect();
            }
            _ => {}
        }
    }
    info
}

/// Where narinfos come from: `caches` in order, the first that has one
/// answers.
pub trait Lookup: Sync {
    fn narinfo(&self, hash: &str) -> Result<Option<String>>;
}

/// The caches over HTTP.
pub struct Caches<'a> {
    pub web: &'a Web,
    pub urls: Vec<String>,
}

impl Lookup for Caches<'_> {
    fn narinfo(&self, hash: &str) -> Result<Option<String>> {
        for base in &self.urls {
            if let Some((text, _)) = self
                .web
                .text(&format!("{}/{hash}.narinfo", base.trim_end_matches('/')))?
            {
                return Ok(Some(text));
            }
        }
        Ok(None)
    }
}

struct Walk {
    queue: VecDeque<String>,
    seen: HashSet<String>,
    busy: usize,
    found: BTreeMap<String, Info>,
    error: Option<crate::Error>,
}

/// Walks the references from `roots` and returns every path in their
/// closure that isn't local yet, plus the roots themselves. A local path
/// isn't looked up: its closure is local too.
pub fn closure(
    lookup: &dyn Lookup,
    roots: &[String],
    is_local: &(dyn Fn(&str) -> bool + Sync),
) -> Result<BTreeMap<String, Info>> {
    let walk = Mutex::new(Walk {
        queue: roots.iter().cloned().collect(),
        seen: roots.iter().cloned().collect(),
        busy: 0,
        found: BTreeMap::new(),
        error: None,
    });
    let ready = Condvar::new();
    std::thread::scope(|s| {
        for _ in 0..PARALLEL {
            s.spawn(|| worker(lookup, is_local, &walk, &ready));
        }
    });
    let walk = walk
        .into_inner()
        .map_err(|_| crate::Error::Invalid("a narinfo worker panicked".into()))?;
    match walk.error {
        Some(e) => Err(e),
        None => Ok(walk.found),
    }
}

fn worker(
    lookup: &dyn Lookup,
    is_local: &(dyn Fn(&str) -> bool + Sync),
    walk: &Mutex<Walk>,
    ready: &Condvar,
) {
    loop {
        let path = {
            let Ok(mut w) = walk.lock() else { return };
            loop {
                if w.error.is_some() {
                    ready.notify_all();
                    return;
                }
                if let Some(p) = w.queue.pop_front() {
                    w.busy += 1;
                    break p;
                }
                if w.busy == 0 {
                    ready.notify_all();
                    return;
                }
                let Ok(next) = ready.wait(w) else { return };
                w = next;
            }
        };
        let info = if is_local(&path) {
            Ok(Info {
                local: true,
                ..Info::default()
            })
        } else {
            match store_hash(&path) {
                Some(hash) => lookup.narinfo(hash).map(|t| match t {
                    Some(text) => parse(&text),
                    None => Info {
                        missing: true,
                        ..Info::default()
                    },
                }),
                None => Err(crate::Error::Invalid(format!("not a store path: {path}"))),
            }
        };
        let Ok(mut w) = walk.lock() else { return };
        w.busy -= 1;
        match info {
            Ok(info) => {
                for r in &info.references {
                    if w.seen.insert(r.clone()) {
                        w.queue.push_back(r.clone());
                    }
                }
                w.found.insert(path, info);
            }
            Err(e) => {
                w.error.get_or_insert(e);
            }
        }
        ready.notify_all();
    }
}

/// A closure's totals: what isn't local yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sum {
    pub paths: usize,
    pub download: u64,
    pub unpacked: u64,
    /// Paths no cache has: Nix builds them.
    pub unbuilt: usize,
}

pub fn total(closure: &BTreeMap<String, Info>) -> Sum {
    let mut s = Sum::default();
    for i in closure.values().filter(|i| !i.local) {
        s.paths += 1;
        s.download += i.file_size;
        s.unpacked += i.nar_size;
        s.unbuilt += usize::from(i.missing);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const RG: &str = "/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0";
    const PCRE: &str = "/nix/store/m9y00qgmfbzvlffgbx0hi5hc38dps46q-pcre2-10.49";
    const GLIBC: &str = "/nix/store/r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57";

    struct Fixed;
    impl Lookup for Fixed {
        fn narinfo(&self, hash: &str) -> Result<Option<String>> {
            Ok(match hash {
                "whbrk657qmih065xal60m4khwf6cb9pj" => Some(format!(
                    "StorePath: {RG}\nFileSize: 2070203\nNarSize: 7341664\nReferences: m9y00qgmfbzvlffgbx0hi5hc38dps46q-pcre2-10.49 r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57\n"
                )),
                "m9y00qgmfbzvlffgbx0hi5hc38dps46q" => Some(
                    "FileSize: 100\nNarSize: 400\nReferences: r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57 m9y00qgmfbzvlffgbx0hi5hc38dps46q-pcre2-10.49\n".into(),
                ),
                _ => None,
            })
        }
    }

    #[test]
    fn walks_what_is_not_local() -> Result<()> {
        let c = closure(&Fixed, &[RG.to_owned()], &|p| p == GLIBC)?;
        assert_eq!(c.len(), 3);
        assert!(c[GLIBC].local);
        assert_eq!(c[PCRE].nar_size, 400);
        assert_eq!(
            total(&c),
            Sum {
                paths: 2,
                download: 2_070_303,
                unpacked: 7_342_064,
                unbuilt: 0
            }
        );
        Ok(())
    }

    #[test]
    fn a_path_no_cache_has_is_built() -> Result<()> {
        let fake = "/nix/store/00000000000000000000000000000000-nothing-1.0";
        let c = closure(&Fixed, &[fake.to_owned()], &|_| false)?;
        assert!(c[fake].missing);
        assert_eq!(total(&c).unbuilt, 1);
        Ok(())
    }
}
