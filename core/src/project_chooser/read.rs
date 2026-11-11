//! Blocking filesystem operations for folder browsing. Call only from a reader thread.

use std::cmp::Reverse;
use std::collections::{HashSet, VecDeque};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use super::listing::{natural_cmp, Cap, Entry, EntryKind, LinkKind, Listing};
use crate::onboarding::display::display_name;

pub const READ_SLOW: Duration = Duration::from_millis(150);
pub const READ_STUCK: Duration = Duration::from_secs(2);
pub const MARK_BUDGET: Duration = Duration::from_millis(200);
pub const MAX_SCANNED: usize = 50_000;
pub const MAX_FOLDERS: usize = 2000;
pub const READS_MAX: usize = 3;
pub const MAX_LINKS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DType {
    Dir,
    File,
    Link,
    Other,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Link,
    Other,
}

/// Each call may block. The caller registers its physical mount before entering it.
pub trait Fs: Send + Sync {
    fn lstat(&self, p: &Path) -> io::Result<Kind>;
    fn readlink(&self, p: &Path) -> io::Result<PathBuf>;
    fn read_dir(&self, p: &Path, each: &mut dyn FnMut(OsString, DType) -> bool) -> io::Result<()>;
    fn mountinfo(&self) -> io::Result<String>;
}

pub struct RealFs;

fn dtype(raw: u8) -> DType {
    match raw {
        libc::DT_DIR => DType::Dir,
        libc::DT_REG => DType::File,
        libc::DT_LNK => DType::Link,
        libc::DT_UNKNOWN => DType::Unknown,
        _ => DType::Other,
    }
}

struct DirStream(*mut libc::DIR);
impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: opendir gave this guard the sole ownership of the stream.
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: libc returns the calling thread's errno storage.
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: libc returns the calling thread's errno storage.
    unsafe { libc::__error() }
}

impl Fs for RealFs {
    fn lstat(&self, p: &Path) -> io::Result<Kind> {
        let kind = std::fs::symlink_metadata(p)?.file_type();
        Ok(if kind.is_dir() {
            Kind::Dir
        } else if kind.is_file() {
            Kind::File
        } else if kind.is_symlink() {
            Kind::Link
        } else {
            Kind::Other
        })
    }

    fn readlink(&self, p: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(p)
    }

