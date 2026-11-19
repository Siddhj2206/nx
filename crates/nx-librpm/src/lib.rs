//! A safe face on the image's librpm (ADR-0008): check a package file's
//! signature against keys nx chose, and unpack its payload into a directory.
//!
//! nx never installs through rpm (there is no rpmdb to write to); it reads
//! package files the way rpm does, with the image's own copy of librpm and
//! its signature policy. Like `nx-solv`, this crate is where the `unsafe`
//! lives, and nothing here crosses threads.

#![allow(unsafe_code)]

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod sys;

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use nx_scriptlet::{PackageFile, PackageMeta, Phase, Script, Scripts, Trigger};
use sha2::{Digest, Sha256, Sha512};

/// Errors from reading package files.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("rpm's configuration could not be read")]
    Config,
    #[error("not a public key: {0}")]
    Key(String),
    #[error("opening {path}: {message}")]
    Open { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Verify { path: PathBuf, message: String },
    #[error("{path} is not signed")]
    Unsigned { path: PathBuf },
    #[error("{path}: {file} {why}")]
    Refused {
        path: PathBuf,
        file: String,
        why: &'static str,
    },
    #[error("unpacking {path}: {message}")]
    Payload { path: PathBuf, message: String },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid string for librpm: {0:?}")]
    Nul(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Reads rpm's configuration once per process and keeps librpm's log
/// quiet: nx reports failures itself, from [`last_message`].
fn init() -> Result<()> {
    static DONE: OnceLock<bool> = OnceLock::new();
    let ok = *DONE.get_or_init(|| {
        // SAFETY: called once, before any other librpm call; null means
        // the default rc files and target. The callback never unwinds.
        unsafe {
            sys::rpmlogSetCallback(Some(quiet), std::ptr::null_mut());
            sys::rpmReadConfigFiles(std::ptr::null(), std::ptr::null()) == 0
        }
    });
    if ok { Ok(()) } else { Err(Error::Config) }
}

/// Swallows librpm's log lines; the last one stays readable through
/// `rpmlogMessage`.
unsafe extern "C" fn quiet(_rec: sys::rpmlogRec, _data: sys::rpmlogCallbackData) -> i32 {
    0
}

fn last_message() -> String {
    // SAFETY: rpmlogMessage returns librpm's own buffer or null.
    let msg = unsafe { owned(sys::rpmlogMessage()) };
    msg.trim().to_owned()
}

/// The keys a package may be signed with. Starts empty: nx never falls back
/// to the system keyring, so a repo's packages verify against its own keys
/// only.
pub struct Keyring {
    raw: sys::rpmKeyring,
}

impl Keyring {
    pub fn new() -> Result<Self> {
        init()?;
        // SAFETY: returns a fresh keyring, freed in Drop.
        let raw = unsafe { sys::rpmKeyringNew() };
        if raw.is_null() {
            return Err(Error::Key("rpmKeyringNew failed".into()));
        }
        Ok(Self { raw })
    }

    /// Adds every key in an ASCII-armored file (one or more blocks) and
    /// returns their primary fingerprints, in hex.
    pub fn add(&mut self, armored: &str) -> Result<Vec<String>> {
        const BEGIN: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
        let blocks: Vec<String> = armored
            .split(BEGIN)
            .skip(1)
            .map(|b| format!("{BEGIN}{b}"))
            .collect();
        if blocks.is_empty() {
            return Err(Error::Key("no armored public key block".into()));
        }
        blocks.iter().map(|b| self.add_block(b)).collect()
    }

    fn add_block(&mut self, block: &str) -> Result<String> {
        let text = CString::new(block).map_err(|_| Error::Nul("key".into()))?;
        let mut pkt: *mut u8 = std::ptr::null_mut();
        let mut len = 0usize;
        // SAFETY: valid C string; on success pkt is malloc'd by librpm and
        // freed below with rfree.
        let kind = unsafe { sys::pgpParsePkts(text.as_ptr(), &mut pkt, &mut len) };
        if kind != sys::PGPARMOR_PUBKEY || pkt.is_null() {
            return Err(Error::Key(last_message()));
        }
        // SAFETY: pkt holds len bytes; rpmPubkeyNew copies them. Keys and
        // subkeys are linked by the keyring when added, so freeing ours
        // afterwards only drops our reference. The subkey array is librpm's
        // malloc and freed with rfree.
        unsafe {
            let key = sys::rpmPubkeyNew(pkt, len);
            sys::rfree(pkt.cast());
            if key.is_null() {
                return Err(Error::Key(last_message()));
            }
            let fingerprint = owned(sys::rpmPubkeyFingerprintAsHex(key));
            sys::rpmKeyringAddKey(self.raw, key);
            let mut n = 0;
            let subkeys = sys::rpmGetSubkeys(key, &mut n);
            if !subkeys.is_null() {
                for i in 0..usize::try_from(n).unwrap_or(0) {
                    let sub = *subkeys.add(i);
                    sys::rpmKeyringAddKey(self.raw, sub);
                    sys::rpmPubkeyFree(sub);
                }
                sys::rfree(subkeys.cast());
            }
            sys::rpmPubkeyFree(key);
            Ok(fingerprint)
        }
    }
}

impl Drop for Keyring {
    fn drop(&mut self) {
        // SAFETY: created by rpmKeyringNew, freed once.
        unsafe { sys::rpmKeyringFree(self.raw) };
    }
}

/// The primary fingerprints of the keys in an armored key file.
pub fn fingerprints(armored: &str) -> Result<Vec<String>> {
    Keyring::new()?.add(armored)
}

/// What [`unpack`] wrote, and what the signed header says about the package.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Unpacked {
    pub files: usize,
    pub bytes: u64,
    /// Scriptlets, triggers and the files outside `/usr`, read from the
    /// same verified header the payload was checked against.
    pub meta: PackageMeta,
}

/// Checks that `path` is signed by a key in `keyring` and that its header
/// is intact. The payload is checked file by file by [`unpack`].
pub fn verify(path: &Path, keyring: &Keyring) -> Result<()> {
    Package::open(path, keyring).map(drop)
}

/// Verifies `path` like [`verify`], then writes its payload under `dest`
/// (which must exist), checking every file against the digest in the signed
/// header. Refuses setuid and setgid files, file capabilities, device nodes
/// and paths that leave `dest`. Files keep their mode and mtime; owners are
/// left to whoever activates the tree.
pub fn unpack(path: &Path, keyring: &Keyring, dest: &Path) -> Result<Unpacked> {
    let pkg = Package::open(path, keyring)?;
    pkg.unpack(dest)
}

/// Verifies `path` like [`verify`] and reads what its signed header says
/// about scriptlets and files outside `/usr`, without unpacking.
pub fn metadata(path: &Path, keyring: &Keyring) -> Result<PackageMeta> {
    Package::open(path, keyring)?.meta()
}

/// An open, verified package file.
struct Package {
    path: PathBuf,
    fd: sys::FD_t,
    header: sys::Header,
}

impl Package {
    fn open(path: &Path, keyring: &Keyring) -> Result<Self> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Nul(path.display().to_string()))?;
        // SAFETY: valid C strings; a failed open returns null or an fd with
        // Ferror set, which is closed below.
        let fd = unsafe { sys::Fopen(c.as_ptr(), c"r.ufdio".as_ptr()) };
        // SAFETY: fd is null or an open FD_t.
        if fd.is_null() || unsafe { sys::Ferror(fd) } != 0 {
            let message = if fd.is_null() {
                "cannot open".to_owned()
            } else {
                // SAFETY: fd is open; Fstrerror returns its own buffer.
                let m = unsafe { owned(sys::Fstrerror(fd)) };
                // SAFETY: fd is open and not used again.
                unsafe { sys::Fclose(fd) };
                m
            };
            return Err(Error::Open {
                path: path.to_owned(),
                message,
            });
        }
        let mut pkg = Self {
            path: path.to_owned(),
            fd,
            header: std::ptr::null_mut(),
        };
        // SAFETY: rpmtsCreate returns a fresh ts, freed below; setting the
        // keyring before any read stops librpm loading the system one. The
        // header comes back owned (freed in Drop); with NEEDPAYLOAD the fd
        // is left at the start of the payload.
        let rc = unsafe {
            let ts = sys::rpmtsCreate();
            sys::rpmtsSetKeyring(ts, keyring.raw);
            sys::rpmtsSetVSFlags(ts, sys::RPMVSF_NEEDPAYLOAD as _);
            let rc = sys::rpmReadPackageFile(ts, fd, c.as_ptr(), &mut pkg.header);
            sys::rpmtsFree(ts);
            rc
        };
        let fail = |message: String| Error::Verify {
            path: path.to_owned(),
            message,
        };
        match rc {
            sys::RPMRC_OK => {}
            sys::RPMRC_NOKEY => return Err(fail("signed with a key nx does not trust".into())),
            sys::RPMRC_NOTTRUSTED => {
                return Err(fail("signed with a revoked or expired key".into()));
            }
            _ => return Err(fail(last_message())),
        }
        // rpmReadPackageFile checks what signatures it finds but accepts a
        // package with none.
        let signed = [
            sys::RPMTAG_RSAHEADER,
            sys::RPMTAG_DSAHEADER,
            sys::RPMTAG_OPENPGP,
        ]
        .iter()
        // SAFETY: the header is live.
        .any(|tag| unsafe { sys::headerIsEntry(pkg.header, *tag as _) } != 0);
        if !signed {
            return Err(Error::Unsigned {
                path: path.to_owned(),
            });
        }
        Ok(pkg)
    }

    fn unpack(mut self, dest: &Path) -> Result<Unpacked> {
        let meta = self.meta()?;
        // SAFETY: the header is live; the returned string is the header's.
        let compressor = unsafe {
            owned(sys::headerGetString(
                self.header,
                sys::RPMTAG_PAYLOADCOMPRESSOR as _,
            ))
        };
        let compressor = if compressor.is_empty() {
            "gzip".to_owned()
        } else {
            compressor
        };
        let mode =
            CString::new(format!("r.{compressor}")).map_err(|_| Error::Nul(compressor.clone()))?;
        // SAFETY: the fd is open at the payload; Fdopen pushes a
        // decompressor onto it and returns the same handle.
        self.fd = unsafe { sys::Fdopen(self.fd, mode.as_ptr()) };
        if self.fd.is_null() {
            return Err(self.payload(format!("unsupported compression {compressor}")));
        }
        // SAFETY: header and fd are live; the file set keeps its own
        // reference to the header; both are freed by their guards.
        let files = Guard(
            unsafe {
                sys::rpmfilesNew(
                    std::ptr::null_mut(),
                    self.header,
                    sys::RPMTAG_BASENAMES as _,
                    sys::RPMFI_KEEPHEADER as _,
                )
            },
            // SAFETY: f came from rpmfilesNew above and is freed once.
            |f| unsafe {
                sys::rpmfilesFree(f);
            },
        );
        let fi = Guard(
            // SAFETY: as above.
            unsafe {
                sys::rpmfiNewArchiveReader(
                    self.fd,
                    files.0,
                    sys::RPMFI_ITER_READ_ARCHIVE_CONTENT_FIRST as _,
                )
            },
            // SAFETY: f came from rpmfiNewArchiveReader above and is freed
            // once, before the file set it reads (guards drop in reverse).
            |f| unsafe {
                sys::rpmfiFree(f);
            },
        );
        if fi.0.is_null() {
            return Err(self.payload(last_message()));
        }

        let mut tree = Written::default();
        loop {
            // SAFETY: fi is a live archive reader.
            let rc = unsafe { sys::rpmfiNext(fi.0) };
            if rc == RPMERR_ITER_END {
                break;
            }
            if rc < 0 {
                return Err(self.payload(format!("archive error {rc}")));
            }
            self.entry(fi.0, dest, &mut tree)?;
        }
        // SAFETY: fi is live.
        if unsafe { sys::rpmfiArchiveClose(fi.0) } > 0 {
            return Err(self.payload(last_message()));
        }
        // Directory modes last, so a read-only directory (0555) is filled
        // first; deepest first, so a parent's mode never blocks a child.
        tree.dirs.sort_by(|a, b| b.0.cmp(&a.0));
        for (dir, mode) in &tree.dirs {
            set_mode(dir, *mode)?;
        }
        tree.out.meta = meta;
        Ok(tree.out)
    }

    /// `rpm --qf`'s formatting of `fmt` against the header.
    fn format(&self, fmt: &str) -> Result<String> {
        let c = CString::new(fmt).map_err(|_| Error::Nul(fmt.to_owned()))?;
        let mut err: sys::errmsg_t = std::ptr::null();
        // SAFETY: the header is live; the result is null or malloc'd (freed
        // below), and err is set to a static message on failure.
        let out = unsafe { sys::headerFormat(self.header, c.as_ptr(), &mut err) };
        if out.is_null() {
            // SAFETY: err is null or a valid C string.
            let message = unsafe { owned(err) };
            return Err(self.payload(format!("reading the header: {message}")));
        }
        // SAFETY: out is a valid C string; copied, then freed once.
        let text = unsafe {
            let t = owned(out);
            sys::rfree(out.cast());
            t
        };
        Ok(text)
    }

    fn has(&self, tag: sys::rpmTag_e) -> bool {
        // SAFETY: the header is live.
        unsafe { sys::headerIsEntry(self.header, tag as _) != 0 }
    }

    /// Scriptlets, triggers and the files outside `/usr`.
    fn meta(&self) -> Result<PackageMeta> {
        let mut scripts = Scripts::default();
        for (phase, tag, prog_tag, name) in [
            (
                Phase::Pretrans,
                sys::RPMTAG_PRETRANS,
                sys::RPMTAG_PRETRANSPROG,
                "PRETRANS",
            ),
            (
                Phase::Pre,
                sys::RPMTAG_PREIN,
                sys::RPMTAG_PREINPROG,
                "PREIN",
            ),
            (
                Phase::Post,
                sys::RPMTAG_POSTIN,
                sys::RPMTAG_POSTINPROG,
                "POSTIN",
            ),
            (
                Phase::Preun,
                sys::RPMTAG_PREUN,
                sys::RPMTAG_PREUNPROG,
                "PREUN",
            ),
            (
                Phase::Postun,
                sys::RPMTAG_POSTUN,
                sys::RPMTAG_POSTUNPROG,
                "POSTUN",
            ),
            (
                Phase::Posttrans,
                sys::RPMTAG_POSTTRANS,
                sys::RPMTAG_POSTTRANSPROG,
                "POSTTRANS",
            ),
            (
                Phase::Preuntrans,
                sys::RPMTAG_PREUNTRANS,
                sys::RPMTAG_PREUNTRANSPROG,
                "PREUNTRANS",
            ),
            (
                Phase::Postuntrans,
                sys::RPMTAG_POSTUNTRANS,
                sys::RPMTAG_POSTUNTRANSPROG,
                "POSTUNTRANS",
            ),
        ] {
            if !self.has(tag) && !self.has(prog_tag) {
                continue;
            }
            let body = if self.has(tag) {
                self.format(&format!("%{{{name}}}"))?
            } else {
                String::new()
            };
            let interpreter = if self.has(prog_tag) {
                self.format(&format!("%{{{name}PROG}}"))?
            } else {
                String::new()
            };
            scripts.phases.push((
                phase,
                Script {
                    interpreter: interpreter.trim().to_owned(),
                    body,
                },
            ));
        }
        scripts.triggers = self.triggers("TRIGGER")?;
        scripts.file_triggers = self.triggers("FILETRIGGER")?;
        scripts
            .file_triggers
            .extend(self.triggers("TRANSFILETRIGGER")?);
        Ok(PackageMeta {
            scripts,
            files: self.outside_usr()?,
        })
    }

    /// The triggers of one family (`TRIGGER`, `FILETRIGGER`, …): each
    /// condition names a target and the index of the script it runs.
    fn triggers(&self, family: &str) -> Result<Vec<Trigger>> {
        let conds = self.format(&format!(
            "[%{{{family}NAME}}\t%{{{family}TYPE}}\t%{{{family}INDEX}}\n]"
        ))?;
        if conds.trim().is_empty() {
            return Ok(Vec::new());
        }
        let marker = "\u{1e}nx-script\u{1e}";
        let bodies = self.format(&format!("[%{{{family}SCRIPTS}}\n{marker}\n]"))?;
        let progs = self.format(&format!("[%{{{family}SCRIPTPROG}}\n]"))?;
        let bodies: Vec<&str> = bodies.split(&format!("\n{marker}\n")).collect();
        let progs: Vec<&str> = progs.lines().collect();
        let mut out = Vec::new();
        for line in conds.lines() {
            let mut f = line.split('\t');
            let (Some(target), Some(kind), Some(index)) = (f.next(), f.next(), f.next()) else {
                return Err(self.payload(format!("malformed trigger entry {line:?}")));
            };
            let index: usize = index
                .parse()
                .map_err(|_| self.payload(format!("malformed trigger index {index:?}")))?;
            let Some(body) = bodies.get(index) else {
                return Err(self.payload(format!("trigger {target} has no script")));
            };
            out.push(Trigger {
                kind: kind.to_owned(),
                target: target.to_owned(),
                script: Script {
                    interpreter: progs.get(index).copied().unwrap_or("").trim().to_owned(),
                    body: (*body).to_owned(),
                },
            });
        }
        Ok(out)
    }

    /// The file list entries not under `/usr`.
    fn outside_usr(&self) -> Result<Vec<PackageFile>> {
        let list = self.format(
            "[%{FILENAMES}\t%{FILEMODES}\t%{FILEUSERNAME}\t%{FILEGROUPNAME}\t%{FILEFLAGS}\t%{FILELINKTOS}\n]",
        )?;
        let mut out = Vec::new();
        for line in list.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let [path, mode, user, group, flags, link] = f[..] else {
                return Err(self.payload(format!("malformed file entry {line:?}")));
            };
            if path.starts_with("/usr/") || path == "/usr" {
                continue;
            }
            let num = |s: &str| {
                s.parse::<u32>()
                    .map_err(|_| self.payload(format!("malformed file entry {line:?}")))
            };
            out.push(PackageFile {
                path: path.to_owned(),
                mode: num(mode)?,
                user: user.to_owned(),
                group: group.to_owned(),
                flags: num(flags)?,
                link: link.to_owned(),
            });
        }
        Ok(out)
    }

    /// Writes the archive entry `fi` is at.
    fn entry(&self, fi: sys::rpmfi, dest: &Path, tree: &mut Written) -> Result<()> {
        // SAFETY: fi is positioned on a file; strings are copied at once.
        let (name, mode, caps, mtime, fx) = unsafe {
            (
                owned(sys::rpmfiFN(fi)),
                u32::from(sys::rpmfiFMode(fi)),
                owned(sys::rpmfiFCaps(fi)),
                sys::rpmfiFMtime(fi),
                sys::rpmfiFX(fi),
            )
        };
        let refuse = |why| Error::Refused {
            path: self.path.clone(),
            file: name.clone(),
            why,
        };
        let rel = relative(&name).ok_or_else(|| refuse("leaves the package root"))?;
        let target = dest.join(&rel);
        if mode & 0o6000 != 0 {
            return Err(refuse("is setuid or setgid"));
        }
        if !caps.is_empty() {
            return Err(refuse("has file capabilities"));
        }
        let is_dir = mode & libc::S_IFMT == libc::S_IFDIR;
        // A directory is made with its parents; anything else needs only
        // its parents, and is created with O_EXCL (which never follows a
        // symlink) below.
        let dirs = if is_dir {
            rel.as_path()
        } else {
            rel.parent().unwrap_or(Path::new(""))
        };
        if !real_dirs(dest, dirs, &mut tree.real).map_err(|e| write_err(&dest.join(dirs), e))? {
            return Err(refuse("goes through a symlink or a file"));
        }
        match mode & libc::S_IFMT {
            libc::S_IFDIR => {
                tree.dirs.push((target, mode & 0o7777));
                return Ok(());
            }
            libc::S_IFLNK => {
                // SAFETY: fi is positioned on a symlink.
                let link = unsafe { owned(sys::rpmfiFLink(fi)) };
                std::os::unix::fs::symlink(&link, &target).map_err(|e| write_err(&target, e))?;
            }
            libc::S_IFREG => {
                // SAFETY: fi is positioned on a regular file.
                if unsafe { sys::rpmfiArchiveHasContent(fi) } != 0 {
                    tree.out.bytes += self.write_file(fi, &target, &name, mtime)?;
                    set_mode(&target, mode & 0o7777)?;
                    tree.files.insert(fx, target.clone());
                } else {
                    let first = hardlink_peers(fi)
                        .into_iter()
                        .find_map(|i| tree.files.get(&i))
                        .ok_or_else(|| refuse("is a hard link with no content"))?;
                    std::fs::hard_link(first, &target).map_err(|e| write_err(&target, e))?;
                }
            }
            _ => return Err(refuse("is a device node or fifo")),
        }
        tree.out.files += 1;
        Ok(())
    }

    /// Streams the current file's content to `target`, checking it against
    /// the header's digest. Returns its size.
    fn write_file(&self, fi: sys::rpmfi, target: &Path, name: &str, mtime: u32) -> Result<u64> {
        let mut algo = 0;
        // SAFETY: fi is positioned on a regular file; the hex string is
        // malloc'd and freed with rfree.
        let expected = unsafe {
            let p = sys::rpmfiFDigestHex(fi, &mut algo);
            let s = owned(p);
            sys::rfree(p.cast());
            s
        };
        let mut hasher: Box<dyn sha2::digest::DynDigest> = match algo {
            PGPHASHALGO_SHA256 => Box::new(Sha256::new()),
            PGPHASHALGO_SHA512 => Box::new(Sha512::new()),
            _ => {
                return Err(Error::Refused {
                    path: self.path.clone(),
                    file: name.to_owned(),
                    why: "has no SHA-2 digest",
                });
            }
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .map_err(|e| write_err(target, e))?;
        let mut buf = vec![0u8; 1 << 16];
        let mut size = 0u64;
        loop {
            // SAFETY: buf is writable for its length.
            let n = unsafe { sys::rpmfiArchiveRead(fi, buf.as_mut_ptr().cast(), buf.len()) };
            let n = usize::try_from(n).map_err(|_| self.payload(format!("reading {name}")))?;
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            hasher.update(chunk);
            file.write_all(chunk).map_err(|e| write_err(target, e))?;
            size += n as u64;
        }
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != expected {
            return Err(Error::Refused {
                path: self.path.clone(),
                file: name.to_owned(),
                why: "does not match its signed digest",
            });
        }
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(mtime));
        file.set_modified(when).map_err(|e| write_err(target, e))?;
        Ok(size)
    }

    fn payload(&self, message: String) -> Error {
        Error::Payload {
            path: self.path.clone(),
            message,
        }
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        // SAFETY: each is null or owned by this package, freed once.
        unsafe {
            if !self.header.is_null() {
                sys::headerFree(self.header);
            }
            if !self.fd.is_null() {
                sys::Fclose(self.fd);
            }
        }
    }
}

