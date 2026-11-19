//! Whether an RPM plugs into the system (design, *Tier routing*): what it
//! provides or needs ties it to the image's ABI, so it belongs in the host
//! tier. Everything here reads repo metadata only (name, provides,
//! requires), which `primary.xml` carries for every package.

/// The commands of compilers, linkers, interpreters and the build tools
/// that find host libraries: they build or load native code against the
/// image's libraries, so they never go to the tool tier (design, *Tool
/// tier*). One list for both tiers: an RPM ships one in a `bin` directory,
/// a nixpkgs package names one as its `mainProgram`. A trailing `*` matches
/// a prefix (`python3.*`: every versioned interpreter). The manifest adds to
/// it (`routing.toolchains`) and takes from it (`routing.standalone`).
pub const TOOLCHAINS: &[&str] = &[
    // C, C++ and friends
    "cc",
    "c++",
    "gcc",
    "g++",
    "gfortran",
    "gccgo",
    "clang",
    "clang++",
    "tcc",
    // linkers and assemblers
    "ld",
    "ld.lld",
    "lld",
    "as",
    // compiled languages with their own toolchains
    "rustc",
    "cargo",
    "go",
    "zig",
    "ghc",
    "ocaml",
    "nim",
    "crystal",
    "swift",
    "dotnet",
    // interpreters and runtimes that load native extensions
    "python",
    "python3",
    "python3.*",
    "node",
    "deno",
    "bun",
    "perl",
    "ruby",
    "php",
    "lua",
    "luajit",
    "java",
    "javac",
    "julia",
    "erl",
    "guile",
    // build tools that find the host's libraries
    "make",
    "cmake",
    "meson",
    "ninja",
    "pkg-config",
    "pkgconf",
];

/// [`TOOLCHAINS`] with the manifest's additions and exceptions.
#[derive(Debug, Clone)]
pub struct Toolchains {
    commands: Vec<String>,
}

impl Default for Toolchains {
    fn default() -> Self {
        Self::new(&[], &[])
    }
}

impl Toolchains {
    /// nx's list plus `extra`, less the entries `standalone` names.
    pub fn new(extra: &[String], standalone: &[String]) -> Self {
        let commands = TOOLCHAINS
            .iter()
            .map(|c| (*c).to_owned())
            .chain(extra.iter().cloned())
            .filter(|c| !standalone.contains(c))
            .collect();
        Self { commands }
    }

    /// The entry `command` matches, if any.
    pub fn matching(&self, command: &str) -> Option<&str> {
        self.commands
            .iter()
            .find(|c| match c.strip_suffix('*') {
                Some(prefix) => command.starts_with(prefix),
                None => *c == command,
            })
            .map(String::as_str)
    }

    /// The toolchain command among an RPM's files, if it ships one in a
    /// directory on `PATH`.
    pub fn in_files<'a>(&self, files: &'a [String]) -> Option<&'a str> {
        files.iter().find_map(|f| {
            let (dir, name) = f.rsplit_once('/')?;
            let on_path = matches!(dir, "/usr/bin" | "/usr/sbin" | "/bin" | "/sbin");
            (on_path && self.matching(name).is_some()).then_some(name)
        })
    }

    /// Whether a nixpkgs package is one: by its `mainProgram`, or, when
    /// it names none, by its `pname` (cc-wrapper's packages end in
    /// `-wrapper`).
    pub fn nixpkg(&self, main_program: &str, pname: &str) -> bool {
        let command = if main_program.is_empty() {
            pname.strip_suffix("-wrapper").unwrap_or(pname)
        } else {
            main_program
        };
        self.matching(command).is_some()
    }
}

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

/// What repo metadata says about a package.
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    pub name: &'a str,
    /// Provides and requires as rpm prints them.
    pub provides: &'a [String],
    pub requires: &'a [String],
    /// The files `primary.xml` lists (those in `bin` directories and `/etc`).
    pub files: &'a [String],
}