    fn read_dir(&self, p: &Path, each: &mut dyn FnMut(OsString, DType) -> bool) -> io::Result<()> {
        let name = CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path contains a NUL byte"))?;
        // SAFETY: name is NUL-terminated and lives through the call.
        let stream = unsafe { libc::opendir(name.as_ptr()) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        let stream = DirStream(stream);
        loop {
            // SAFETY: this stream belongs to this thread; readdir's result is copied before the next call.
            let item = unsafe {
                *errno() = 0;
                #[cfg(target_os = "linux")]
                let item = libc::readdir64(stream.0);
                #[cfg(target_os = "macos")]
                let item = libc::readdir(stream.0);
                if item.is_null() {
                    let error = *errno();
                    return if error == 0 {
                        Ok(())
                    } else {
                        Err(io::Error::from_raw_os_error(error))
                    };
                }
                let name = CStr::from_ptr((*item).d_name.as_ptr()).to_bytes();
                (OsString::from_vec(name.to_vec()), dtype((*item).d_type))
            };
            if item.0 == "." || item.0 == ".." {
                continue;
            }
            if !each(item.0, item.1) {
                return Ok(());
            }
        }
    }

    fn mountinfo(&self) -> io::Result<String> {
        std::fs::read_to_string("/proc/self/mountinfo")
    }
}

pub struct Mounts {
    points: Vec<(PathBuf, String)>,
    paths: HashSet<PathBuf>,
}

fn unescape_mount(text: &str) -> OsString {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..=i + 3].iter().all(|b| matches!(b, b'0'..=b'7')) {
            let value =
                (bytes[i + 1] - b'0') as u16 * 64 + (bytes[i + 2] - b'0') as u16 * 8 + (bytes[i + 3] - b'0') as u16;
            if value <= u8::MAX as u16 {
                out.push(value as u8);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    OsString::from_vec(out)
}

impl Mounts {
    pub fn parse(text: &str) -> Mounts {
        let mut points = Vec::new();
        for line in text.lines() {
            let Some((before, after)) = line.split_once(" - ") else {
                continue;
            };
            let (Some(point), Some(kind)) = (before.split_whitespace().nth(4), after.split_whitespace().next()) else {
                continue;
            };
            let point = PathBuf::from(unescape_mount(point));
            if point.is_absolute() {
                points.push((point, kind.to_owned()));
            }
        }
        // A stacked mount's last entry wins when its path is identical.
        points.reverse();
        points.sort_by_key(|(point, _)| Reverse(point.components().count()));
        let paths = points.iter().map(|(point, _)| point.clone()).collect();
        Mounts { points, paths }
    }

    fn find(&self, physical: &Path) -> Option<&(PathBuf, String)> {
        self.points.iter().find(|(point, _)| physical.starts_with(point))
    }

    pub fn of(&self, physical: &Path) -> &Path {
        self.find(physical).map_or(Path::new("/"), |(point, _)| point.as_path())
    }

    pub fn local(&self, physical: &Path) -> bool {
        self.find(physical).is_some_and(|(_, kind)| {
            matches!(
                kind.as_str(),
                "ext2" | "ext3" | "ext4" | "btrfs" | "xfs" | "f2fs" | "bcachefs" | "zfs" | "tmpfs"
            )
        })
    }

    pub fn is_point(&self, physical: &Path) -> bool {
        self.paths.contains(physical)
    }
}

/// A call that answered with an error nobody named (EIO, ESTALE, a long name) reads as `Stuck`: the folder
/// shows as not answering although the call returned, and nothing will free it for a re-read.
fn failed(error: io::Error) -> Listing {
    match error.raw_os_error() {
        Some(libc::ENOENT) => Listing::Missing,
        Some(libc::ENOTDIR) => Listing::NotAFolder,
        Some(libc::ELOOP) => Listing::Loop,
        Some(libc::EACCES | libc::EPERM) => Listing::Denied { exact: None },
        _ => match error.kind() {
            io::ErrorKind::NotFound => Listing::Missing,
            io::ErrorKind::PermissionDenied => Listing::Denied { exact: None },
            _ => Listing::Stuck,
        },
    }
}

fn stat(fs: &dyn Fs, p: &Path, mounts: &Mounts, touch: &mut dyn FnMut(&Path) -> bool) -> Result<Kind, Listing> {
    if !touch(mounts.of(p)) {
        return Err(Listing::Stuck);
    }
    fs.lstat(p).map_err(failed)
}

fn target(fs: &dyn Fs, p: &Path, mounts: &Mounts, touch: &mut dyn FnMut(&Path) -> bool) -> Result<PathBuf, Listing> {
    if !touch(mounts.of(p)) {
        return Err(Listing::Stuck);
    }
    fs.readlink(p).map_err(failed)
}

fn components(p: &Path) -> VecDeque<OsString> {
    // Keep dot components and a trailing slash: `file/.` must fail even when reached through a link.
    let bytes = p.as_os_str().as_bytes();
    let mut parts = VecDeque::new();
    if p.is_absolute() {
        parts.push_back(OsString::from("/"));
    }
    for part in bytes.split(|byte| *byte == b'/').filter(|part| !part.is_empty()) {
        parts.push_back(OsString::from_vec(part.to_vec()));
    }
    if bytes.last() == Some(&b'/') {
        parts.push_back(OsString::from("."));
    }
    parts
}

fn resolve_kind(
    fs: &dyn Fs,
    p: &Path,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
) -> Result<(PathBuf, Kind), Listing> {
    if !p.is_absolute() {
        return Err(Listing::Missing);
    }
    let root = PathBuf::from("/");
    let kind = stat(fs, &root, mounts, touch)?;
    walk(fs, root, kind, components(p), mounts, touch)
}

/// Resolve one name inside `dir`, an already physical folder: nothing above it is touched again.
fn resolve_child(
    fs: &dyn Fs,
    dir: &Path,
    raw: &OsStr,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
) -> Result<(PathBuf, Kind), Listing> {
    walk(
        fs,
        dir.to_path_buf(),
        Kind::Dir,
        VecDeque::from([raw.to_owned()]),
        mounts,
        touch,
    )
}

fn walk(
    fs: &dyn Fs,
    start: PathBuf,
    start_kind: Kind,
    mut pending: VecDeque<OsString>,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
) -> Result<(PathBuf, Kind), Listing> {
    let mut physical = start;
    let mut kind = start_kind;
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if kind != Kind::Dir {
            return Err(Listing::NotAFolder);
        }
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => {
                physical = PathBuf::from("/");
            }
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                physical.pop();
            }
            Some(Component::Normal(_)) => {
                let next = physical.join(&part);
                kind = stat(fs, &next, mounts, touch)?;
                if kind == Kind::Link {
                    if links == MAX_LINKS {
                        return Err(Listing::Loop);
                    }
                    links += 1;
                    let link = target(fs, &next, mounts, touch)?;
                    if link.as_os_str().is_empty() {
                        return Err(Listing::Missing);
                    }
                    let mut expanded = components(&link);
                    expanded.append(&mut pending);
                    pending = expanded;
                    kind = Kind::Dir;
                } else {
                    physical = next;
                }
            }
            _ => return Err(Listing::Missing),
        }
    }
    Ok((physical, kind))
}