const RPMERR_ITER_END: i32 = -1;
const PGPHASHALGO_SHA256: i32 = 8;
const PGPHASHALGO_SHA512: i32 = 10;

/// Runs a free function on drop.
struct Guard<T: Copy>(T, fn(T));

impl<T: Copy> Drop for Guard<T> {
    fn drop(&mut self) {
        (self.1)(self.0);
    }
}

/// The other members of the current file's hard-link set.
fn hardlink_peers(fi: sys::rpmfi) -> Vec<i32> {
    let mut peers: *const i32 = std::ptr::null();
    // SAFETY: fi is positioned on a file; the array belongs to fi and holds
    // n entries.
    unsafe {
        let n = sys::rpmfiFLinks(fi, &mut peers);
        if peers.is_null() {
            return Vec::new();
        }
        std::slice::from_raw_parts(peers, n as usize).to_vec()
    }
}

/// What an unpack has written so far.
#[derive(Default)]
struct Written {
    out: Unpacked,
    /// File index → where its content went, for hard links.
    files: HashMap<i32, PathBuf>,
    /// Directories and their modes, applied last.
    dirs: Vec<(PathBuf, u32)>,
    /// Paths under the destination known to be real directories.
    real: HashSet<PathBuf>,
}

/// Makes `rel` and every directory above it under `dest` a real
/// directory, creating what is missing. `false` when one of them is a
/// symlink or a file: a package that ships `usr/x -> /etc` and then
/// `usr/x/f` would otherwise write outside `dest`.
fn real_dirs(dest: &Path, rel: &Path, real: &mut HashSet<PathBuf>) -> std::io::Result<bool> {
    let mut path = PathBuf::new();
    for part in rel.components() {
        path.push(part);
        if real.contains(&path) {
            continue;
        }
        let full = dest.join(&path);
        match std::fs::symlink_metadata(&full) {
            // symlink_metadata never follows, so a symlink is not a dir.
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&full)?,
            Err(e) => return Err(e),
        }
        real.insert(path.clone());
    }
    Ok(true)
}

/// `/usr/bin/rg` → `usr/bin/rg`; `None` for anything that could leave the
/// destination (`..`, an empty path).
fn relative(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(name).components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| write_err(path, e))
}

fn write_err(path: &Path, source: std::io::Error) -> Error {
    Error::Write {
        path: path.to_owned(),
        source,
    }
}

/// Copies a C string librpm returned; null is empty.
///
/// # Safety
/// `s` is null or a valid NUL-terminated string.
unsafe fn owned(s: *const std::os::raw::c_char) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: per the contract above.
    unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests;
