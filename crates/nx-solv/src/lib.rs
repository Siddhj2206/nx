//! A safe face on the image's libsolv (ADR-0007).
//!
//! [`Pool`] owns a libsolv pool: repos are loaded into it (the image's rpmdb
//! as the installed set, repo metadata as the rest), then it answers
//! lookups and solves install jobs. Everything `unsafe` in nx is in this
//! crate; nothing here outlives the pool, and a pool never crosses threads.

#![allow(unsafe_code)]

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod sys;

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;

/// Errors from libsolv.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("opening {path}")]
    Open { path: std::path::PathBuf },
    #[error("loading {what}: {message}")]
    Load { what: String, message: String },
    #[error("writing {path}: {message}")]
    Write {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("invalid string for libsolv: {0:?}")]
    Nul(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A repo loaded into a [`Pool`], by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RepoId(usize);

/// A package in a [`Pool`], by libsolv's solvable id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct SolvableId(sys::Id);

/// Which dependency list [`Pool::deps`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deps {
    Provides,
    Requires,
}

/// What nx needs to know about one package.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Package {
    pub id: SolvableId,
    pub name: String,
    /// `[epoch:]version-release`.
    pub evr: String,
    pub arch: String,
    /// The repo it comes from; the installed repo's name for image packages.
    pub repo: String,
    pub installed: bool,
    pub summary: String,
    /// `name-version-release.src.rpm`.
    pub source: String,
    /// The source package's name (`wayland` for `wayland-devel`) and its
    /// `version-release`, as the metadata records them.
    pub source_name: String,
    pub source_evr: String,
    pub license: String,
    pub url: String,
    /// Path relative to the repo base (`Packages/l/…rpm`).
    pub location: String,
    pub download_size: u64,
    pub install_size: u64,
    /// The package file's SHA-256 (hex), from the repo metadata; empty when
    /// the metadata has none or uses another algorithm.
    pub sha256: String,
}

impl Package {
    /// `name-evr.arch`, as dnf prints it.
    pub fn nevra(&self) -> String {
        format!("{}-{}.{}", self.name, self.evr, self.arch)
    }
}

/// The outcome of a solve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Solution {
    /// Packages to add, none of them in the image.
    Install(Vec<Package>),
    /// Why it can't be solved.
    Problems(Problems),
}

/// A failed solve.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Problems {
    /// libsolv's explanation of each problem, one string per problem.
    pub messages: Vec<String>,
    /// Every package the problems' rules name, as the package a rule is
    /// about or a provider of the dependency it is about, hidden ones
    /// included. A caller that can supply a missing build looks here for
    /// what is in the way.
    pub involved: Vec<SolvableId>,
}

impl Problems {
    fn message(messages: Vec<String>) -> Self {
        Self {
            messages,
            involved: Vec::new(),
        }
    }
}

/// A libsolv pool.
pub struct Pool {
    raw: NonNull<sys::Pool>,
    repos: Vec<NonNull<sys::Repo>>,
    installed: Option<RepoId>,
}

impl Pool {
    /// An empty pool resolving for `arch` (`x86_64`).
    pub fn new(arch: &str) -> Result<Self> {
        let arch = cstr(arch)?;
        // SAFETY: pool_create returns a fresh pool or aborts on OOM.
        let raw = unsafe { sys::pool_create() };
        let raw = NonNull::new(raw).ok_or_else(|| Error::Load {
            what: "pool".into(),
            message: "pool_create returned null".into(),
        })?;
        // SAFETY: raw is a live pool; arch is a valid C string.
        unsafe { sys::pool_setarch(raw.as_ptr(), arch.as_ptr()) };
        Ok(Self {
            raw,
            repos: Vec::new(),
            installed: None,
        })
    }

    fn p(&self) -> *mut sys::Pool {
        self.raw.as_ptr()
    }

    fn create_repo(&mut self, name: &str, priority: i32) -> Result<(RepoId, *mut sys::Repo)> {
        let name = cstr(name)?;
        // SAFETY: live pool; libsolv copies the name.
        let repo = unsafe { sys::repo_create(self.p(), name.as_ptr()) };
        let repo = NonNull::new(repo).ok_or_else(|| Error::Load {
            what: "repo".into(),
            message: "repo_create returned null".into(),
        })?;
        // libsolv: higher priority wins; dnf: lower wins.
        // SAFETY: repo is live and owned by the pool.
        unsafe { (*repo.as_ptr()).priority = -priority };
        self.repos.push(repo);
        Ok((RepoId(self.repos.len() - 1), repo.as_ptr()))
    }

