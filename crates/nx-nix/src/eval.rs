//! Store paths for tool-tier attributes, by evaluating them at the pinned
//! release (ADR-0009): all the attributes a command needs in one `nix
//! eval`, which returns each one's outputs to install and its derivation.
//! The `.drv` files it creates are written by the daemon, as any eval's.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::nix::Nix;
use crate::{Error, Result};

/// One output of an attribute: `bin` and its store path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct Output {
    pub name: String,
    pub path: String,
}

/// What evaluating one attribute gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluated {
    Found {
        /// `lib.getName`: what an unfree allowance names.
        name: String,
        version: String,
        /// The derivation, for building what no cache has.
        drv: String,
        /// `meta.outputsToInstall`, with their store paths.
        outputs: Vec<Output>,
    },
    /// No such attribute, or not a package.
    Missing,
    /// nixpkgs refused to evaluate it.
    Refused {
        unfree: bool,
        broken: bool,
        insecure: bool,
    },
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    drv: String,
    #[serde(default)]
    outputs: Vec<Output>,
    #[serde(default)]
    unfree: bool,
    #[serde(default)]
    broken: bool,
    #[serde(default)]
    insecure: bool,
}

/// A Nix string literal for `s`.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' | '$' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The function `nix eval --apply` gets: nixpkgs' entry point, called for
/// `system` with only the `unfree` names allowed, mapped over `attrs`.
/// Forcing each result inside `tryEval` (with `deepSeq`) is what turns an
/// unfree or broken package into a value instead of an error that would
/// fail the whole evaluation.
pub fn expression(system: &str, attrs: &[String], unfree: &[String]) -> String {
    let list = |v: &[String]| v.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ");
    format!(
        r#"f: let
  pkgs = f {{ system = {system}; overlays = [ ]; config = {{ allowUnfreePredicate = p: builtins.elem (pkgs.lib.getName p) [ {unfree} ]; }}; }};
  lib = pkgs.lib;
  one = a: let
    p = lib.attrByPath (lib.splitString "." a) null pkgs;
    v = {{
      name = lib.getName p;
      version = p.version or "";
      drv = p.drvPath;
      outputs = map (o: {{ name = o; path = p.${{o}}.outPath; }}) (p.meta.outputsToInstall or [ "out" ]);
    }};
    r = builtins.tryEval (builtins.deepSeq v v);
  in
    if p == null || !(lib.isDerivation p) then {{ error = "missing"; }}
    else if r.success then r.value
    else {{ error = "refused"; unfree = p.meta.unfree or false; broken = p.meta.broken or false; insecure = p.meta.insecure or false; }};
in builtins.listToAttrs (map (a: {{ name = a; value = one a; }}) [ {attrs} ])"#,
        system = quote(system),
        unfree = list(unfree),
        attrs = list(attrs),
    )
}

/// Evaluates `attrs` from the nixpkgs at `source` (a release's
/// `nixexprs.tar.xz`) for `system`, allowing the `unfree` names.
pub fn evaluate(
    nix: &dyn Nix,
    source: &str,
    system: &str,
    attrs: &[String],
    unfree: &[String],
) -> Result<BTreeMap<String, Evaluated>> {
    if attrs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let args = [
        "eval".to_owned(),
        "--json".to_owned(),
        "--file".to_owned(),
        source.to_owned(),
        "--apply".to_owned(),
        expression(system, attrs, unfree),
    ];
    let out = nix.run(&args, &mut |_| {})?;
    parse(&out)
}

fn parse(json: &str) -> Result<BTreeMap<String, Evaluated>> {
    let raw: BTreeMap<String, Raw> = serde_json::from_str(json)
        .map_err(|e| Error::Nix(format!("unexpected answer from nix eval: {e}")))?;
    Ok(raw
        .into_iter()
        .map(|(attr, r)| {
            let e = match r.error.as_deref() {
                None => Evaluated::Found {
                    name: r.name,
                    version: r.version,
                    drv: r.drv,
                    outputs: r.outputs,
                },
                Some("missing") => Evaluated::Missing,
                Some(_) => Evaluated::Refused {
                    unfree: r.unfree,
                    broken: r.broken,
                    insecure: r.insecure,
                },
            };
            (attr, e)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nix::tests::Fake;

    /// What the VM's Nix 2.35 answered for these attributes (M6 spike).
    const ANSWER: &str = r#"{"jq":{"name":"jq","drv":"/nix/store/8ydjymrq0hlzxz7c1h1r1qsa19iw8js6-jq-1.8.2.drv","outputs":[{"name":"bin","path":"/nix/store/nh0wly5a1h79bfjgw46sly70ljdy1xkl-jq-1.8.2-bin"},{"name":"man","path":"/nix/store/00sg1s16cq6vld8sg6hy6q0kpxdgqzbg-jq-1.8.2-man"}],"version":"1.8.2"},"lib":{"error":"missing"},"nope":{"error":"missing"},"vscode":{"broken":false,"error":"refused","insecure":false,"unfree":true}}"#;

    #[test]
    fn reads_found_missing_and_refused() -> Result<()> {
        let nix = Fake::answering(ANSWER);
        let attrs = ["jq", "lib", "nope", "vscode"].map(String::from);
        let got = evaluate(&nix, "nixexprs.tar.xz", "x86_64-linux", &attrs, &[])?;
        match &got["jq"] {
            Evaluated::Found {
                version, outputs, ..
            } => {
                assert_eq!(version, "1.8.2");
                assert_eq!(outputs.len(), 2);
                assert_eq!(outputs[0].name, "bin");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(got["nope"], Evaluated::Missing);
        assert_eq!(
            got["vscode"],
            Evaluated::Refused {
                unfree: true,
                broken: false,
                insecure: false
            }
        );
        let args = nix.calls();
        assert_eq!(
            args[0][..4],
            ["eval", "--json", "--file", "nixexprs.tar.xz"]
        );
        Ok(())
    }

    #[test]
    fn quotes_what_it_puts_in_the_expression() {
        let e = expression(
            "x86_64-linux",
            &["a\"${b}".to_owned()],
            &["terraform".to_owned()],
        );
        assert!(e.contains(r#"[ "a\"\${b}" ]"#), "{e}");
        assert!(e.contains(r#"[ "terraform" ]"#), "{e}");
        assert!(e.contains(r#"system = "x86_64-linux""#), "{e}");
    }
}
