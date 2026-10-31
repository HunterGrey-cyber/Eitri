//! Whether a directory can serve as a project boundary.
//!
//! The permission policy judges paths against the project root, and an allow there is only worth
//! something when nobody else can change what is inside. This module is the one test of that: the
//! gate ([`crate::permission_policy`]), the deferral to the CLI's classifier
//! (`eitri_core::agent_backend`) and the panel's note all ask it, so one root behaves one way
//! whichever of them looks at it and however the window was started.
//!
//! A directory is usable when it resolves, is not `/`, a kernel tree or `$HOME` and above, still
//! resolves to itself, is owned by the effective uid, and no other account can write to it. That
//! last part is [`open_to_others`]: world-writable never passes, and group-writable passes only
//! for the caller's own primary group when that group is private. A umask of 0002 gives
//! `drwxrwxr-x grey grey` checkouts, which are the caller's alone, while the same mode on a shared
//! group's tree is not.
//!
//! Private means the account files say so ([`private_group`]). `/etc/group` and `/etc/passwd` are the
//! only sources read: an NSS lookup can wait on a network, and the first caller may be the GTK
//! thread. So a group known only through NSS is never private, and a 0775 checkout on such a machine
//! is no boundary, which fails toward a card.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::permission_policy::is_in_a_kernel_tree;

/// Why a directory is no project boundary. In the order [`refusal`] asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotABoundary {
    Unresolvable,
    FilesystemRoot,
    KernelTree,
    NoLongerItself,
    HomeOrAbove,
    NotYours,
    OthersCanWrite,
}

impl NotABoundary {
    pub const ALL: [NotABoundary; 7] = [
        NotABoundary::Unresolvable,
        NotABoundary::FilesystemRoot,
        NotABoundary::KernelTree,
        NotABoundary::NoLongerItself,
        NotABoundary::HomeOrAbove,
        NotABoundary::NotYours,
        NotABoundary::OthersCanWrite,
    ];

    /// The gate's words for this refusal. None is in
    /// [`REPLACEABLE_BY_A_RULE`](crate::permission_policy::REPLACEABLE_BY_A_RULE): a saved rule says
    /// a command is fine, not that the directory around it can be trusted.
    pub fn reason(self) -> &'static str {
        match self {
            NotABoundary::Unresolvable => "the project root itself could not be resolved",
            NotABoundary::FilesystemRoot => "the project root is the filesystem root, which is no boundary",
            NotABoundary::KernelTree => {
                "the project root is in /proc, /sys or /dev, whose links lead somewhere different in each process"
            }
            NotABoundary::NoLongerItself => {
                "the project root no longer resolves to itself: it was moved, or a link now stands in its place"
            }
            NotABoundary::HomeOrAbove => "the project root is the home directory or above it, which is no boundary",
            NotABoundary::NotYours => "the project root belongs to another user, which is no boundary",
            NotABoundary::OthersCanWrite => "other users can write to the project root, which is no boundary",
        }
    }

    /// The same refusal in the words of the panel's note ([`note`]).
    pub fn why(self) -> &'static str {
        match self {
            NotABoundary::Unresolvable => "this folder could not be resolved",
            NotABoundary::FilesystemRoot => "the filesystem root",
            NotABoundary::KernelTree => "a kernel tree",
            NotABoundary::NoLongerItself => "this folder has moved",
            NotABoundary::HomeOrAbove => "your home directory or above it",
            NotABoundary::NotYours => "not your directory",
            NotABoundary::OthersCanWrite => "other users can write to it",
        }
    }
}

/// Who is asking: what [`open_to_others`] compares a directory's owner and group with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    /// `gid` is a group of this account alone ([`private_group`]).
    pub private_group: bool,
}