    /// Loads the rpmdb under `root` (`/` or a deployment's root) as the
    /// installed set.
    pub fn load_rpmdb(&mut self, name: &str, root: &Path) -> Result<RepoId> {
        let rootdir = cpath(root)?;
        // SAFETY: live pool; libsolv copies rootdir.
        unsafe { sys::pool_set_rootdir(self.p(), rootdir.as_ptr()) };
        let (id, repo) = self.create_repo(name, 0)?;
        // Without REPO_USE_ROOTDIR libsolv ignores the pool's rootdir and
        // reads the running system's rpmdb.
        let flags = sys::REPO_USE_ROOTDIR as i32;
        // SAFETY: repo belongs to this pool; no reference repo.
        let rc = unsafe { sys::repo_add_rpmdb(repo, std::ptr::null_mut(), flags) };
        // SAFETY: as above; an empty rootdir resets it.
        unsafe { sys::pool_set_rootdir(self.p(), std::ptr::null()) };
        if rc != 0 {
            return Err(self.error(format!("rpmdb under {}", root.display())));
        }
        self.set_installed(id);
        Ok(id)
    }

    /// Loads repo metadata (`primary.xml`, compressed or not).
    pub fn load_rpmmd(&mut self, name: &str, path: &Path, priority: i32) -> Result<RepoId> {
        let fp = File::open(path)?;
        let (id, repo) = self.create_repo(name, priority)?;
        // SAFETY: repo is live; fp is an open FILE*.
        let rc = unsafe { sys::repo_add_rpmmd(repo, fp.0, std::ptr::null(), 0) };
        if rc != 0 {
            return Err(self.error(path.display().to_string()));
        }
        Ok(id)
    }

    /// Loads a `.solv` file written by [`Pool::write_solv`].
    pub fn load_solv(&mut self, name: &str, path: &Path, priority: i32) -> Result<RepoId> {
        let fp = File::open(path)?;
        let (id, repo) = self.create_repo(name, priority)?;
        // SAFETY: repo is live; fp is an open FILE*.
        let rc = unsafe { sys::repo_add_solv(repo, fp.0, 0) };
        if rc != 0 {
            return Err(self.error(path.display().to_string()));
        }
        Ok(id)
    }

    /// An empty repo, for packages added one file at a time
    /// ([`Pool::add_rpm`]).
    pub fn create_empty(&mut self, name: &str, priority: i32) -> Result<RepoId> {
        Ok(self.create_repo(name, priority)?.0)
    }

    /// Adds one package file's header to `repo`. Call [`Pool::prepare`]
    /// again before the next lookup or solve.
    pub fn add_rpm(&mut self, repo: RepoId, path: &Path) -> Result<SolvableId> {
        let c = cpath(path)?;
        // SAFETY: the repo belongs to this pool; libsolv reads the file and
        // internalizes the repo (no REPO_NO_INTERNALIZE).
        let id = unsafe { sys::repo_add_rpm(self.repo_ptr(repo), c.as_ptr(), 0) };
        if id == 0 {
            return Err(self.error(path.display().to_string()));
        }
        Ok(SolvableId(id))
    }

