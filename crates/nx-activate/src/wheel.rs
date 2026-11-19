//! Who counts as an administrator when nxd looks at who owns a file: the
//! members of group `wheel`, wherever the system keeps them. Local
//! accounts are read from `/etc`; for anyone else nxd asks the system
//! (`id -G`, which goes through NSS: SSSD, LDAP) so an account that exists
//! only there is recognised too.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;

use crate::Runner;

/// Answers "is this uid in `wheel`?", asking the system at most once per
/// uid.
pub struct Wheel<'a> {
    runner: &'a dyn Runner,
    local: Vec<u32>,
    gid: RefCell<Option<Option<u32>>>,
    asked: RefCell<HashMap<u32, bool>>,
}

impl<'a> Wheel<'a> {
    pub fn new(runner: &'a dyn Runner, root: &Path) -> Self {
        Self {
            runner,
            local: crate::wheel_uids(root),
            gid: RefCell::new(None),
            asked: RefCell::new(HashMap::new()),
        }
    }

    pub fn is_member(&self, uid: u32) -> bool {
        if self.local.contains(&uid) {
            return true;
        }
        if let Some(known) = self.asked.borrow().get(&uid) {
            return *known;
        }
        let member = self.ask(uid);
        self.asked.borrow_mut().insert(uid, member);
        member
    }

    fn ask(&self, uid: u32) -> bool {
        let Some(wheel) = self.wheel_gid() else {
            return false;
        };
        let uid = uid.to_string();
        let Ok(out) = self.runner.run("id", &[OsStr::new("-G"), OsStr::new(&uid)]) else {
            return false;
        };
        out.success
            && out
                .stdout
                .split_whitespace()
                .any(|g| g.parse() == Ok(wheel))
    }

    /// The gid of `wheel`, from NSS.
    fn wheel_gid(&self) -> Option<u32> {
        *self.gid.borrow_mut().get_or_insert_with(|| {
            let out = self
                .runner
                .run("getent", &[OsStr::new("group"), OsStr::new("wheel")])
                .ok()?;
            out.success
                .then(|| out.stdout.split(':').nth(2)?.trim().parse().ok())
                .flatten()
        })
    }
}
