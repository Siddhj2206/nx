//! Which tier `nx add <spec>` puts a package in (design, *Tier routing*):
//! the host tier for what plugs into the system, the tool tier for
//! standalone tools nixpkgs has. [`decide`] is a pure function of what the
//! repos and the catalog say, so the order is tested without either.

use nx_core::CliTools;
use nx_core::manifest::Routing;
use nx_rpm::plugs::Toolchains;

/// `--host` or `--tool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forced {
    Host,
    Tool,
}

/// What the repos say about a spec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rpm {
    pub name: String,
    /// The image has it.
    pub installed: bool,
    /// Why it plugs into the system, if it does.
    pub plugs: Option<String>,
}

/// What the nixpkgs catalog says about a spec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Nixpkg {
    pub attr: String,
    /// `lib.getName`: what the unfree allowlist names.
    pub pname: String,
    pub unfree: bool,
    /// On the manifest's allowlist, or `--allow-unfree`.
    pub allowed: bool,
    /// Broken, or not available on this platform.
    pub broken: bool,
    /// A compiler, linker or interpreter: never the tool tier unless asked
    /// for with `--tool` (design, *Tool tier*).
    pub toolchain: bool,
}

impl Nixpkg {
    /// What the catalog says about `p`; `allowed` if its license is.
    pub fn new(p: &nx_nix::catalog::Package, allowed: bool, toolchains: &Toolchains) -> Self {
        Self {
            attr: p.attr.clone(),
            pname: p.pname.clone(),
            unfree: p.unfree,
            allowed,
            broken: p.broken || p.unavailable,
            toolchain: toolchains.nixpkg(&p.main_program, &p.pname),
        }
    }
}

/// The toolchain commands as the manifest's `[routing]` adjusts them.
pub fn toolchains(routing: &Routing) -> Toolchains {
    Toolchains::new(&routing.toolchains, &routing.standalone)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The host tier, and why.
    Host(String),
    /// The tool tier: the attribute to add, and why.
    Tool { attr: String, reason: String },
    /// Neither: what to tell the user.
    Refuse(String),
}

/// Whether `spec` can only mean an RPM: a file, a provide such as
/// `pkgconfig(x)`, or `name@repo`.
pub fn host_only(spec: &str) -> bool {
    spec.contains(['/', '(', '@'])
}

/// The first rule that matches wins:
///
/// 1. a file, provide or `@repo` spec, or `--host` → host;
/// 2. `--tool` → tool, if nixpkgs has it;
/// 3. in the image → host (nothing to install);
/// 4. plugs into the system → host;
/// 5. a usable nixpkgs package that isn't a toolchain, unless
///    `routing.cli_tools = "fedora"` → tool;
/// 6. an RPM → host;
/// 7. a nixpkgs toolchain, or a package that needs allowing (unfree) or
///    can't build → refuse and say why;
/// 8. nothing → refuse, with names that are close.
pub fn decide(
    spec: &str,
    forced: Option<Forced>,
    cli_tools: CliTools,
    rpm: Option<&Rpm>,
    nix: Option<&Nixpkg>,
    similar: &[String],
) -> Route {
    if host_only(spec) {
        return Route::Host("a file, provide or repo".into());
    }
    if forced == Some(Forced::Host) {
        return Route::Host("--host".into());
    }
    if forced == Some(Forced::Tool) {
        return match nix {
            Some(n) if usable(n) => tool(n, "--tool"),
            Some(n) => refuse_nix(n),
            None => Route::Refuse(format!("nixpkgs has no package {spec}{}", hint(similar))),
        };
    }
    if let Some(r) = rpm {
        if r.installed {
            return Route::Host("in the image".into());
        }
        if let Some(why) = &r.plugs {
            return Route::Host(format!("plugs into the system: {why}"));
        }
    }
    if cli_tools == CliTools::Nix
        && let Some(n) = nix.filter(|n| usable(n) && !n.toolchain)
    {
        return tool(n, "a standalone tool");
    }
    if rpm.is_some() {
        let why = if nix.is_some_and(|n| n.toolchain) {
            "a compiler, linker or interpreter"
        } else if cli_tools == CliTools::Fedora {
            "a standalone tool; routing.cli_tools is fedora"
        } else {
            "a standalone tool nixpkgs doesn't have"
        };
        return Route::Host(why.into());
    }
    if let Some(n) = nix.filter(|n| n.toolchain) {
        return Route::Refuse(format!(
            "{} is a compiler, linker or interpreter, which nx takes from Fedora so it builds \
             against the image's libraries, and no RPM is called {spec} (nx search {spec}); \
             nx add --tool {spec} installs nixpkgs' all the same",
            n.attr
        ));
    }
    match nix {
        Some(n) => refuse_nix(n),
        None => Route::Refuse(format!(
            "no RPM or nixpkgs package is called {spec}{}",
            hint(similar)
        )),
    }
}

fn usable(n: &Nixpkg) -> bool {
    !n.broken && (!n.unfree || n.allowed)
}

fn tool(n: &Nixpkg, why: &str) -> Route {
    let reason = if n.unfree {
        format!("{why}; unfree, allowed")
    } else {
        why.to_owned()
    };
    Route::Tool {
        attr: n.attr.clone(),
        reason,
    }
}

fn refuse_nix(n: &Nixpkg) -> Route {
    if n.broken {
        return Route::Refuse(format!(
            "nixpkgs has {}, but it is broken or not built for this platform",
            n.attr
        ));
    }
    Route::Refuse(format!(
        "{} is unfree; if its license is fine with you: nx add --allow-unfree {}",
        n.attr, n.attr
    ))
}