    /// Writes a repo as a `.solv` file, which loads in a fraction of the
    /// time its XML takes.
    pub fn write_solv(&self, repo: RepoId, path: &Path) -> Result<()> {
        let tmp = path.with_extension("solv.part");
        let fp = File::create(&tmp)?;
        // SAFETY: the repo belongs to this pool; fp is open for writing.
        let rc = unsafe { sys::repo_write(self.repo_ptr(repo), fp.0) };
        let closed = fp.close();
        if rc != 0 || !closed {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Write {
                path: path.to_owned(),
                message: self.last_error(),
            });
        }
        std::fs::rename(&tmp, path).map_err(|e| Error::Write {
            path: path.to_owned(),
            message: e.to_string(),
        })
    }

    /// Marks a repo as the installed set.
    pub fn set_installed(&mut self, repo: RepoId) {
        // SAFETY: both pointers are live and belong together.
        unsafe { sys::pool_set_installed(self.p(), self.repo_ptr(repo)) };
        self.installed = Some(repo);
    }

    /// Builds the lookup indexes. Call after loading every repo and before
    /// [`Pool::select`] or [`Pool::install`].
    pub fn prepare(&mut self) {
        // A considered map from an earlier solve is sized for the solvables
        // of then; one added since would read past it.
        self.set_considered(&[]);
        // SAFETY: live pool.
        unsafe {
            sys::pool_addfileprovides(self.p());
            sys::pool_createwhatprovides(self.p());
        }
    }

    fn repo_ptr(&self, repo: RepoId) -> *mut sys::Repo {
        self.repos[repo.0].as_ptr()
    }

    /// Every package in every repo, installed ones included.
    pub fn all(&self) -> impl Iterator<Item = SolvableId> + '_ {
        let pool = self.p();
        self.repos.iter().flat_map(move |repo| {
            // SAFETY: repo is live; start..end bounds its solvables, and an
            // id in that range whose repo pointer differs is a free slot.
            let (start, end) = unsafe { ((*repo.as_ptr()).start, (*repo.as_ptr()).end) };
            (start..end).filter_map(move |id| {
                // SAFETY: id < pool.nsolvables because end <= nsolvables.
                let s = unsafe { &*(*pool).solvables.add(id as usize) };
                (s.repo == repo.as_ptr()).then_some(SolvableId(id))
            })
        })
    }

    /// Packages a user's word matches, the way dnf matches `install` args:
    /// names, provides (`pkgconfig(xkbcommon)`, `/usr/bin/rg`), globs,
    /// `name.arch` and `name = version`. `spec@repo` matches only that
    /// repo's packages.
    pub fn select(&self, spec: &str) -> Result<Vec<SolvableId>> {
        if let Some((spec, repo)) = split_repo(spec) {
            return Ok(self
                .select_any(spec)?
                .into_iter()
                .filter(|id| self.repo_name(*id) == repo)
                .collect());
        }
        self.select_any(spec)
    }

    /// The name of the repo `id` belongs to.
    fn repo_name(&self, id: SolvableId) -> String {
        // SAFETY: ids come from this pool, so they index its solvables; the
        // name is copied before any other libsolv call.
        unsafe {
            let s = (*self.p()).solvables.add(id.0 as usize);
            if (*s).repo.is_null() {
                String::new()
            } else {
                owned((*(*s).repo).name)
            }
        }
    }

    fn select_any(&self, spec: &str) -> Result<Vec<SolvableId>> {
        let spec = cstr(spec)?;
        let flags = sys::SELECTION_NAME
            | sys::SELECTION_PROVIDES
            | sys::SELECTION_GLOB
            | sys::SELECTION_CANON
            | sys::SELECTION_DOTARCH
            | sys::SELECTION_REL;
        let mut sel = Queue::new();
        let mut pkgs = Queue::new();
        // SAFETY: live pool, initialised queues, valid C string.
        unsafe {
            sys::selection_make(self.p(), &mut sel.0, spec.as_ptr(), flags as i32);
            sys::selection_solvables(self.p(), &mut sel.0, &mut pkgs.0);
        }
        Ok(pkgs.ids().map(SolvableId).collect())
    }

    /// Image packages that own `path`, from the rpmdb's file lists.
    pub fn image_owners(&self, path: &str) -> Result<Vec<SolvableId>> {
        let path = cstr(path)?;
        let flags = sys::SELECTION_FILELIST | sys::SELECTION_INSTALLED_ONLY;
        let mut sel = Queue::new();
        let mut pkgs = Queue::new();
        // SAFETY: as in select().
        unsafe {
            sys::selection_make(self.p(), &mut sel.0, path.as_ptr(), flags as i32);
            sys::selection_solvables(self.p(), &mut sel.0, &mut pkgs.0);
        }
        Ok(pkgs.ids().map(SolvableId).collect())
    }

    /// A package's file list: every file for an installed package, the
    /// ones `primary.xml` carries (`*/bin/*`, `/etc/*`) for a repo's.
    pub fn files(&self, id: SolvableId) -> Vec<String> {
        let mut out = Vec::new();
        // SAFETY: an all-zero Dataiterator is plain data (ints and null
        // pointers), and dataiterator_init sets every field before use; the
        // id comes from this pool; each path is copied out of libsolv's
        // temporary space before the next step reuses it; the iterator is
        // freed on every path out.
        unsafe {
            let mut di: sys::Dataiterator = std::mem::zeroed();
            if sys::dataiterator_init(
                &mut di,
                self.p(),
                std::ptr::null_mut(),
                id.0,
                sys::SOLVABLE_FILELIST as i32,
                std::ptr::null(),
                0,
            ) != 0
            {
                return out;
            }
            while sys::dataiterator_step(&mut di) != 0 {
                out.push(owned(sys::repodata_dir2str(di.data, di.kv.id, di.kv.str_)));
            }
            sys::dataiterator_free(&mut di);
        }
        out
    }

    /// Reads a package's details.
    pub fn package(&self, id: SolvableId) -> Package {
        // SAFETY: ids come from this pool (all/select/solve), so they index
        // its solvables array; lookups copy into owned Strings before any
        // other libsolv call can reuse the temporary space.
        unsafe {
            let p = self.p();
            let s = (*p).solvables.add(id.0 as usize);
            let installed = !(*p).installed.is_null() && (*s).repo == (*p).installed;
            let repo_name = if (*s).repo.is_null() {
                String::new()
            } else {
                owned((*(*s).repo).name)
            };
            let mut medianr = 0;
            let mut kind = 0;
            let sum = sys::solvable_lookup_checksum(s, sys::SOLVABLE_CHECKSUM as i32, &mut kind);
            let sha256 = if kind == sys::REPOKEY_TYPE_SHA256 as i32 {
                owned(sum)
            } else {
                String::new()
            };
            // libsolv leaves the source name and evr out when they are the
            // package's own, as solvable_lookup_sourcepkg does.
            let source_id = |key: u32, own: sys::Id| match sys::solvable_lookup_id(s, key as i32) {
                0 => own,
                id => id,
            };
            let source_evr = owned(sys::pool_id2str(
                p,
                source_id(sys::SOLVABLE_SOURCEEVR, (*s).evr),
            ));
            // A source package's file name has no epoch.
            let source_evr = match source_evr.split_once(':') {
                Some((epoch, rest)) if epoch.bytes().all(|b| b.is_ascii_digit()) => rest.to_owned(),
                _ => source_evr,
            };
            Package {
                id,
                source_name: owned(sys::pool_id2str(
                    p,
                    source_id(sys::SOLVABLE_SOURCENAME, (*s).name),
                )),
                source_evr,
                name: owned(sys::pool_id2str(p, (*s).name)),
                evr: owned(sys::pool_id2str(p, (*s).evr)),
                arch: owned(sys::pool_id2str(p, (*s).arch)),
                repo: repo_name,
                installed,
                summary: owned(sys::solvable_lookup_str(s, sys::SOLVABLE_SUMMARY as i32)),
                source: owned(sys::solvable_lookup_sourcepkg(s)),
                license: owned(sys::solvable_lookup_str(s, sys::SOLVABLE_LICENSE as i32)),
                url: owned(sys::solvable_lookup_str(s, sys::SOLVABLE_URL as i32)),
                location: owned(sys::solvable_lookup_location(s, &mut medianr)),
                download_size: sys::solvable_lookup_num(s, sys::SOLVABLE_DOWNLOADSIZE as i32, 0),
                install_size: sys::solvable_lookup_num(s, sys::SOLVABLE_INSTALLSIZE as i32, 0),
                sha256,
            }
        }
    }

    /// The long description; separate from [`Pool::package`] because only
    /// `nx info` wants it.
    pub fn description(&self, id: SolvableId) -> String {
        // SAFETY: as in package().
        unsafe {
            let s = (*self.p()).solvables.add(id.0 as usize);
            owned(sys::solvable_lookup_str(
                s,
                sys::SOLVABLE_DESCRIPTION as i32,
            ))
        }
    }

    /// What a package provides or requires, each as rpm prints it
    /// (`libxkbcommon.so.0()(64bit)`, `pkgconfig(wayland-client) >= 1.20`).
    pub fn deps(&self, id: SolvableId, which: Deps) -> Vec<String> {
        let key = match which {
            Deps::Provides => sys::SOLVABLE_PROVIDES,
            Deps::Requires => sys::SOLVABLE_REQUIRES,
        };
        let mut q = Queue::new();
        // SAFETY: as in package(); the queue is initialised, and each
        // dep2str result is copied before the next call reuses its space.
        unsafe {
            let p = self.p();
            let s = (*p).solvables.add(id.0 as usize);
            sys::solvable_lookup_deparray(s, key as i32, &mut q.0, 0);
            q.ids()
                .filter(|&dep| dep != sys::SOLVABLE_PREREQMARKER as i32)
                .map(|dep| owned(sys::pool_dep2str(p, dep)))
                .collect()
        }
    }

    /// Compares two `[epoch:]version-release` strings the way rpm does.
    pub fn evr_cmp(&self, a: &str, b: &str) -> std::cmp::Ordering {
        let (Ok(a), Ok(b)) = (cstr(a), cstr(b)) else {
            return std::cmp::Ordering::Equal;
        };
        // SAFETY: live pool, valid C strings.
        let c = unsafe {
            sys::pool_evrcmp_str(self.p(), a.as_ptr(), b.as_ptr(), sys::EVRCMP_COMPARE as i32)
        };
        c.cmp(&0)
    }

    /// Solves installing `specs` on top of the installed set, which stays
    /// exactly as it is: nothing in it may be removed, upgraded or
    /// replaced. `hidden` packages are treated as if absent from the repos;
    /// `favored` ones win over other candidates (what is already locked).
    /// Weak dependencies are not pulled in.
    pub fn install(
        &mut self,
        specs: &[&str],
        hidden: &[SolvableId],
        favored: &[SolvableId],
    ) -> Result<Solution> {
        // Selections honour the considered map; the previous solve's hiding
        // must not decide what a spec matches.
        self.set_considered(&[]);
        let mut missing = Vec::new();
        for spec in specs {
            if self.select(spec)?.is_empty() {
                missing.push(format!("no package matches {spec}"));
            }
        }
        if !missing.is_empty() {
            return Ok(Solution::Problems(Problems::message(missing)));
        }
        let mut job = self.one_of_per_spec(specs)?;
        if let Some(installed) = self.installed {
            // SAFETY: the repo is live.
            let repoid = unsafe { (*self.repo_ptr(installed)).repoid };
            job.push2(
                (sys::SOLVER_LOCK | sys::SOLVER_SOLVABLE_REPO) as i32,
                repoid,
            );
        }
        for id in favored {
            job.push2((sys::SOLVER_FAVOR | sys::SOLVER_SOLVABLE) as i32, id.0);
        }
        self.set_considered(hidden);

        // SAFETY: live pool with whatprovides built (prepare); the solver
        // and transaction are freed by their guards before returning.
        unsafe {
            let solver = Solver(sys::solver_create(self.p()));
            sys::solver_set_flag(solver.0, sys::SOLVER_FLAG_IGNORE_RECOMMENDED as i32, 1);
            sys::solver_set_flag(solver.0, sys::SOLVER_FLAG_ALLOW_UNINSTALL as i32, 0);
            let nproblems = sys::solver_solve(solver.0, &mut job.0);
            if nproblems > 0 {
                let messages = (1..=nproblems)
                    .map(|i| owned(sys::solver_problem2str(solver.0, i)))
                    .collect();
                let mut involved = Vec::new();
                for i in 1..=nproblems {
                    self.involved(solver.0, i, &mut involved);
                }
                involved.sort();
                involved.dedup();
                return Ok(Solution::Problems(Problems { messages, involved }));
            }
            let trans = Transaction(sys::solver_create_transaction(solver.0));
            let mut adds = Vec::new();
            let mut changes = Vec::new();
            let steps = &(*trans.0).steps;
            for i in 0..steps.count {
                let p = *steps.elements.add(i as usize);
                let kind =
                    sys::transaction_type(trans.0, p, sys::SOLVER_TRANSACTION_SHOW_ACTIVE as i32)
                        as u32;
                match kind {
                    sys::SOLVER_TRANSACTION_INSTALL | sys::SOLVER_TRANSACTION_MULTIINSTALL => {
                        adds.push(self.package(SolvableId(p)));
                    }
                    sys::SOLVER_TRANSACTION_IGNORE => {}
                    _ => changes.push(self.package(SolvableId(p)).nevra()),
                }
            }
            if !changes.is_empty() {
                // The lock should make this impossible; refuse rather than
                // trust it.
                return Ok(Solution::Problems(Problems::message(
                    changes
                        .into_iter()
                        .map(|n| format!("the solution would change {n}, which is in the image"))
                        .collect(),
                )));
            }
            adds.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(Solution::Install(adds))
        }
    }

    /// Adds the packages `problem`'s rules name to `out`.
    ///
    /// # Safety
    /// `solver` is live and has just solved with problems.
    unsafe fn involved(&self, solver: *mut sys::Solver, problem: i32, out: &mut Vec<SolvableId>) {
        let mut rules = Queue::new();
        // SAFETY: per the contract; the queue is initialised.
        unsafe { sys::solver_findallproblemrules(solver, problem, &mut rules.0) };
        for rule in rules.ids() {
            let (mut source, mut target, mut dep) = (0, 0, 0);
            // SAFETY: rule ids come from the solver.
            unsafe { sys::solver_ruleinfo(solver, rule, &mut source, &mut target, &mut dep) };
            for id in [source, target] {
                if id > 0 {
                    out.push(SolvableId(id));
                }
            }
            if dep != 0 {
                out.extend(self.providers(dep));
            }
        }
    }

    /// Every package providing `dep`, as libsolv's inline
    /// `pool_whatprovides` finds them.
    fn providers(&self, dep: sys::Id) -> Vec<SolvableId> {
        const RELDEP: u32 = 0x8000_0000;
        let p = self.p();
        // SAFETY: the pool is prepared; offsets index whatprovidesdata,
        // whose lists end in 0. Rel ids index whatprovides_rel.
        unsafe {
            let cached = if (dep as u32) & RELDEP == 0 {
                *(*p).whatprovides.add(dep as usize)
            } else {
                *(*p).whatprovides_rel.add(((dep as u32) ^ RELDEP) as usize)
            };
            let off = if cached != 0 {
                cached as sys::Id
            } else {
                sys::pool_addrelproviders(p, dep)
            };
            let mut out = Vec::new();
            let mut at = (*p).whatprovidesdata.add(off as usize);
            while *at != 0 {
                out.push(SolvableId(*at));
                at = at.add(1);
            }
            out
        }
    }

    /// One "one of" install job per spec, so `nx add foo` installs one of
    /// its matches, not all of them.
    fn one_of_per_spec(&self, specs: &[&str]) -> Result<Queue> {
        let mut job = Queue::new();
        for spec in specs {
            let ids = self.select(spec)?;
            if ids.is_empty() {
                continue;
            }
            // SAFETY: live pool; the id list is a zero-terminated whatprovides
            // entry libsolv copies.
            let what = unsafe {
                let mut q = Queue::new();
                for id in &ids {
                    q.push(id.0);
                }
                sys::pool_queuetowhatprovides(self.p(), &mut q.0)
            };
            job.push2(
                (sys::SOLVER_INSTALL | sys::SOLVER_SOLVABLE_ONE_OF) as i32,
                what as i32,
            );
        }
        Ok(job)
    }

    /// Makes `hidden` packages invisible to the solver (pool->considered).
    fn set_considered(&mut self, hidden: &[SolvableId]) {
        // SAFETY: the map is allocated with libsolv's allocator so pool_free
        // can release it; every index is < nsolvables.
        unsafe {
            let p = self.p();
            if !(*p).considered.is_null() {
                sys::map_free((*p).considered);
                sys::solv_free((*p).considered.cast());
                (*p).considered = std::ptr::null_mut();
            }
            if hidden.is_empty() {
                return;
            }
            let n = (*p).nsolvables;
            let map = sys::solv_calloc(1, std::mem::size_of::<sys::Map>()).cast::<sys::Map>();
            sys::map_init(map, n);
            let bytes = std::slice::from_raw_parts_mut((*map).map, (*map).size as usize);
            bytes.fill(0xff);
            for id in hidden {
                let i = id.0 as usize;
                if let Some(b) = bytes.get_mut(i >> 3) {
                    *b &= !(1 << (i & 7));
                }
            }
            (*p).considered = map;
        }
    }

    fn last_error(&self) -> String {
        // SAFETY: live pool; pool_errstr returns its own buffer.
        unsafe { owned(sys::pool_errstr(self.p())) }
    }

    fn error(&self, what: String) -> Error {
        Error::Load {
            what,
            message: self.last_error(),
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: the pool is live and frees its repos and maps with it.
        unsafe { sys::pool_free(self.p()) };
    }
}