/// Walk links in userspace so every component is charged to its physical mount before it is touched.
pub fn resolve(
    fs: &dyn Fs,
    p: &Path,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
) -> Result<PathBuf, Listing> {
    resolve_kind(fs, p, mounts, touch).map(|(path, _)| path)
}

fn link_kind(kind: Kind) -> LinkKind {
    match kind {
        Kind::Dir => LinkKind::Dir,
        Kind::File => LinkKind::File,
        _ => LinkKind::Other,
    }
}

fn make_entry(
    fs: &dyn Fs,
    dir: &Path,
    raw: OsString,
    kind: Kind,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
) -> Result<Option<Entry>, Listing> {
    let physical = dir.join(&raw);
    let name = display_name(&raw.to_string_lossy());
    let kind = match kind {
        Kind::Dir => EntryKind::Dir {
            mount_point: mounts.is_point(&physical),
        },
        Kind::File => EntryKind::File,
        Kind::Link => {
            let target = match target(fs, &physical, mounts, touch) {
                Ok(target) => display_name(&target.to_string_lossy()),
                Err(Listing::Stuck) => return Err(Listing::Stuck),
                Err(_) => String::new(),
            };
            EntryKind::Link {
                target,
                kind: LinkKind::Unknown,
            }
        }
        Kind::Other => return Ok(None),
    };
    let physical = if matches!(kind, EntryKind::Link { .. }) {
        None
    } else {
        Some(physical)
    };
    Ok(Some(Entry {
        raw,
        name,
        kind,
        physical,
    }))
}

fn exact_entry(
    fs: &dyn Fs,
    dir: &Path,
    filter: &str,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
) -> Result<Option<Entry>, Listing> {
    // The filter is one raw name, never a display label or a path into another folder.
    if filter.is_empty() || filter == "." || filter == ".." || filter.contains('/') || filter.contains('\0') {
        return Ok(None);
    }
    let path = dir.join(filter);
    let kind = match stat(fs, &path, mounts, touch) {
        Ok(kind) => kind,
        Err(Listing::Stuck) => return Err(Listing::Stuck),
        Err(_) => return Ok(None),
    };
    let mut entry = make_entry(fs, dir, filter.into(), kind, mounts, touch)?;
    if let Some(entry) = &mut entry {
        classify_exact_link(fs, dir, entry, mounts, touch)?;
    }
    Ok(entry)
}

