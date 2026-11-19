//! Whether an RPM plugs into the system (design, *Tier routing*): what it
//! provides or needs ties it to the image's ABI, so it belongs in the host
//! tier. Everything here reads repo metadata only (name, provides,
//! requires), which `primary.xml` carries for every package.

/// Compilers, linkers and interpreters: they build or load native code
/// against the host's libraries, so they never go to the tool tier.
const TOOLCHAINS: &[&str] = &[
    "gcc",
    "gcc-c++",
    "gcc-gfortran",
    "clang",
    "llvm",
    "lld",
    "binutils",
    "cargo",
    "rust",
    "rustc",
    "golang",
    "go",
    "python3",
    "python",
    "nodejs",
    "node",
    "perl",
    "ruby",
    "php",
    "lua",
    "luajit",
    "java",
    "openjdk",
    "ghc",
    "ocaml",
    "make",
    "cmake",
    "meson",
    "ninja-build",
    "pkgconf",
    "pkg-config",
    "zig",
];

/// Sonames whose presence among a package's requirements means it talks to
/// the GPU, the display server or the sound server: those must load the
/// host's copies.
const SESSION_LIBS: &[&str] = &[
    "libGL.so",
    "libEGL.so",
    "libGLX.so",
    "libvulkan.so",
    "libgbm.so",
    "libdrm.so",
    "libwayland-client.so",
    "libxkbcommon.so",
    "libX11.so",
    "libxcb.so",
    "libasound.so",
    "libpulse.so",
    "libpipewire-0.3.so",
    "libgtk-3.so",
    "libgtk-4.so",
    "libQt5Gui.so",
    "libQt6Gui.so",
    "libSDL2",
    "libSDL3",
];

/// Why a package with this `name`, `provides` and `requires` (as rpm
/// prints them) plugs into the system, or `None` for a standalone one.
pub fn plugs_in(name: &str, provides: &[String], requires: &[String]) -> Option<String> {
    for suffix in ["-devel", "-static", "-headers", "-libs"] {
        if name.ends_with(suffix) {
            return Some(format!("a {suffix} package"));
        }
    }
    let base = toolchain_base(name);
    if TOOLCHAINS.contains(&base) {
        return Some("a compiler, linker or interpreter".to_owned());
    }
    // A library package. A tool that carries its own library (jq ships
    // libjq) is still a tool.
    if name.starts_with("lib")
        && let Some(lib) = provides.iter().find_map(|p| soname(p))
    {
        return Some(format!("provides the library {lib}"));
    }
    if let Some(pc) = provides.iter().find(|p| p.starts_with("pkgconfig(")) {
        let pc = pc.split(' ').next().unwrap_or(pc);
        return Some(format!("provides {pc}"));
    }
    if let Some(lib) = requires
        .iter()
        .find(|r| SESSION_LIBS.iter().any(|l| r.starts_with(l)))
    {
        let lib = lib.split('(').next().unwrap_or(lib);
        return Some(format!("links {lib}, which must be the host's"));
    }
    if requires
        .iter()
        .any(|r| r == "systemd" || r == "systemd-units")
    {
        return Some("ships system services".to_owned());
    }
    None
}

/// `python3.14` → `python3`, `java-21-openjdk` → `openjdk`, `gcc` → `gcc`:
/// the name a toolchain goes by across versions.
/// Whether a nixpkgs attribute or package name is a compiler, linker or
/// interpreter (design, *Tool tier*: those never go there). nixpkgs names
/// versions with digits and underscores (`go_1_23`, `python312`,
/// `nodejs_22`, `lua5_4`, `gcc14`, `jdk21`), and has a few names of its own.
pub fn nix_toolchain(name: &str) -> bool {
    const NIX_ONLY: &[&str] = &[
        "gnumake",
        "jdk",
        "gfortran",
        "llvmPackages",
        "rustc",
        "gcc-unwrapped",
    ];
    let leaf = name.rsplit('.').next().unwrap_or(name);
    let base = leaf.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_' || c == '.');
    let base = base.strip_suffix("Packages").unwrap_or(base);
    TOOLCHAINS.contains(&base)
        || NIX_ONLY.contains(&base)
        || NIX_ONLY.contains(&leaf)
        || TOOLCHAINS.contains(&toolchain_base(leaf))
}

fn toolchain_base(name: &str) -> &str {
    if name.starts_with("java-") && name.contains("openjdk") {
        return "openjdk";
    }
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if trimmed.len() < name.len() && TOOLCHAINS.contains(&trimmed) {
        return trimmed;
    }
    // python3.14 → python3: keep the major digit some names carry.
    match name.split_once('.') {
        Some((head, _)) if TOOLCHAINS.contains(&head) => head,
        _ => name,
    }
}

/// `libxkbcommon.so.0()(64bit)` → `libxkbcommon.so.0`.
fn soname(provide: &str) -> Option<&str> {
    let name = provide.split('(').next()?;
    (name.starts_with("lib") && name.contains(".so")).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_nixpkgs_toolchain_names() {
        for t in [
            "go",
            "go_1_23",
            "python312",
            "python3",
            "nodejs_22",
            "lua5_4",
            "gcc14",
            "jdk21",
            "gnumake",
            "rustc",
            "zig_0_13",
            "clang_19",
        ] {
            assert!(nix_toolchain(t), "{t}");
        }
        for t in [
            "ripgrep",
            "gopls",
            "rustup",
            "golangci-lint",
            "python3Packages.black",
            "nodePackages.prettier",
        ] {
            assert!(!nix_toolchain(t), "{t}");
        }
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn libraries_headers_and_toolchains_plug_in() {
        assert_eq!(
            plugs_in("wayland-devel", &[], &[]).as_deref(),
            Some("a -devel package")
        );
        assert_eq!(
            plugs_in("libxkbcommon", &v(&["libxkbcommon.so.0()(64bit)"]), &[]).as_deref(),
            Some("provides the library libxkbcommon.so.0")
        );
        assert_eq!(
            plugs_in("foo", &v(&["pkgconfig(foo) = 1.0"]), &[]).as_deref(),
            Some("provides pkgconfig(foo)")
        );
        for name in ["cargo", "python3.14", "gcc", "java-21-openjdk", "nodejs22"] {
            assert!(plugs_in(name, &[], &[]).is_some(), "{name}");
        }
    }

    #[test]
    fn session_libraries_and_services_plug_in() {
        assert_eq!(
            plugs_in("foot", &[], &v(&["libwayland-client.so.0()(64bit)"])).as_deref(),
            Some("links libwayland-client.so.0, which must be the host's")
        );
        assert_eq!(
            plugs_in("syncthing", &[], &v(&["systemd"])).as_deref(),
            Some("ships system services")
        );
    }

    #[test]
    fn a_plain_command_line_tool_is_standalone() {
        let requires = v(&["libc.so.6()(64bit)", "libpcre2-8.so.0()(64bit)"]);
        assert_eq!(plugs_in("ripgrep", &v(&["ripgrep"]), &requires), None);
        let provides = v(&["jq", "libjq.so.1()(64bit)"]);
        assert_eq!(
            plugs_in("jq", &provides, &v(&["libonig.so.5()(64bit)"])),
            None
        );
    }
}