/// A libsolv queue, freed on drop.
struct Queue(sys::Queue);

impl Queue {
    fn new() -> Self {
        let mut q = std::mem::MaybeUninit::<sys::Queue>::uninit();
        // SAFETY: queue_init fully initialises the struct.
        unsafe {
            sys::queue_init(q.as_mut_ptr());
            Self(q.assume_init())
        }
    }

    fn push(&mut self, id: sys::Id) {
        // SAFETY: initialised queue.
        unsafe { sys::queue_insert(&mut self.0, self.0.count, id) };
    }

    fn push2(&mut self, a: sys::Id, b: sys::Id) {
        // SAFETY: initialised queue.
        unsafe { sys::queue_insert2(&mut self.0, self.0.count, a, b) };
    }

    fn ids(&self) -> impl Iterator<Item = sys::Id> + '_ {
        // SAFETY: elements holds count initialised ids.
        (0..self.0.count).map(|i| unsafe { *self.0.elements.add(i as usize) })
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        // SAFETY: initialised queue.
        unsafe { sys::queue_free(&mut self.0) };
    }
}

struct Solver(*mut sys::Solver);
impl Drop for Solver {
    fn drop(&mut self) {
        // SAFETY: created by solver_create, freed once.
        unsafe { sys::solver_free(self.0) };
    }
}