fn classify_exact_link(
    fs: &dyn Fs,
    dir: &Path,
    entry: &mut Entry,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
) -> Result<(), Listing> {
    if let EntryKind::Link { kind, .. } = &mut entry.kind {
        let mut stopped = false;
        let answer = resolve_child(fs, dir, &entry.raw, mounts, &mut |mount| {
            if !mounts.local(mount) {
                return false;
            }
            let allowed = touch(mount);
            stopped |= !allowed;
            allowed
        });
        if stopped {
            return Err(Listing::Stuck);
        }
        if let Ok((physical, resolved)) = answer {
            entry.physical = Some(physical);
            *kind = link_kind(resolved);
        }
    }
    Ok(())
}

/// Whether a listed name counts against the folder bound: a folder, a link (which may lead to one) or a
/// mount point whose parent did not say it is a file.
fn folder_like(dtype: DType, point: bool) -> bool {
    matches!(dtype, DType::Dir | DType::Link) || (point && dtype != DType::File)
}

/// Enumerate only this folder, retaining raw names for all later path operations.
pub fn read(
    fs: &dyn Fs,
    dir: &Path,
    filter: Option<&str>,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
) -> Listing {
    let (dir, kind) = match resolve_kind(fs, dir, mounts, touch) {
        Ok(answer) => answer,
        Err(answer) => return answer,
    };
    if kind != Kind::Dir {
        return Listing::NotAFolder;
    }
    if !touch(mounts.of(&dir)) {
        return Listing::Stuck;
    }
    let filter_folded = filter.unwrap_or_default().to_lowercase();
    let mut raw_entries = Vec::new();
    let mut scanned = 0;
    let mut folders = 0;
    let mut capped = None;
    // Metadata is deferred until read_dir returns: no nested call can replace its in-flight mount.
    // A bound is hit only when one more entry turns up past it, so a folder that holds exactly
    // as many as the bound allows is complete.
    let result = fs.read_dir(&dir, &mut |raw, dtype| {
        if raw == "." || raw == ".." {
            return true;
        }
        if scanned == MAX_SCANNED {
            capped = Some(Cap::Scanned);
            return false;
        }
        scanned += 1;
        if filter_folded.is_empty() || raw.to_string_lossy().to_lowercase().contains(&filter_folded) {
            if folder_like(dtype, mounts.is_point(&dir.join(&raw))) {
                if folders == MAX_FOLDERS {
                    capped = Some(Cap::Folders);
                    return false;
                }
                folders += 1;
            }
            raw_entries.push((raw, dtype));
        }
        true
    });
    if let Err(error) = result {
        return match failed(error) {
            Listing::Denied { .. } => match exact_entry(fs, &dir, filter.unwrap_or_default(), mounts, touch) {
                Ok(exact) => Listing::Denied { exact },
                Err(answer) => answer,
            },
            answer => answer,
        };
    }
    let mut entries = Vec::new();
    let mut exact = None;
    let mut kept = 0;
    for (raw, dtype) in raw_entries {
        let physical = dir.join(&raw);
        // A mount point is never entered to learn its type; a file bind-mounted over a name keeps the
        // type its parent reported.
        let point = mounts.is_point(&physical);
        let kind = match dtype {
            DType::Dir => Kind::Dir,
            DType::File => Kind::File,
            _ if point => Kind::Dir,
            DType::Link => Kind::Link,
            DType::Other => Kind::Other,
            DType::Unknown => match stat(fs, &physical, mounts, touch) {
                Ok(kind) => kind,
                Err(Listing::Stuck) => return Listing::Stuck,
                Err(_) => continue,
            },
        };
        let mut entry = match make_entry(fs, &dir, raw, kind, mounts, touch) {
            Ok(Some(entry)) => entry,
            Ok(None) => continue,
            Err(answer) => return answer,
        };
        if filter.is_some_and(|filter| entry.raw == filter) {
            if let Err(answer) = classify_exact_link(fs, &dir, &mut entry, mounts, touch) {
                return answer;
            }
            exact = Some(entry.clone());
        }
        // Files ride along and count against no bound; every other entry is a folder or a link.
        if !matches!(entry.kind, EntryKind::File) {
            if kept == MAX_FOLDERS {
                capped = Some(Cap::Folders);
                break;
            }
            kept += 1;
        }
        entries.push(entry);
    }
    if capped.is_some() && filter.is_some_and(|filter| !filter.is_empty()) {
        exact = match exact_entry(fs, &dir, filter.unwrap(), mounts, touch) {
            Ok(exact) => exact,
            Err(answer) => return answer,
        };
    }
    entries.sort_by(|a, b| natural_cmp(&a.name, &b.name).then_with(|| a.raw.cmp(&b.raw)));
    Listing::Read {
        dir,
        entries,
        capped,
        exact,
    }
}