impl Identity {
    /// The effective uid and gid, the ones every file this process touches is checked against.
    /// The account files are read once: the gate asks for every call it judges, and an account
    /// that gains a member mid-session is one more thing a restart picks up.
    pub fn current() -> Identity {
        static CURRENT: OnceLock<Identity> = OnceLock::new();
        *CURRENT.get_or_init(|| {
            // SAFETY: neither call has preconditions, and neither can fail.
            let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
            let private_group = match (read_account_file("/etc/group"), read_account_file("/etc/passwd")) {
                (Some(group_file), Some(passwd_file)) => private_group(&group_file, &passwd_file, uid, gid),
                _ => false,
            };
            Identity {
                uid,
                gid,
                private_group,
            }
        })
    }
}

/// The most of an account file that is read. A real one is far smaller; the bound is there because
/// the first caller may be on the GTK thread.
const MAX_ACCOUNT_FILE: u64 = 1024 * 1024;

/// An account file's text, or `None` when it cannot be read whole: a file cut at the limit could
/// have lost the very line that names another member.
fn read_account_file(path: &str) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_ACCOUNT_FILE)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() < MAX_ACCOUNT_FILE as usize).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

/// Whether `gid` is a group of the account `uid` alone, by the text of `/etc/group` and
/// `/etc/passwd`: the group file has it, every line with that gid lists nobody but the caller (a
/// gid can have several lines, and a name is the caller's when any passwd line gives it `uid`), and
/// no account of another uid has it as primary group. An account or group the files do not know is
/// not private. A line that does not parse is skipped.
pub fn private_group(group_file: &str, passwd_file: &str, uid: u32, gid: u32) -> bool {
    let accounts: Vec<(&str, u32, u32)> = passwd_file.lines().filter_map(passwd_line).collect();
    let my_names: Vec<&str> = accounts
        .iter()
        .filter(|(_, account_uid, _)| *account_uid == uid)
        .map(|(name, ..)| *name)
        .collect();
    if my_names.is_empty() {
        return false;
    }
    if accounts
        .iter()
        .any(|(_, account_uid, primary)| *primary == gid && *account_uid != uid)
    {
        return false;
    }
    let mut listed = false;
    for (line_gid, members) in group_file.lines().filter_map(group_line) {
        if line_gid != gid {
            continue;
        }
        listed = true;
        let mut members = members.split(',').map(str::trim).filter(|name| !name.is_empty());
        if members.any(|name| !my_names.contains(&name)) {
            return false;
        }
    }
    listed
}

/// `name:password:uid:gid:...`, as (name, uid, gid).
fn passwd_line(line: &str) -> Option<(&str, u32, u32)> {
    let mut fields = line.split(':');
    let name = fields.next()?;
    let uid = fields.nth(1)?.parse().ok()?;
    let gid = fields.next()?.parse().ok()?;
    Some((name, uid, gid))
}

/// `name:password:gid:members`, as (gid, members). Anything past a fourth `:` stays in the members,
/// where it reads as a name that is not the caller's.
fn group_line(line: &str) -> Option<(u32, &str)> {
    let mut fields = line.splitn(4, ':');
    let gid = fields.nth(2)?.parse().ok()?;
    Some((gid, fields.next()?))
}

/// Whether an account other than `who` can change a directory with this owner, group and mode.
/// `mode` is `st_mode`, of the directory itself and never of what a link leads to.
pub fn open_to_others(owner: u32, group: u32, mode: u32, who: &Identity) -> Option<NotABoundary> {
    if owner != who.uid {
        return Some(NotABoundary::NotYours);
    }
    // The sticky bit exempts nothing: it stops others renaming your files, not adding their own.
    if mode & 0o002 != 0 {
        return Some(NotABoundary::OthersCanWrite);
    }
    if mode & 0o020 != 0 && !(group == who.gid && who.private_group) {
        return Some(NotABoundary::OthersCanWrite);
    }
    None
}

/// Why `root` is no project boundary, or `None` when it is one.
pub fn refusal(root: &Path, home: Option<&Path>, who: &Identity) -> Option<NotABoundary> {
    checked_root(root, home, who).err()
}