struct Transaction(*mut sys::Transaction);
impl Drop for Transaction {
    fn drop(&mut self) {
        // SAFETY: created by solver_create_transaction, freed once.
        unsafe { sys::transaction_free(self.0) };
    }
}

/// A C `FILE*`, closed on drop.
struct File(*mut sys::FILE);

impl File {
    /// Opens for reading, decompressing `.zst`, `.gz` and `.xz` on the fly.
    fn open(path: &Path) -> Result<Self> {
        let c = cpath(path)?;
        // SAFETY: valid C strings.
        let fp = unsafe { sys::solv_xfopen(c.as_ptr(), c"r".as_ptr()) };
        if fp.is_null() {
            return Err(Error::Open {
                path: path.to_owned(),
            });
        }
        Ok(Self(fp))
    }

    fn create(path: &Path) -> Result<Self> {
        let c = cpath(path)?;
        // SAFETY: valid C strings.
        let fp = unsafe { libc::fopen(c.as_ptr(), c"w".as_ptr()) };
        if fp.is_null() {
            return Err(Error::Open {
                path: path.to_owned(),
            });
        }
        Ok(Self(fp))
    }

    /// Closes, reporting whether buffered writes reached the file.
    fn close(mut self) -> bool {
        let fp = std::mem::replace(&mut self.0, std::ptr::null_mut());
        // SAFETY: fp is open and is not used again.
        unsafe { libc::fclose(fp) == 0 }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: fp is open and is not used again.
            unsafe { libc::fclose(self.0) };
        }
    }
}

/// `foo@copr:owner:project` → (`foo`, `copr:owner:project`). Paths
/// (`/usr/lib/systemd/system/getty@.service`) and rich provides never name
/// a repo.
pub fn split_repo(spec: &str) -> Option<(&str, &str)> {
    if spec.starts_with('/') || spec.contains('(') {
        return None;
    }
    let (name, repo) = spec.rsplit_once('@')?;
    (!name.is_empty() && !repo.is_empty()).then_some((name, repo))
}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::Nul(s.to_owned()))
}

fn cpath(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| Error::Nul(p.display().to_string()))
}

/// Copies a C string libsolv returned; null is empty.
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