/// Why a package plugs into the system, or `None` for a standalone one.
pub fn plugs_in(p: Meta<'_>, toolchains: &Toolchains) -> Option<String> {
    let Meta {
        name,
        provides,
        requires,
        files,
    } = p;
    for suffix in ["-devel", "-static", "-headers", "-libs"] {
        if name.ends_with(suffix) {
            return Some(format!("a {suffix} package"));
        }
    }
    if let Some(command) = toolchains.in_files(files) {
        return Some(format!("a compiler, linker or interpreter ({command})"));
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

/// `libxkbcommon.so.0()(64bit)` → `libxkbcommon.so.0`.
fn soname(provide: &str) -> Option<&str> {
    let name = provide.split('(').next()?;
    (name.starts_with("lib") && name.contains(".so")).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_command_list_serves_both_tiers() {
        let t = Toolchains::default();
        // nixpkgs: (attr) mainProgram, pname, as the catalog has them.
        for (main, pname) in [
            ("go", "go"),
            ("python3.12", "python3"),
            ("node", "nodejs"),
            ("gcc", "gcc-wrapper"),
            ("java", "openjdk"),
            ("make", "gnumake"),
            ("zig", "zig"),
            ("", "ghc"),
            ("", "lld"),
        ] {
            assert!(t.nixpkg(main, pname), "{main} {pname}");
        }
        for (main, pname) in [("rg", "ripgrep"), ("gopls", "gopls"), ("", "rustup")] {
            assert!(!t.nixpkg(main, pname), "{main} {pname}");
        }
        // RPMs: what they ship on PATH.
        assert_eq!(t.in_files(&v(&["/usr/bin/python3.14"])), Some("python3.14"));
        assert_eq!(t.in_files(&v(&["/usr/bin/ld", "/usr/bin/ar"])), Some("ld"));
        assert_eq!(t.in_files(&v(&["/usr/lib/app/bin/node"])), None);
        assert_eq!(t.in_files(&v(&["/usr/bin/rg"])), None);
        // The manifest's additions and exceptions.
        let t = Toolchains::new(&["dmd".into()], &["zig".into()]);
        assert!(t.nixpkg("dmd", "dmd") && !t.nixpkg("zig", "zig"));
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn meta(name: &str) -> Meta<'_> {
        Meta {
            name,
            provides: &[],
            requires: &[],
            files: &[],
        }
    }

    fn plugs(m: Meta<'_>) -> Option<String> {
        plugs_in(m, &Toolchains::default())
    }

    /// A package with no files on `PATH`.
    fn by_deps(name: &str, provides: &[String], requires: &[String]) -> Option<String> {
        plugs(Meta {
            provides,
            requires,
            ..meta(name)
        })
    }

    #[test]
    fn libraries_headers_and_toolchains_plug_in() {
        assert_eq!(
            by_deps("wayland-devel", &[], &[]).as_deref(),
            Some("a -devel package")
        );
        assert_eq!(
            by_deps("libxkbcommon", &v(&["libxkbcommon.so.0()(64bit)"]), &[]).as_deref(),
            Some("provides the library libxkbcommon.so.0")
        );
        assert_eq!(
            by_deps("foo", &v(&["pkgconfig(foo) = 1.0"]), &[]).as_deref(),
            Some("provides pkgconfig(foo)")
        );
        assert_eq!(
            plugs(Meta {
                files: &v(&["/usr/bin/go", "/usr/bin/gofmt"]),
                ..meta("golang-bin")
            })
            .as_deref(),
            Some("a compiler, linker or interpreter (go)")
        );
    }

    #[test]
    fn session_libraries_and_services_plug_in() {
        assert_eq!(
            by_deps("foot", &[], &v(&["libwayland-client.so.0()(64bit)"])).as_deref(),
            Some("links libwayland-client.so.0, which must be the host's")
        );
        assert_eq!(
            by_deps("syncthing", &[], &v(&["systemd"])).as_deref(),
            Some("ships system services")
        );
    }

    #[test]
    fn a_plain_command_line_tool_is_standalone() {
        let requires = v(&["libc.so.6()(64bit)", "libpcre2-8.so.0()(64bit)"]);
        assert_eq!(by_deps("ripgrep", &v(&["ripgrep"]), &requires), None);
        let provides = v(&["jq", "libjq.so.1()(64bit)"]);
        assert_eq!(
            by_deps("jq", &provides, &v(&["libonig.so.5()(64bit)"])),
            None
        );
    }
}