/// The canonical project root, or why it cannot serve as a boundary.
///
/// - It does not resolve on disk -- if the boundary is unknown, nothing can be judged against it.
/// - It IS the home directory, or an ancestor of it (`/` included). `project_root` falls back to
///   the process cwd when no directory is given (`eitri_core::project_root`), and the packaged
///   `.desktop` entry passes `%f`, which is empty when Eitri is started from the app menu -- so a
///   menu launch can make `$HOME` the "project", and then every `Read` of `~/.ssh/id_ed25519` and
///   every `cat .credentials/...` would count as "inside the project". Whether a menu launch really
///   starts with cwd `$HOME` was NOT observed; the check does not depend on it.
///
/// `home` unset or unresolvable still leaves the `/` check, since `/` is an ancestor of every home.
///
/// A root in `/proc`, `/sys` or `/dev` is no boundary either -- every path under it passes through
/// a tree whose links lead somewhere different in each process, which the policy's resolver would
/// refuse one path at a time anyway; this says why once.
///
/// Nor is a root that no longer resolves to ITSELF. `project_root` is the canonical path the
/// session was started in, and the CLI child's cwd; this used to canonicalize it again and take
/// whatever came back. Once a process able to write the root's parent renamed the project and left
/// a link to another directory in its place, that other directory became "the project" here, and
/// an absolute `Write` or `Read` into it passed as inside -- while the CLI still sat in the renamed
/// original, and a hook `allow` overrides the working-directory ask its own post-hook re-check
/// would have raised. Every path-judged call cards now until the root is back. It compares paths,
/// not the directory's identity: a real directory put in the root's place under the same name is
/// not caught, and needs no catching here, since the CLI addresses its own cwd by that same string
/// (`path.resolve` against it, and the `Bash` tool's `cwd`), so both sides then judge and act on
/// the same replacement. The comparison is `Path`'s own, component by component, so a trailing `/`
/// or a `.` in the middle is the same root.
///
/// Nor, last, is a root somebody else controls ([`open_to_others`]): another account could add the
/// file the policy has just called inside, or the `.git` it reads, between two calls. The test is
/// on the canonical path with `lstat`. Only the root itself is looked at, not its parents: what
/// someone able to write a parent can do to the root is the case above.
pub(crate) fn checked_root(project_root: &Path, home: Option<&Path>, who: &Identity) -> Result<PathBuf, NotABoundary> {
    let Ok(root) = project_root.canonicalize() else {
        return Err(NotABoundary::Unresolvable);
    };
    if root.parent().is_none() {
        return Err(NotABoundary::FilesystemRoot);
    }
    if is_in_a_kernel_tree(&root) {
        return Err(NotABoundary::KernelTree);
    }
    if root != project_root {
        return Err(NotABoundary::NoLongerItself);
    }
    if let Some(home) = home.and_then(|h| h.canonicalize().ok()) {
        if home.starts_with(&root) {
            return Err(NotABoundary::HomeOrAbove);
        }
    }
    let Ok(metadata) = root.symlink_metadata() else {
        return Err(NotABoundary::Unresolvable);
    };
    match open_to_others(metadata.uid(), metadata.gid(), metadata.mode(), who) {
        Some(refused) => Err(refused),
        None => Ok(root),
    }
}

