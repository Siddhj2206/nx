//! Every package of one nixpkgs release, from the channel's
//! `packages.json.br` (ADR-0002, ported from nx 0.x): attribute, pname,
//! version, description and the meta nx routes by. Built once per release
//! into a flat file (about 20 MB) that loads and searches in milliseconds,
//! so search, info and routing never evaluate Nix.

use std::fmt;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};

use crate::channel::Release;
use crate::web::Web;
use crate::{Error, Result};

/// Changes when the file's layout does; a file in another layout is rebuilt.
const FORMAT: &str = "nx-catalog 1";
const SEP: char = '\x1f';

/// One catalog entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Package {
    /// The attribute path, what the tool tier installs.
    pub attr: String,
    pub pname: String,
    pub version: String,
    pub description: String,
    pub homepage: String,
    pub license: String,
    pub main_program: String,
    /// `meta.outputsToInstall`; empty means `out`.
    pub outputs: Vec<String>,
    pub unfree: bool,
    pub broken: bool,
    pub insecure: bool,
    /// `meta.available` was false for another reason than the flags above
    /// (which make it false too: the list is made without allowing them):
    /// not for this platform.
    pub unavailable: bool,
}

impl Package {
    /// Inside a package set (`python3Packages.black`), not top level.
    pub fn nested(&self) -> bool {
        self.attr.contains('.')
    }
}

/// A loaded catalog.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub rev: String,
    /// The release's name.
    pub release: String,
    /// Sorted by attribute.
    pub packages: Vec<Package>,
}

/// Where the catalog for `release` is kept under `cache`.
pub fn path(cache: &Path, release: &Release) -> PathBuf {
    cache.join(format!("catalog-{}", release.rev))
}

impl Catalog {
    /// The catalog for `release` from `cache`, downloading and building it
    /// first when it isn't there (about 10 MB, once per release).
    /// `on_build` is called before a download.
    pub fn ensure(
        web: &Web,
        cache: &Path,
        release: &Release,
        on_build: &mut dyn FnMut(),
    ) -> Result<Self> {
        let file = path(cache, release);
        match Self::load(&file) {
            Ok(c) if c.rev == release.rev => return Ok(c),
            _ => {}
        }
        on_build();
        std::fs::create_dir_all(cache).map_err(|source| Error::Io {
            path: cache.to_owned(),
            source,
        })?;
        let tmp = cache.join(format!(".catalog-{}.{}", release.rev, std::process::id()));
        let built = (|| -> Result<()> {
            let out = std::fs::File::create(&tmp).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
            let body = web.reader(&release.packages_json())?;
            let json = brotli_decompressor::Decompressor::new(body, 1 << 16);
            build(json, out, release)?;
            std::fs::rename(&tmp, &file).map_err(|source| Error::Io {
                path: file.clone(),
                source,
            })
        })();
        if built.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        built?;
        Self::load(&file)
    }