/// Fill optional marks and link kinds, starting near the cursor and stopping between blocking calls.
pub fn later(
    fs: &dyn Fs,
    dir: &Path,
    entries: &[Entry],
    near: Option<&OsStr>,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
    until: Instant,
) -> (Vec<OsString>, Vec<(OsString, LinkKind)>) {
    later_with_clock(fs, dir, entries, near, mounts, touch, until, &Instant::now)
}

fn later_with_clock(
    fs: &dyn Fs,
    dir: &Path,
    entries: &[Entry],
    near: Option<&OsStr>,
    mounts: &Mounts,
    touch: &mut dyn FnMut(&Path) -> bool,
    until: Instant,
    now: &dyn Fn() -> Instant,
) -> (Vec<OsString>, Vec<(OsString, LinkKind)>) {
    let until = until.min(now() + MARK_BUDGET);
    let mut sorted: Vec<_> = entries.iter().collect();
    sorted.sort_by(|a, b| natural_cmp(&a.name, &b.name).then_with(|| a.raw.cmp(&b.raw)));
    let center = near
        .and_then(|near| sorted.iter().position(|e| e.raw == near))
        .unwrap_or(0);
    let mut order = vec![center];
    for distance in 1..sorted.len() {
        if let Some(left) = center.checked_sub(distance) {
            order.push(left);
        }
        if center + distance < sorted.len() {
            order.push(center + distance);
        }
    }
    let mut marks = Vec::new();
    let mut kinds = Vec::new();
    let mut calls = 0;
    let mut stopped = false;
    for index in order {
        if now() >= until || stopped || calls >= MAX_FOLDERS {
            break;
        }
        let Some(entry) = sorted.get(index) else { continue };
        let path = dir.join(&entry.raw);
        // Each permission is one call about to be made: it counts against the bound, and a wait inside
        // touch that outlasts the deadline cancels the call.
        let mut allowed = |mount: &Path| {
            if now() >= until || calls >= MAX_FOLDERS {
                stopped = true;
                return false;
            }
            if !mounts.local(mount) {
                return false;
            }
            let ok = touch(mount) && now() < until;
            stopped |= !ok;
            if ok {
                calls += 1;
            }
            ok
        };
        match &entry.kind {
            EntryKind::Dir { mount_point: false } if !mounts.is_point(&path) && mounts.local(&path) => {
                if stat(fs, &path.join(".git"), mounts, &mut allowed).is_ok() {
                    marks.push(entry.raw.clone());
                }
            }
            EntryKind::Link { .. } => {
                if let Ok((_, kind)) = resolve_child(fs, dir, &entry.raw, mounts, &mut allowed) {
                    kinds.push((entry.raw.clone(), link_kind(kind)));
                }
            }
            _ => {}
        }
    }
    (marks, kinds)
}

#[cfg(test)]
mod tests;