/// The line the panel shows at a root that is no boundary.
pub fn note(refusal: NotABoundary) -> String {
    format!(
        "No project boundary here ({}): a Claude tab asks before every file and command, and a Codex tab starts \
         read-only, without the folder's configuration. Other agents keep their own rules.",
        refusal.why()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const ME: u32 = 1000;
    const MY_GID: u32 = 1000;
    const OTHER: u32 = 1001;
    const OTHER_GID: u32 = 1001;

    fn who(private_group: bool) -> Identity {
        Identity {
            uid: ME,
            gid: MY_GID,
            private_group,
        }
    }

    #[test]
    fn open_to_others_says_who_can_change_the_tree() {
        for private_group in [true, false] {
            assert_eq!(open_to_others(ME, MY_GID, 0o755, &who(private_group)), None);
            assert_eq!(open_to_others(ME, MY_GID, 0o700, &who(private_group)), None);
        }
        // The checkout a umask of 0002 makes: its group is the caller alone, or it is not.
        assert_eq!(open_to_others(ME, MY_GID, 0o775, &who(true)), None);
        assert_eq!(
            open_to_others(ME, MY_GID, 0o775, &who(false)),
            Some(NotABoundary::OthersCanWrite)
        );
        // A private primary group says nothing about a directory in some other group.
        assert_eq!(
            open_to_others(ME, OTHER_GID, 0o775, &who(true)),
            Some(NotABoundary::OthersCanWrite)
        );
        // Everyone can write here, and the sticky bit exempts nothing: it keeps others from
        // renaming what you made, not from making what you run.
        for private_group in [true, false] {
            for mode in [0o777, 0o1777, 0o757] {
                assert_eq!(
                    open_to_others(ME, MY_GID, mode, &who(private_group)),
                    Some(NotABoundary::OthersCanWrite),
                    "{mode:o}"
                );
            }
        }
        // Ownership is asked first: the owner can change the mode whenever they like.
        assert_eq!(
            open_to_others(OTHER, MY_GID, 0o755, &who(true)),
            Some(NotABoundary::NotYours)
        );
        assert_eq!(
            open_to_others(OTHER, MY_GID, 0o777, &who(true)),
            Some(NotABoundary::NotYours)
        );
    }

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
                          grey:x:1000:1000:Grey:/home/user:/bin/bash\n\
                          sam:x:1001:1001:Sam:/home/sam:/bin/bash\n";

    #[test]
    fn a_group_nobody_else_is_in_is_private() {
        assert!(private_group("grey:x:1000:\n", PASSWD, ME, MY_GID));
        assert!(private_group("grey:x:1000:grey\n", PASSWD, ME, MY_GID));
        assert!(private_group("grey:x:1000:grey,\n", PASSWD, ME, MY_GID));
    }

    #[test]
    fn a_group_that_lists_someone_else_is_not_private() {
        assert!(!private_group("grey:x:1000:sam\n", PASSWD, ME, MY_GID));
        assert!(!private_group("grey:x:1000:grey,sam\n", PASSWD, ME, MY_GID));
        assert!(!private_group("grey:x:1000:ghost\n", PASSWD, ME, MY_GID));
    }

    #[test]
    fn every_line_of_a_gid_is_read() {
        let two_lines = "grey:x:1000:grey\nwheel2:x:1000:sam\n";
        assert!(!private_group(two_lines, PASSWD, ME, MY_GID));
        let two_quiet_lines = "grey:x:1000:\nalias:x:1000:grey\n";
        assert!(private_group(two_quiet_lines, PASSWD, ME, MY_GID));
    }

    #[test]
    fn a_second_account_on_the_gid_makes_it_shared() {
        let passwd = format!("{PASSWD}daemon2:x:1002:1000::/nonexistent:/bin/false\n");
        assert!(!private_group("grey:x:1000:\n", &passwd, ME, MY_GID));
    }

    #[test]
    fn a_second_name_for_the_same_uid_is_still_the_caller() {
        let passwd = format!("{PASSWD}grey2:x:1000:1000::/home/user:/bin/bash\n");
        assert!(private_group("grey:x:1000:grey2\n", &passwd, ME, MY_GID));
    }

    #[test]
    fn an_account_the_files_do_not_know_is_not_private() {
        assert!(!private_group("other:x:2000:\n", PASSWD, ME, MY_GID), "gid absent");
        assert!(!private_group("grey:x:1000:\n", PASSWD, 4242, MY_GID), "uid absent");
        assert!(!private_group("", PASSWD, ME, MY_GID));
        assert!(!private_group("grey:x:1000:\n", "", ME, MY_GID));
    }

    #[test]
    fn a_malformed_line_is_skipped() {
        let group = "# a comment\n\nnot a group line\nbad:x:notanumber:sam\nshort:x\ngrey:x:1000:\n";
        let passwd = format!("# a comment\n\ngarbage\nbad:x:nan:1000::/:/bin/false\n{PASSWD}");
        assert!(private_group(group, &passwd, ME, MY_GID));
    }

    /// A line cut in two by the read limit could have lost the member that matters.
    #[test]
    fn an_account_file_cut_at_the_limit_is_not_read() {
        let dir = Scratch::new();
        let small = dir.path().join("small");
        std::fs::write(&small, "grey:x:1000:\n").unwrap();
        assert_eq!(
            read_account_file(small.to_str().unwrap()).as_deref(),
            Some("grey:x:1000:\n")
        );
        let large = dir.path().join("large");
        std::fs::write(&large, vec![b'#'; MAX_ACCOUNT_FILE as usize]).unwrap();
        assert_eq!(read_account_file(large.to_str().unwrap()), None);
        assert_eq!(read_account_file(dir.path().join("gone").to_str().unwrap()), None);
    }

    /// A directory of the test's own, canonical as a session's root always is.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("agent-project-boundary-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let dir = dir.canonicalize().unwrap();
            Self::chmod(&dir, 0o700);
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn chmod(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The caller as far as `dir` goes: the process's own uid and the directory's own group, which
    /// is not always the process's on macOS, where a new directory takes its parent's.
    fn owner_of(dir: &Path, private_group: bool) -> Identity {
        let metadata = dir.symlink_metadata().unwrap();
        Identity {
            uid: metadata.uid(),
            gid: metadata.gid(),
            private_group,
        }
    }

    #[test]
    fn your_own_private_directory_is_a_boundary() {
        let dir = Scratch::new();
        assert_eq!(refusal(dir.path(), None, &owner_of(dir.path(), false)), None);
    }

    #[test]
    fn the_filesystem_root_is_no_boundary() {
        assert_eq!(
            refusal(Path::new("/"), None, &who(true)),
            Some(NotABoundary::FilesystemRoot)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_kernel_tree_is_no_boundary() {
        assert_eq!(
            refusal(Path::new("/proc/self"), None, &who(true)),
            Some(NotABoundary::KernelTree)
        );
    }

    #[test]
    fn a_link_standing_in_for_the_root_is_no_boundary() {
        let dir = Scratch::new();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            refusal(&link, None, &owner_of(&real, true)),
            Some(NotABoundary::NoLongerItself)
        );
    }

    #[test]
    fn home_and_what_is_above_it_are_no_boundary() {
        let dir = Scratch::new();
        let below = dir.path().join("below");
        std::fs::create_dir(&below).unwrap();
        let who = owner_of(dir.path(), true);
        assert_eq!(
            refusal(dir.path(), Some(dir.path()), &who),
            Some(NotABoundary::HomeOrAbove)
        );
        assert_eq!(refusal(dir.path(), Some(&below), &who), Some(NotABoundary::HomeOrAbove));
        // Below home, or with no home to compare, the directory stands on its own.
        assert_eq!(refusal(&below, Some(dir.path()), &who), None);
        assert_eq!(
            refusal(dir.path(), Some(&dir.path().join("gone")), &who),
            None,
            "a home that does not resolve rules nothing out"
        );
    }

    #[test]
    fn a_path_that_does_not_resolve_is_no_boundary() {
        let dir = Scratch::new();
        assert_eq!(
            refusal(&dir.path().join("gone"), None, &who(true)),
            Some(NotABoundary::Unresolvable)
        );
    }

    #[test]
    fn someone_elses_directory_is_no_boundary() {
        let dir = Scratch::new();
        let mut someone_else = owner_of(dir.path(), true);
        someone_else.uid = someone_else.uid.wrapping_add(1);
        assert_eq!(refusal(dir.path(), None, &someone_else), Some(NotABoundary::NotYours));
    }

    #[test]
    fn a_directory_everyone_can_write_to_is_no_boundary() {
        for mode in [0o777, 0o1777] {
            let dir = Scratch::new();
            Scratch::chmod(dir.path(), mode);
            assert_eq!(
                refusal(dir.path(), None, &owner_of(dir.path(), true)),
                Some(NotABoundary::OthersCanWrite),
                "{mode:o}"
            );
        }
    }

    /// What umask 0002 makes of a checkout, and what a shared tree looks like, on real modes.
    #[test]
    fn a_group_writable_directory_stands_only_on_a_private_group() {
        let dir = Scratch::new();
        Scratch::chmod(dir.path(), 0o775);
        assert_eq!(refusal(dir.path(), None, &owner_of(dir.path(), true)), None);
        assert_eq!(
            refusal(dir.path(), None, &owner_of(dir.path(), false)),
            Some(NotABoundary::OthersCanWrite)
        );
    }

    /// A root refused for an older reason keeps saying so when it fails the ownership test as well.
    #[test]
    fn the_older_refusals_come_before_the_new_ones() {
        let dir = Scratch::new();
        Scratch::chmod(dir.path(), 0o777);
        let mut someone_else = owner_of(dir.path(), true);
        someone_else.uid = someone_else.uid.wrapping_add(1);
        assert_eq!(
            refusal(dir.path(), Some(dir.path()), &someone_else),
            Some(NotABoundary::HomeOrAbove)
        );
        assert_eq!(
            refusal(Path::new("/"), None, &someone_else),
            Some(NotABoundary::FilesystemRoot)
        );
    }

    #[test]
    fn the_checked_root_is_the_canonical_path() {
        let dir = Scratch::new();
        let who = owner_of(dir.path(), true);
        assert_eq!(checked_root(dir.path(), None, &who).as_deref(), Ok(dir.path()));
        let spelled = PathBuf::from(format!("{}/", dir.path().display()));
        assert_eq!(checked_root(&spelled, None, &who).as_deref(), Ok(dir.path()));
    }

    #[test]
    fn every_reason_is_distinct_and_note_names_why() {
        let reasons: std::collections::BTreeSet<_> = NotABoundary::ALL.iter().map(|r| r.reason()).collect();
        let whys: std::collections::BTreeSet<_> = NotABoundary::ALL.iter().map(|r| r.why()).collect();
        assert_eq!(reasons.len(), NotABoundary::ALL.len());
        assert_eq!(whys.len(), NotABoundary::ALL.len());
        for refused in NotABoundary::ALL {
            assert!(note(refused).contains(&format!("({})", refused.why())), "{refused:?}");
            assert!(
                !crate::permission_policy::REPLACEABLE_BY_A_RULE.contains(&refused.reason()),
                "{refused:?}"
            );
        }
        assert_eq!(
            note(NotABoundary::OthersCanWrite),
            "No project boundary here (other users can write to it): a Claude tab asks before every file and \
             command, and a Codex tab starts read-only, without the folder's configuration. Other agents keep their \
             own rules."
        );
        assert_eq!(
            NotABoundary::ALL.map(|r| r.why()),
            [
                "this folder could not be resolved",
                "the filesystem root",
                "a kernel tree",
                "this folder has moved",
                "your home directory or above it",
                "not your directory",
                "other users can write to it",
            ]
        );
    }

    /// The running process's own answer, whatever the machine: it is what every session asks.
    #[test]
    fn the_current_identity_is_this_process_and_stays_put() {
        let current = Identity::current();
        // SAFETY: neither call has preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        assert_eq!((current.uid, current.gid), (uid, gid));
        assert_eq!(Identity::current(), current);
    }
}