    /// Reads a catalog file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self> {
        let mut lines = text.split('\n');
        let head: Vec<&str> = lines.next().unwrap_or_default().split(SEP).collect();
        if head.len() != 3 || head[0] != FORMAT {
            return Err(Error::Catalog("written by another nx version".into()));
        }
        let mut packages = Vec::with_capacity(text.len() / 120);
        for line in lines.filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split(SEP).collect();
            let [
                attr,
                pname,
                version,
                description,
                homepage,
                license,
                main,
                outputs,
                flags,
            ] = f[..]
            else {
                return Err(Error::Catalog(format!("short record {line:?}")));
            };
            packages.push(Package {
                attr: attr.to_owned(),
                pname: pname.to_owned(),
                version: version.to_owned(),
                description: description.to_owned(),
                homepage: homepage.to_owned(),
                license: license.to_owned(),
                main_program: main.to_owned(),
                outputs: outputs
                    .split(',')
                    .filter(|o| !o.is_empty())
                    .map(str::to_owned)
                    .collect(),
                unfree: flags.contains('u'),
                broken: flags.contains('b'),
                insecure: flags.contains('i'),
                // Hydra's evaluation allows nothing unfree, broken or
                // insecure, so those are all unavailable to it.
                unavailable: flags.contains('x') && !flags.contains(['u', 'b', 'i']),
            });
        }
        if !packages.is_sorted_by(|a, b| a.attr <= b.attr) {
            packages.sort_by(|a, b| a.attr.cmp(&b.attr));
        }
        Ok(Self {
            rev: head[1].to_owned(),
            release: head[2].to_owned(),
            packages,
        })
    }

    /// The package at attribute `attr`.
    pub fn get(&self, attr: &str) -> Option<&Package> {
        self.packages
            .binary_search_by(|p| p.attr.as_str().cmp(attr))
            .ok()
            .and_then(|i| self.packages.get(i))
    }

    /// What `nx add <name>` means in nixpkgs: the attribute itself, else a
    /// top-level package of that pname, else one whose main program it is
    /// (`rg` → `ripgrep`). Shortest attribute first among equals.
    pub fn find(&self, name: &str) -> Option<&Package> {
        if let Some(p) = self.get(name) {
            return Some(p);
        }
        let top = || self.packages.iter().filter(|p| !p.nested());
        let shortest = |it: &mut dyn Iterator<Item = &'_ Package>| -> Option<usize> {
            it.map(|p| p.attr.len()).min()
        };
        for by_pname in [true, false] {
            let matches = |p: &&Package| {
                if by_pname {
                    p.pname == name
                } else {
                    p.main_program == name
                }
            };
            if let Some(len) = shortest(&mut top().filter(matches)) {
                return top().filter(matches).find(|p| p.attr.len() == len);
            }
        }
        None
    }

    /// Packages whose attribute, name or description holds every term.
    /// Name matches first: exact, then prefix, then anywhere; top-level
    /// attributes before those in package sets.
    pub fn search(&self, terms: &[String]) -> Vec<&Package> {
        if terms.is_empty() {
            return Vec::new();
        }
        let lower: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        let whole = lower.join(" ");
        let mut out: Vec<(u32, &Package)> = self
            .packages
            .iter()
            .filter_map(|p| rank(p, &lower, &whole).map(|r| (r, p)))
            .collect();
        out.sort_by(|(ra, a), (rb, b)| {
            ra.cmp(rb)
                .then_with(|| {
                    if *ra <= 2 {
                        a.attr.len().cmp(&b.attr.len())
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| a.attr.cmp(&b.attr))
        });
        out.into_iter().map(|(_, p)| p).collect()
    }

    /// Top-level attributes within two edits of `name`, closest first, for
    /// "did you mean".
    pub fn similar(&self, name: &str, n: usize) -> Vec<String> {
        let name = name.to_lowercase();
        let mut cands: Vec<(usize, &str)> = self
            .packages
            .iter()
            .filter(|p| !p.nested() && p.attr.len().abs_diff(name.len()) <= 2)
            .filter_map(|p| {
                let d = distance(&name, &p.attr.to_lowercase(), 2);
                (d <= 2).then_some((d, p.attr.as_str()))
            })
            .collect();
        cands.sort_by_key(|(d, _)| *d);
        cands
            .into_iter()
            .take(n)
            .map(|(_, a)| a.to_owned())
            .collect()
    }
}

fn rank(p: &Package, terms: &[String], whole: &str) -> Option<u32> {
    let attr = p.attr.to_lowercase();
    let name = p.pname.to_lowercase();
    let leaf = attr.rsplit('.').next().unwrap_or(&attr).to_owned();
    let nested = if leaf == attr { 0 } else { 10 };
    if attr == whole || (name == whole && nested == 0) {
        return Some(0);
    }
    if leaf == whole || name == whole {
        return Some(1 + nested);
    }
    let (mut in_name, mut in_desc) = (true, true);
    let mut desc: Option<String> = None;
    for t in terms {
        if attr.contains(t.as_str()) || name.contains(t.as_str()) {
            continue;
        }
        in_name = false;
        let d = desc.get_or_insert_with(|| p.description.to_lowercase());
        if !d.contains(t.as_str()) {
            in_desc = false;
            break;
        }
    }
    if in_name
        && (leaf.starts_with(whole)
            || name.starts_with(whole)
            || (terms.len() > 1 && (terms.contains(&leaf) || terms.contains(&name))))
    {
        return Some(2 + nested);
    }
    if in_name {
        return Some(3 + nested);
    }
    in_desc.then_some(4 + nested)
}

/// Optimal string alignment distance, giving up past `limit`.
fn distance(a: &str, b: &str, limit: usize) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev2 = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut best = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
            best = best.min(cur[j]);
        }
        if best > limit {
            return limit + 1;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// --- Building the file from packages.json -------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct Raw {
    pname: String,
    version: String,
    meta: Meta,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Meta {
    description: String,
    homepage: serde_json::Value,
    license: serde_json::Value,
    main_program: String,
    outputs_to_install: Vec<String>,
    unfree: bool,
    broken: bool,
    insecure: bool,
    available: Option<bool>,
}

/// Reads `packages.json` (decompressed) from `json` and writes the catalog
/// file to `out`, one entry at a time: the JSON is 400 MB and is never held
/// whole. Returns how many packages it wrote.
pub fn build(json: impl Read, out: impl Write, release: &Release) -> Result<usize> {
    let mut w = BufWriter::with_capacity(1 << 20, out);
    let io = |e: std::io::Error| Error::Catalog(e.to_string());
    writeln!(w, "{FORMAT}{SEP}{}{SEP}{}", release.rev, release.name).map_err(io)?;
    let mut records: Vec<String> = Vec::new();
    let mut de = serde_json::Deserializer::from_reader(BufReader::with_capacity(1 << 20, json));
    Top(&mut |attr, raw| records.push(record(&attr, &raw)))
        .deserialize(&mut de)
        .map_err(|e| Error::Catalog(e.to_string()))?;
    if records.is_empty() {
        return Err(Error::Catalog("no packages in it".into()));
    }
    records.sort();
    for r in &records {
        w.write_all(r.as_bytes()).map_err(io)?;
    }
    w.flush().map_err(io)?;
    Ok(records.len())
}

fn record(attr: &str, p: &Raw) -> String {
    let mut flags = String::new();
    for (on, c) in [
        (p.meta.unfree, 'u'),
        (p.meta.broken, 'b'),
        (p.meta.insecure, 'i'),
        (p.meta.available == Some(false), 'x'),
    ] {
        if on {
            flags.push(c);
        }
    }
    let outputs = if p.meta.outputs_to_install == ["out"] {
        String::new()
    } else {
        p.meta.outputs_to_install.join(",")
    };
    let fields = [
        attr,
        &p.pname,
        &p.version,
        p.meta.description.trim(),
        &homepage(&p.meta.homepage),
        &license(&p.meta.license),
        &p.meta.main_program,
        &outputs,
        &flags,
    ];
    let mut line = fields.map(clean).join(&SEP.to_string());
    line.push('\n');
    line
}

fn clean(s: &str) -> String {
    if !s.contains([SEP, '\n', '\r', '\t']) {
        return s.to_owned();
    }
    s.split([SEP, '\n', '\r', '\t'])
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn homepage(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a
            .first()
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

fn license(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.iter().map(license).collect::<Vec<_>>().join(", "),
        serde_json::Value::Object(o) => {
            if let Some(serde_json::Value::Array(ls)) = o.get("licenses") {
                let op = o
                    .get("operator")
                    .and_then(|s| s.as_str())
                    .map_or(", ".to_owned(), |op| format!(" {} ", op.to_uppercase()));
                return ls.iter().map(license).collect::<Vec<_>>().join(&op);
            }
            ["spdxId", "shortName", "fullName"]
                .iter()
                .find_map(|k| o.get(*k).and_then(|s| s.as_str()).filter(|s| !s.is_empty()))
                .unwrap_or_default()
                .to_owned()
        }
        _ => String::new(),
    }
}

/// The top-level object: only `packages` is read.
struct Top<'a>(&'a mut dyn FnMut(String, Raw));

impl<'de> DeserializeSeed<'de> for Top<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Top<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("the package list object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if key == "packages" {
                map.next_value_seed(Packages(&mut *self.0))?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

/// `packages`: attribute → entry, handed over one at a time.
struct Packages<'a>(&'a mut dyn FnMut(String, Raw));

impl<'de> DeserializeSeed<'de> for Packages<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Packages<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a map of packages")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some((attr, raw)) = map.next_entry::<String, Raw>()? {
            (self.0)(attr, raw);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
      "version": 2,
      "packages": {
        "ripgrep": {"name": "ripgrep-15.2.0", "pname": "ripgrep", "version": "15.2.0",
          "meta": {"description": "Recursively searches directories\nfor a regex",
                   "homepage": ["https://github.com/BurntSushi/ripgrep"],
                   "license": [{"spdxId": "MIT"}, {"spdxId": "Unlicense"}],
                   "mainProgram": "rg", "outputsToInstall": ["out"], "available": true}},
        "jq": {"pname": "jq", "version": "1.8.2",
          "meta": {"description": "Command-line JSON processor", "mainProgram": "jq",
                   "license": {"spdxId": "MIT"}, "outputsToInstall": ["bin", "man"]}},
        "python3Packages.ripgrep-py": {"pname": "ripgrep-py", "version": "0.1",
          "meta": {"description": "Python bindings"}},
        "vscode": {"pname": "vscode", "version": "1.106",
          "meta": {"unfree": true, "description": "Code editing", "available": false}},
        "ripgrep-all": {"pname": "ripgrep-all", "version": "0.10",
          "meta": {"description": "ripgrep, but also in PDFs", "broken": true}}
      }
    }"#;

    fn release() -> Release {
        Release {
            rev: "e21fec321d73511849707a7585e4ea155ee4bf40".into(),
            name: "nixpkgs-26.11pre1122334.e21fec321d73".into(),
            url: "https://releases.nixos.org/nixpkgs/nixpkgs-26.11pre1122334.e21fec321d73".into(),
        }
    }

    fn catalog() -> Result<Catalog> {
        let mut out = Vec::new();
        assert_eq!(build(JSON.as_bytes(), &mut out, &release())?, 5);
        Catalog::parse(&String::from_utf8_lossy(&out))
    }

    #[test]
    fn builds_and_reads_back() -> Result<()> {
        let c = catalog()?;
        assert_eq!(c.rev, release().rev);
        let rg = c.get("ripgrep").cloned().unwrap_or_default();
        assert_eq!(rg.main_program, "rg");
        assert_eq!(
            rg.description,
            "Recursively searches directories for a regex"
        );
        assert_eq!(rg.homepage, "https://github.com/BurntSushi/ripgrep");
        assert_eq!(rg.license, "MIT, Unlicense");
        assert!(rg.outputs.is_empty());
        let jq = c.get("jq").cloned().unwrap_or_default();
        assert_eq!(jq.outputs, ["bin", "man"]);
        assert_eq!(jq.license, "MIT");
        // Unavailable to Hydra only because it is unfree.
        assert!(c.get("vscode").is_some_and(|p| p.unfree && !p.unavailable));
        assert!(c.get("ripgrep-all").is_some_and(|p| p.broken));
        Ok(())
    }

    #[test]
    fn finds_by_attribute_pname_or_program() -> Result<()> {
        let c = catalog()?;
        assert_eq!(c.find("rg").map(|p| p.attr.as_str()), Some("ripgrep"));
        assert_eq!(c.find("jq").map(|p| p.attr.as_str()), Some("jq"));
        assert_eq!(
            c.find("python3Packages.ripgrep-py")
                .map(|p| p.pname.as_str()),
            Some("ripgrep-py")
        );
        // Nested packages are found only by their attribute.
        assert!(c.find("ripgrep-py").is_none());
        Ok(())
    }

    #[test]
    fn searches_names_first() -> Result<()> {
        let c = catalog()?;
        let found: Vec<&str> = c
            .search(&["ripgrep".to_owned()])
            .iter()
            .map(|p| p.attr.as_str())
            .collect();
        assert_eq!(
            found,
            ["ripgrep", "ripgrep-all", "python3Packages.ripgrep-py"]
        );
        let by_desc: Vec<&str> = c
            .search(&["json".to_owned()])
            .iter()
            .map(|p| p.attr.as_str())
            .collect();
        assert_eq!(by_desc, ["jq"]);
        assert_eq!(c.similar("ripgrp", 3), ["ripgrep"]);
        Ok(())
    }

    #[test]
    fn refuses_another_format() {
        assert!(Catalog::parse("nx-catalog 0\x1fa\x1fb\n").is_err());
    }
}