fn hint(similar: &[String]) -> String {
    if similar.is_empty() {
        " (try nx search)".to_owned()
    } else {
        format!(" (did you mean {}?)", similar.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpm(plugs: Option<&str>) -> Rpm {
        Rpm {
            name: "x".into(),
            installed: false,
            plugs: plugs.map(str::to_owned),
        }
    }

    fn nixpkg(attr: &str) -> Nixpkg {
        Nixpkg {
            attr: attr.into(),
            pname: attr.into(),
            ..Nixpkg::default()
        }
    }

    fn tool_route(attr: &str, reason: &str) -> Route {
        Route::Tool {
            attr: attr.into(),
            reason: reason.into(),
        }
    }

    #[test]
    fn standalone_tools_go_to_nixpkgs_and_the_rest_to_fedora() {
        let rg = nixpkg("ripgrep");
        assert_eq!(
            decide(
                "ripgrep",
                None,
                CliTools::Nix,
                Some(&rpm(None)),
                Some(&rg),
                &[]
            ),
            tool_route("ripgrep", "a standalone tool")
        );
        assert_eq!(
            decide("rg", None, CliTools::Nix, None, Some(&rg), &[]),
            tool_route("ripgrep", "a standalone tool")
        );
        assert_eq!(
            decide(
                "ripgrep",
                None,
                CliTools::Fedora,
                Some(&rpm(None)),
                Some(&rg),
                &[]
            ),
            Route::Host("a standalone tool; routing.cli_tools is fedora".into())
        );
        let devel = rpm(Some("a -devel package"));
        assert_eq!(
            decide(
                "wayland-devel",
                None,
                CliTools::Nix,
                Some(&devel),
                Some(&nixpkg("wayland")),
                &[]
            ),
            Route::Host("plugs into the system: a -devel package".into())
        );
        assert_eq!(
            decide(
                "pkgconfig(x11)",
                Some(Forced::Tool),
                CliTools::Nix,
                None,
                None,
                &[]
            ),
            Route::Host("a file, provide or repo".into())
        );
        let image = Rpm {
            installed: true,
            ..rpm(None)
        };
        assert_eq!(
            decide(
                "git",
                None,
                CliTools::Nix,
                Some(&image),
                Some(&nixpkg("git")),
                &[]
            ),
            Route::Host("in the image".into())
        );
    }

    #[test]
    fn flags_win_over_the_order() {
        let devel = rpm(Some("a -devel package"));
        assert_eq!(
            decide(
                "jq",
                Some(Forced::Host),
                CliTools::Nix,
                Some(&rpm(None)),
                Some(&nixpkg("jq")),
                &[]
            ),
            Route::Host("--host".into())
        );
        assert_eq!(
            decide(
                "zig",
                Some(Forced::Tool),
                CliTools::Nix,
                Some(&devel),
                Some(&nixpkg("zig")),
                &[]
            ),
            tool_route("zig", "--tool")
        );
        assert!(matches!(
            decide(
                "nope",
                Some(Forced::Tool),
                CliTools::Nix,
                Some(&rpm(None)),
                None,
                &[]
            ),
            Route::Refuse(_)
        ));
    }

    #[test]
    fn unfree_needs_allowing_and_fedora_comes_next() {
        let mut ngrok = Nixpkg {
            unfree: true,
            ..nixpkg("ngrok")
        };
        assert_eq!(
            decide("ngrok", None, CliTools::Nix, None, Some(&ngrok), &[]),
            Route::Refuse(
                "ngrok is unfree; if its license is fine with you: nx add --allow-unfree ngrok"
                    .into()
            )
        );
        // A vendor RPM repo is the next choice.
        assert_eq!(
            decide(
                "ngrok",
                None,
                CliTools::Nix,
                Some(&rpm(None)),
                Some(&ngrok),
                &[]
            ),
            Route::Host("a standalone tool nixpkgs doesn't have".into())
        );
        ngrok.allowed = true;
        assert_eq!(
            decide("ngrok", None, CliTools::Nix, None, Some(&ngrok), &[]),
            tool_route("ngrok", "a standalone tool; unfree, allowed")
        );
    }

    #[test]
    fn toolchains_stay_out_of_the_tool_tier_unless_forced() {
        let go = Nixpkg {
            toolchain: true,
            ..nixpkg("go")
        };
        let golang = Rpm {
            name: "golang".into(),
            ..rpm(None)
        };
        assert!(matches!(
            decide("go", None, CliTools::Nix, None, Some(&go), &[]),
            Route::Refuse(m) if m.starts_with("go is a compiler")
        ));
        assert_eq!(
            decide("go", None, CliTools::Nix, Some(&golang), Some(&go), &[]),
            Route::Host("a compiler, linker or interpreter".into())
        );
        assert_eq!(
            decide(
                "go",
                Some(Forced::Tool),
                CliTools::Nix,
                None,
                Some(&go),
                &[]
            ),
            tool_route("go", "--tool")
        );
    }

    #[test]
    fn nothing_found_suggests_close_names() {
        assert_eq!(
            decide(
                "ripgrap",
                None,
                CliTools::Nix,
                None,
                None,
                &["ripgrep".into()]
            ),
            Route::Refuse(
                "no RPM or nixpkgs package is called ripgrap (did you mean ripgrep?)".into()
            )
        );
    }
}
