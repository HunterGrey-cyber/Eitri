use super::*;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::sync::Mutex;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("eitri-browse-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn listing(&self, filter: Option<&str>) -> Listing {
        read(&RealFs, &self.0, filter, &local_mounts(), &mut |_| true)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn local_mounts() -> Mounts {
    Mounts::parse("1 0 0:1 / / rw - ext4 /dev/root rw\n")
}
fn parts(listing: Listing) -> (PathBuf, Vec<Entry>, Option<Cap>, Option<Entry>) {
    match listing {
        Listing::Read {
            dir,
            entries,
            capped,
            exact,
        } => (dir, entries, capped, exact),
        other => panic!("expected a listing, got {other:?}"),
    }
}
fn entry(raw: &str, kind: EntryKind) -> Entry {
    Entry {
        raw: raw.into(),
        name: raw.into(),
        kind,
        physical: Some(Path::new("/work").join(raw)),
    }
}

#[test]
fn lists_folders_and_dot_folders_and_counts_only_folders() {
    let s = Scratch::new();
    for name in ["v10", "v2", ".hidden"] {
        fs::create_dir(s.0.join(name)).unwrap();
    }
    for n in 0..2100 {
        fs::write(s.0.join(format!("file{n}")), b"").unwrap();
    }
    let (_, entries, capped, _) = parts(s.listing(None));
    let folders: Vec<_> = entries
        .iter()
        .filter(|e| !matches!(e.kind, EntryKind::File))
        .map(|e| e.raw.clone())
        .collect();
    assert_eq!(folders, [".hidden", "v2", "v10"]);
    // The files ride along and count against nothing: 2100 of them leave the folder uncapped.
    assert_eq!(entries.iter().filter(|e| e.kind == EntryKind::File).count(), 2100);
    assert_eq!(entries.len(), 2103);
    assert_eq!(capped, None);
}
#[test]
fn a_typed_file_name_is_in_an_uncapped_listing_without_a_second_read() {
    let s = Scratch::new();
    fs::write(s.0.join("notes.md"), b"").unwrap();
    for name in ["a", "b", "c"] {
        fs::create_dir(s.0.join(name)).unwrap();
    }
    let (_, entries, capped, _) = parts(s.listing(None));
    assert_eq!(capped, None);
    assert!(entries.iter().any(|e| e.raw == "notes.md" && e.kind == EntryKind::File));
    let (_, entries, _, exact) = parts(s.listing(Some("notes.md")));
    assert_eq!(entries.len(), 1);
    assert_eq!(exact.unwrap().kind, EntryKind::File);
}
#[test]
fn exactly_at_the_bounds_nothing_is_dropped_and_nothing_is_capped() {
    let mut f = Fake::new();
    for n in 0..MAX_FOLDERS {
        f.node(&format!("folder-{n}"), DType::Dir, Kind::Dir);
    }
    for n in 0..100 {
        f.node(&format!("file-{n}"), DType::File, Kind::File);
    }
    let (_, entries, capped, _) = parts(f.listing(&local_mounts()));
    assert_eq!(entries.len(), MAX_FOLDERS + 100);
    assert_eq!(capped, None);
    f.node("one-more", DType::Dir, Kind::Dir);
    let (_, entries, capped, _) = parts(f.listing(&local_mounts()));
    assert_eq!(capped, Some(Cap::Folders));
    assert_eq!(
        entries.iter().filter(|e| !matches!(e.kind, EntryKind::File)).count(),
        MAX_FOLDERS
    );

    let mut f = Fake::new();
    for n in 0..MAX_SCANNED {
        f.node(&format!("file-{n}"), DType::File, Kind::File);
    }
    let (_, entries, capped, _) = parts(f.listing(&local_mounts()));
    assert_eq!(entries.len(), MAX_SCANNED);
    assert_eq!(capped, None);
    f.node("one-more", DType::File, Kind::File);
    let (_, _, capped, _) = parts(f.listing(&local_mounts()));
    assert_eq!(capped, Some(Cap::Scanned));
}
#[test]
fn a_folder_of_3000_folders_keeps_2000_and_says_folders_capped() {
    let s = Scratch::new();
    for n in 0..3000 {
        fs::create_dir(s.0.join(format!("folder-{n}"))).unwrap();
    }
    let (_, entries, capped, _) = parts(s.listing(None));
    assert_eq!(entries.len(), 2000);
    assert_eq!(capped, Some(Cap::Folders));
}
#[test]
fn sixty_thousand_files_scan_50000_and_say_scanned() {
    let s = Scratch::new();
    for n in 0..60_000 {
        fs::write(s.0.join(format!("file-{n}")), b"").unwrap();
    }
    let (_, entries, capped, _) = parts(s.listing(None));
    assert_eq!(entries.len(), MAX_SCANNED);
    assert!(entries.iter().all(|e| e.kind == EntryKind::File));
    assert_eq!(capped, Some(Cap::Scanned));

    let mut f = Fake::new();
    for n in 0..60_000 {
        f.node(&format!("file-{n}"), DType::File, Kind::File);
    }
    parts(f.listing(&local_mounts()));
    // The entry after the bound is read only to learn that the folder has more.
    assert_eq!(*f.scanned.lock().unwrap(), MAX_SCANNED + 1);
}
#[test]
fn a_filter_rereads_a_capped_folder_and_finds_a_folder_past_the_cap() {
    let s = Scratch::new();
    for n in 0..3000 {
        fs::create_dir(s.0.join(format!("folder-{n}"))).unwrap();
    }
    let (_, entries, _, _) = parts(s.listing(None));
    let unseen = (0..3000)
        .map(|n| format!("folder-{n}"))
        .find(|n| !entries.iter().any(|e| e.raw == n.as_str()))
        .unwrap();
    let (_, filtered, capped, _) = parts(s.listing(Some(&unseen)));
    assert!(filtered.iter().any(|e| e.raw == unseen.as_str()));
    assert_eq!(capped, None);
}
#[test]
fn the_exact_name_is_looked_up_when_the_listing_is_denied() {
    let s = Scratch::new();
    fs::create_dir(s.0.join("child")).unwrap();
    fs::write(s.0.join("file"), b"").unwrap();
    fs::set_permissions(&s.0, fs::Permissions::from_mode(0o711)).unwrap();
    assert!(matches!(s.listing(None), Listing::Read { .. }));
    // The owner can read a 0711 folder; remove its read bit to exercise a searchable, unreadable parent.
    fs::set_permissions(&s.0, fs::Permissions::from_mode(0o111)).unwrap();
    for (name, kind) in [
        ("child", EntryKind::Dir { mount_point: false }),
        ("file", EntryKind::File),
    ] {
        match s.listing(Some(name)) {
            Listing::Denied { exact: Some(e) } => {
                assert_eq!(e.raw, name);
                assert_eq!(e.kind, kind);
            }
            other => panic!("searchable unreadable folders keep the exact child: {other:?}"),
        }
    }
}
#[test]
fn the_exact_name_is_looked_up_past_the_cap() {
    let s = Scratch::new();
    for n in 0..3000 {
        fs::create_dir(s.0.join(format!("folder-{n}"))).unwrap();
    }
    let (_, _, capped, exact) = parts(s.listing(Some("folder-")));
    assert_eq!(capped, Some(Cap::Folders));
    assert_eq!(exact, None);
    // Every matching name keeps the folder bound full, including the exact name requested.
    fs::create_dir(s.0.join("folder")).unwrap();
    let (_, _, capped, exact) = parts(s.listing(Some("folder")));
    assert_eq!(capped, Some(Cap::Folders));
    assert_eq!(exact.unwrap().raw, "folder");

    let mut f = Fake::new();
    for n in 0..MAX_FOLDERS {
        f.node(&format!("folder-{n}"), DType::Dir, Kind::Dir);
    }
    f.node("folder", DType::Dir, Kind::Dir);
    let (_, entries, capped, exact) = parts(read(
        &f,
        Path::new("/work"),
        Some("folder"),
        &local_mounts(),
        &mut |_| true,
    ));
    assert_eq!(capped, Some(Cap::Folders));
    assert!(!entries.iter().any(|entry| entry.raw == "folder"));
    assert_eq!(exact.unwrap().raw, "folder");
    assert_eq!(f.calls().iter().filter(|call| *call == "lstat:/work/folder").count(), 1);
}
#[test]
fn a_link_to_a_file_is_link_kind_file_and_the_exact_row() {
    let s = Scratch::new();
    fs::write(s.0.join("notes"), b"").unwrap();
    symlink("notes", s.0.join("link")).unwrap();
    let (_, entries, _, _) = parts(s.listing(None));
    assert!(matches!(&entries[0].kind, EntryKind::Link { target, kind: LinkKind::Unknown } if target == "notes"));
    let (_, kinds) = later(
        &RealFs,
        &s.0,
        &entries,
        None,
        &local_mounts(),
        &mut |_| true,
        Instant::now() + MARK_BUDGET,
    );
    assert_eq!(kinds, vec![("link".into(), LinkKind::File)]);
    let (_, _, _, exact) = parts(s.listing(Some("link")));
    assert!(matches!(
        exact.unwrap().kind,
        EntryKind::Link {
            kind: LinkKind::File,
            ..
        }
    ));
}
#[test]
fn a_loop_reads_loop() {
    let s = Scratch::new();
    symlink("b", s.0.join("a")).unwrap();
    symlink("a", s.0.join("b")).unwrap();
    assert_eq!(
        read(&RealFs, &s.0.join("a"), None, &local_mounts(), &mut |_| true),
        Listing::Loop
    );
}
#[test]
fn a_missing_folder_reads_missing_and_a_file_reads_not_a_folder() {
    let s = Scratch::new();
    assert_eq!(
        read(&RealFs, &s.0.join("missing"), None, &local_mounts(), &mut |_| true),
        Listing::Missing
    );
    fs::write(s.0.join("file"), b"").unwrap();
    assert_eq!(
        read(&RealFs, &s.0.join("file"), None, &local_mounts(), &mut |_| true),
        Listing::NotAFolder
    );
}
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses file names that are not UTF-8; this test needs one"
)]
fn a_non_utf8_name_keeps_its_raw_bytes() {
    let s = Scratch::new();
    let raw = OsString::from_vec(b"bad\xff\n\xe2\x80\xae".to_vec());
    fs::create_dir(s.0.join(&raw)).unwrap();
    let (_, entries, _, _) = parts(s.listing(None));
    assert_eq!(entries[0].raw.as_bytes(), raw.as_bytes());
    assert_eq!(entries[0].name, "bad\u{fffd}\\u{a}\\u{202e}");
    assert_eq!(entries[0].physical, Some(s.0.join(raw)));
}

#[derive(Default)]
struct Fake {
    nodes: BTreeMap<PathBuf, Kind>,
    links: BTreeMap<PathBuf, PathBuf>,
    dirs: BTreeMap<PathBuf, Vec<(OsString, DType)>>,
    calls: Mutex<Vec<String>>,
    scanned: Mutex<usize>,
}
impl Fake {
    fn new() -> Self {
        let mut f = Self::default();
        for path in ["/", "/work", "/srv", "/srv/data"] {
            f.nodes.insert(path.into(), Kind::Dir);
        }
        f.dirs.insert("/work".into(), vec![]);
        f
    }
    fn node(&mut self, name: &str, dtype: DType, kind: Kind) {
        self.nodes.insert(Path::new("/work").join(name), kind);
        self.dirs
            .get_mut(Path::new("/work"))
            .unwrap()
            .push((name.into(), dtype));
    }
    fn link(&mut self, name: &str, target: &str) {
        self.node(name, DType::Link, Kind::Link);
        self.links.insert(Path::new("/work").join(name), target.into());
    }
    fn log(&self, what: &str, p: &Path) {
        self.calls.lock().unwrap().push(format!("{what}:{}", p.display()));
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn listing(&self, mounts: &Mounts) -> Listing {
        read(self, Path::new("/work"), None, mounts, &mut |_| true)
    }
}
impl Fs for Fake {
    fn lstat(&self, p: &Path) -> io::Result<Kind> {
        self.log("lstat", p);
        self.nodes
            .get(p)
            .copied()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }
    fn readlink(&self, p: &Path) -> io::Result<PathBuf> {
        self.log("readlink", p);
        self.links
            .get(p)
            .cloned()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))
    }
    fn read_dir(&self, p: &Path, each: &mut dyn FnMut(OsString, DType) -> bool) -> io::Result<()> {
        self.log("read_dir", p);
        for (name, dtype) in self
            .dirs
            .get(p)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EACCES))?
        {
            *self.scanned.lock().unwrap() += 1;
            if !each(name.clone(), *dtype) {
                break;
            }
        }
        Ok(())
    }
    fn mountinfo(&self) -> io::Result<String> {
        Ok(String::new())
    }
}
#[test]
fn real_fs_reports_the_raw_d_type_and_names_no_stat() {
    let s = Scratch::new();
    fs::create_dir(s.0.join("dir")).unwrap();
    fs::write(s.0.join("file"), b"").unwrap();
    symlink("file", s.0.join("link")).unwrap();
    let mut names = vec![];
    RealFs
        .read_dir(&s.0, &mut |name, dtype| {
            names.push((name, dtype));
            true
        })
        .unwrap();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    // The raw d_type of each entry, with no stat to refine it: a link stays a link.
    assert_eq!(
        names,
        vec![
            ("dir".into(), DType::Dir),
            ("file".into(), DType::File),
            ("link".into(), DType::Link),
        ]
    );
    assert_eq!(dtype(libc::DT_UNKNOWN), DType::Unknown);
    assert_eq!(dtype(libc::DT_FIFO), DType::Other);

    use crate::source_scan::{code_only, functions, impl_blocks, read_required};
    let code = code_only(&read_required("core/src/project_chooser/read.rs").text);
    let bodies: Vec<_> = impl_blocks(&code, "RealFs")
        .into_iter()
        .flat_map(|block| functions(block, "read_dir"))
        .collect();
    assert_eq!(bodies.len(), 1);
    for stat in [
        "symlink_metadata",
        "metadata",
        "file_type",
        "read_link",
        "DirEntry",
        "fstatat",
        "lstat",
    ] {
        assert!(!bodies[0].contains(stat), "RealFs::read_dir must not call {stat}");
    }
    assert!(bodies[0].contains("readdir64"));
}
#[test]
fn mounts_match_by_component() {
    let m = Mounts::parse("1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /mnt/nas rw - nfs4 server rw\n");
    assert_eq!(m.of(Path::new("/mnt/nas/x")), Path::new("/mnt/nas"));
    assert_eq!(m.of(Path::new("/mnt/nas2")), Path::new("/"));
    assert!(!m.local(Path::new("/mnt/nas")));
    assert!(m.local(Path::new("/mnt/nas2")));
    assert!(!Mounts::parse("").local(Path::new("/")));
}
#[test]
fn mountinfo_escapes_are_decoded() {
    let m = Mounts::parse("2 1 0:2 / /a\\040b\\134c\\011d\\012e rw - tmpfs none rw\n");
    assert!(m.is_point(Path::new("/a b\\c\td\ne")));
    assert!(m.local(Path::new("/a b\\c\td\ne/x")));
}
#[test]
fn a_mount_point_under_a_linked_parent_is_found_by_its_physical_path() {
    let mut f = Fake::new();
    f.nodes.insert("/work/data".into(), Kind::Link);
    f.links.insert("/work/data".into(), "/srv/data".into());
    f.dirs.insert("/srv/data".into(), vec![("nas".into(), DType::Unknown)]);
    let m = Mounts::parse("1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /srv/data/nas rw - nfs4 server rw\n");
    let (dir, entries, _, _) = parts(read(&f, Path::new("/work/data"), None, &m, &mut |_| true));
    assert_eq!(dir, Path::new("/srv/data"));
    assert_eq!(entries[0].kind, EntryKind::Dir { mount_point: true });
    assert_eq!(entries[0].physical, Some("/srv/data/nas".into()));
    f.calls.lock().unwrap().clear();
    assert_eq!(
        later(
            &f,
            &dir,
            &entries,
            None,
            &m,
            &mut |_| true,
            Instant::now() + MARK_BUDGET
        ),
        (vec![], vec![])
    );
    assert!(f.calls().is_empty());
}
#[test]
fn lstat_of_a_mount_point_touches_that_mount() {
    let f = Fake::new();
    let m = Mounts::parse("1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /work rw - nfs4 server rw\n");
    let mut touched = vec![];
    resolve(&f, Path::new("/work"), &m, &mut |p| {
        touched.push(p.to_path_buf());
        true
    })
    .unwrap();
    assert_eq!(touched.last().unwrap(), Path::new("/work"));
}
#[test]
fn touch_sees_each_calls_mount_before_the_call() {
    let mut f = Fake::new();
    f.link("link", "child");
    f.node("child", DType::Unknown, Kind::Dir);
    let m = local_mounts();
    read(&f, Path::new("/work"), Some("link"), &m, &mut |p| {
        f.log("touch", p);
        true
    });
    let calls = f.calls();
    for (i, call) in calls.iter().enumerate().filter(|(_, c)| !c.starts_with("touch:")) {
        let (_, path) = call.split_once(':').unwrap();
        assert_eq!(calls[i - 1], format!("touch:{}", m.of(Path::new(path)).display()));
    }
}
#[test]
fn a_false_touch_stops_the_read_as_stuck() {
    let f = Fake::new();
    assert_eq!(
        read(&f, Path::new("/work"), None, &local_mounts(), &mut |_| false),
        Listing::Stuck
    );
    assert!(f.calls().is_empty());
    let mut f = Fake::new();
    f.node("unknown", DType::Unknown, Kind::Dir);
    assert_eq!(
        read(&f, Path::new("/work"), None, &local_mounts(), &mut |_| !f
            .calls()
            .iter()
            .any(|c| c.starts_with("read_dir:"))),
        Listing::Stuck
    );
}
#[test]
fn dt_unknown_gets_one_lstat_and_never_on_a_mount_point() {
    let mut f = Fake::new();
    f.node("child", DType::Unknown, Kind::Dir);
    f.node("nas", DType::Unknown, Kind::Dir);
    let m = Mounts::parse("1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /work/nas rw - autofs none rw\n");
    parts(f.listing(&m));
    assert_eq!(f.calls().iter().filter(|c| *c == "lstat:/work/child").count(), 1);
    assert!(!f.calls().contains(&"lstat:/work/nas".into()));
}
#[test]
fn no_lstat_of_an_entry_itself_when_dtype_is_known() {
    let mut f = Fake::new();
    f.node("child", DType::Dir, Kind::Dir);
    f.node("file", DType::File, Kind::File);
    f.link("link", "child");
    parts(f.listing(&local_mounts()));
    for name in ["child", "file", "link"] {
        assert!(!f.calls().contains(&format!("lstat:/work/{name}")));
    }
}
#[test]
fn later_marks_outward_from_near() {
    let mut f = Fake::new();
    for name in ["v1", "v2", "v3", "v10"] {
        f.nodes.insert(format!("/work/{name}/.git").into(), Kind::File);
    }
    let entries = ["v10", "v3", "v1", "v2"].map(|name| entry(name, EntryKind::Dir { mount_point: false }));
    let (marks, _) = later(
        &f,
        Path::new("/work"),
        &entries,
        Some(OsStr::new("v2")),
        &local_mounts(),
        &mut |_| true,
        Instant::now() + MARK_BUDGET,
    );
    assert_eq!(marks, ["v2", "v1", "v3", "v10"].map(OsString::from));
}
#[test]
fn later_marks_only_on_allowlisted_types() {
    for kind in ["nfs4", "fuse.sshfs", "virtiofs", "autofs", "unknown"] {
        let f = Fake::new();
        let m = Mounts::parse(&format!("1 0 0:1 / / rw - {kind} root rw\n"));
        later(
            &f,
            Path::new("/work"),
            &[entry("dir", EntryKind::Dir { mount_point: false })],
            None,
            &m,
            &mut |_| true,
            Instant::now() + MARK_BUDGET,
        );
        assert!(f.calls().is_empty(), "{kind} is not allowlisted");
    }
    for kind in [
        "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "bcachefs", "zfs", "tmpfs",
    ] {
        assert!(Mounts::parse(&format!("1 0 0:1 / / rw - {kind} root rw\n")).local(Path::new("/work")));
    }
}
#[test]
fn later_stops_at_until() {
    let f = Fake::new();
    later(
        &f,
        Path::new("/work"),
        &[entry("dir", EntryKind::Dir { mount_point: false })],
        None,
        &local_mounts(),
        &mut |_| true,
        Instant::now(),
    );
    assert!(f.calls().is_empty());
}
#[test]
fn a_link_chain_into_a_non_local_mount_stays_unknown_and_no_call_crosses_it() {
    let mut f = Fake::new();
    f.link("a", "b");
    f.link("b", "/srv/data/nas/child");
    let (_, entries, _, _) = parts(f.listing(&local_mounts()));
    f.calls.lock().unwrap().clear();
    let m = Mounts::parse("1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /srv/data/nas rw - nfs4 server rw\n");
    let (_, kinds) = later(
        &f,
        Path::new("/work"),
        &entries,
        None,
        &m,
        &mut |_| true,
        Instant::now() + MARK_BUDGET,
    );
    assert!(kinds.is_empty() || kinds.iter().all(|(_, k)| *k == LinkKind::Unknown));
    assert!(!f.calls().iter().any(|c| c.contains("/srv/data/nas")));
}
#[test]
fn a_link_to_a_local_folder_becomes_dir() {
    let mut f = Fake::new();
    f.link("link", "child");
    f.node("child", DType::Dir, Kind::Dir);
    let (_, entries, _, _) = parts(f.listing(&local_mounts()));
    let (_, kinds) = later(
        &f,
        Path::new("/work"),
        &entries,
        None,
        &local_mounts(),
        &mut |_| true,
        Instant::now() + MARK_BUDGET,
    );
    assert_eq!(kinds, vec![("link".into(), LinkKind::Dir)]);
}
#[test]
fn later_resolves_a_link_from_its_own_folder_without_walking_the_ancestors() {
    let mut f = Fake::new();
    f.link("link", "child");
    f.node("child", DType::Dir, Kind::Dir);
    let (_, entries, _, _) = parts(f.listing(&local_mounts()));
    let entries: Vec<_> = entries.into_iter().filter(|e| e.raw == "link").collect();
    f.calls.lock().unwrap().clear();
    let mut touched = 0;
    let (_, kinds) = later(
        &f,
        Path::new("/work"),
        &entries,
        None,
        &local_mounts(),
        &mut |_| {
            touched += 1;
            true
        },
        Instant::now() + MARK_BUDGET,
    );
    assert_eq!(kinds, vec![("link".into(), LinkKind::Dir)]);
    assert_eq!(
        f.calls(),
        ["lstat:/work/link", "readlink:/work/link", "lstat:/work/child"]
    );
    assert_eq!(touched, 3);
}
#[test]
fn later_charges_link_calls_to_the_call_bound() {
    let mut f = Fake::new();
    f.node("data", DType::Dir, Kind::Dir);
    for n in 0..1000 {
        f.link(&format!("link-{n}"), "data");
    }
    let (_, entries, _, _) = parts(f.listing(&local_mounts()));
    f.calls.lock().unwrap().clear();
    let mut touched = 0;
    let (_, kinds) = later(
        &f,
        Path::new("/work"),
        &entries,
        None,
        &local_mounts(),
        &mut |_| {
            touched += 1;
            true
        },
        Instant::now() + Duration::from_secs(60),
    );
    assert_eq!(f.calls().len(), touched);
    assert!(touched <= MAX_FOLDERS, "{touched} calls");
    assert!(
        touched >= MAX_FOLDERS - 2,
        "the bound is used, not undershot: {touched}"
    );
    assert!(kinds.len() < 1000);
}
#[test]
fn later_makes_no_call_when_the_deadline_passed_while_touch_waited() {
    let mut f = Fake::new();
    f.node("child", DType::Dir, Kind::Dir);
    f.link("link", "child");
    let (_, entries, _, _) = parts(f.listing(&local_mounts()));
    f.calls.lock().unwrap().clear();
    let (marks, kinds) = later(
        &f,
        Path::new("/work"),
        &entries,
        None,
        &local_mounts(),
        &mut |_| {
            // Another reader held this mount past the deadline; touch answers yes only afterwards.
            std::thread::sleep(Duration::from_millis(60));
            true
        },
        Instant::now() + Duration::from_millis(30),
    );
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    assert!(marks.is_empty() && kinds.is_empty());
}
#[test]
fn a_file_at_a_mount_point_stays_a_file_and_a_folder_there_gets_no_stat() {
    let mut f = Fake::new();
    f.node("hosts", DType::File, Kind::File);
    f.node("nas", DType::Unknown, Kind::Dir);
    let m = Mounts::parse(
        "1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /work/hosts rw - ext4 root rw\n3 1 0:3 / /work/nas rw - nfs4 s rw\n",
    );
    let (_, entries, capped, exact) = parts(read(&f, Path::new("/work"), Some("hosts"), &m, &mut |_| true));
    assert_eq!(capped, None);
    assert_eq!(exact.unwrap().kind, EntryKind::File);
    assert!(entries.iter().any(|e| e.raw == "hosts" && e.kind == EntryKind::File));
    assert!(entries.iter().all(|e| e.raw != "nas"), "the filter keeps nas out");
    let (_, entries, _, _) = parts(f.listing(&m));
    assert!(entries
        .iter()
        .any(|e| e.raw == "nas" && e.kind == (EntryKind::Dir { mount_point: true })));
    assert!(!f.calls().iter().any(|c| c.starts_with("lstat:/work/nas")));
}
#[test]
fn resolve_stops_after_40_links() {
    let mut f = Fake::new();
    for n in 0..=40 {
        f.link(&format!("link{n}"), &format!("link{}", n + 1));
    }
    assert_eq!(
        resolve(&f, Path::new("/work/link0"), &local_mounts(), &mut |_| true),
        Err(Listing::Loop)
    );
    assert_eq!(f.calls().iter().filter(|c| c.starts_with("readlink:")).count(), 40);
    f.nodes.insert("/work/link40".into(), Kind::Dir);
    assert_eq!(
        resolve(&f, Path::new("/work/link0"), &local_mounts(), &mut |_| true).unwrap(),
        Path::new("/work/link40")
    );
}

#[test]
fn exact_files_and_file_links_survive_the_scan_bound() {
    for linked in [false, true] {
        let mut f = Fake::new();
        for n in 0..MAX_SCANNED {
            f.node(&format!("needle-{n}"), DType::File, Kind::File);
        }
        if linked {
            f.link("needle", "target");
            f.nodes.insert("/work/target".into(), Kind::File);
        } else {
            f.node("needle", DType::File, Kind::File);
        }
        let (_, entries, capped, exact) = parts(read(
            &f,
            Path::new("/work"),
            Some("needle"),
            &local_mounts(),
            &mut |_| true,
        ));
        assert_eq!(entries.len(), MAX_SCANNED);
        assert!(entries.iter().all(|e| e.kind == EntryKind::File));
        assert_eq!(capped, Some(Cap::Scanned));
        let exact = exact.unwrap();
        assert_eq!(exact.raw, "needle");
        if linked {
            assert!(matches!(
                exact.kind,
                EntryKind::Link {
                    kind: LinkKind::File,
                    ..
                }
            ));
            assert_eq!(exact.physical, Some("/work/target".into()));
        } else {
            assert_eq!(exact.kind, EntryKind::File);
            assert_eq!(f.calls().iter().filter(|c| *c == "lstat:/work/needle").count(), 1);
        }
    }
}

#[test]
fn cancellation_keeps_only_the_marks_already_completed() {
    let mut f = Fake::new();
    f.nodes.insert("/work/a/.git".into(), Kind::Dir);
    f.nodes.insert("/work/b/.git".into(), Kind::File);
    let entries = ["a", "b"].map(|name| entry(name, EntryKind::Dir { mount_point: false }));
    let mut touches = 0;
    let result = later(
        &f,
        Path::new("/work"),
        &entries,
        None,
        &local_mounts(),
        &mut |_| {
            touches += 1;
            touches == 1
        },
        Instant::now() + MARK_BUDGET,
    );
    assert_eq!(result, (vec!["a".into()], vec![]));
    assert_eq!(f.calls(), vec!["lstat:/work/a/.git"]);
}

#[test]
fn parent_components_are_applied_after_link_expansion() {
    let mut f = Fake::new();
    f.link("link", "/srv/data");
    f.nodes.insert("/srv/sibling".into(), Kind::Dir);
    assert_eq!(
        resolve(&f, Path::new("/work/link/../sibling"), &local_mounts(), &mut |_| true).unwrap(),
        Path::new("/srv/sibling")
    );
    f.nodes.insert("/work/file".into(), Kind::File);
    assert_eq!(
        resolve(&f, Path::new("/work/file/../link"), &local_mounts(), &mut |_| true),
        Err(Listing::NotAFolder)
    );
    for target in ["file/.", "file/", "file/./../link"] {
        f.link("invalid", target);
        assert_eq!(
            resolve(&f, Path::new("/work/invalid"), &local_mounts(), &mut |_| true),
            Err(Listing::NotAFolder)
        );
    }
}

#[test]
fn a_non_local_link_does_not_stop_other_local_marks() {
    let mut f = Fake::new();
    f.link("a", "/nas/child");
    f.node("b", DType::Dir, Kind::Dir);
    f.nodes.insert("/work/b/.git".into(), Kind::File);
    let (_, entries, _, _) = parts(f.listing(&local_mounts()));
    f.calls.lock().unwrap().clear();
    let m = Mounts::parse("1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /nas rw - nfs4 server rw\n");
    let (marks, kinds) = later(
        &f,
        Path::new("/work"),
        &entries,
        None,
        &m,
        &mut |_| true,
        Instant::now() + MARK_BUDGET,
    );
    assert_eq!(marks, vec![OsString::from("b")]);
    assert!(kinds.is_empty());
    assert!(!f.calls().iter().any(|call| call.contains("/nas")));
}

#[test]
fn a_git_path_on_a_non_local_mount_is_never_touched() {
    let f = Fake::new();
    let m = Mounts::parse("1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /work/dir/.git rw - autofs none rw\n");
    let (marks, _) = later(
        &f,
        Path::new("/work"),
        &[entry("dir", EntryKind::Dir { mount_point: false })],
        None,
        &m,
        &mut |_| true,
        Instant::now() + MARK_BUDGET,
    );
    assert!(marks.is_empty());
    assert!(f.calls().is_empty());
}

#[test]
fn readlink_registers_its_source_mount_before_crossing_to_the_target() {
    let mut f = Fake::new();
    f.link("link", "/srv/data");
    let m = Mounts::parse(
        "1 0 0:1 / / rw - ext4 root rw\n2 1 0:2 / /work rw - tmpfs none rw\n3 1 0:3 / /srv/data rw - xfs device rw\n",
    );
    resolve(&f, Path::new("/work/link"), &m, &mut |mount| {
        f.log("touch", mount);
        true
    })
    .unwrap();
    let calls = f.calls();
    for (i, call) in calls.iter().enumerate().filter(|(_, c)| !c.starts_with("touch:")) {
        let (_, path) = call.split_once(':').unwrap();
        assert_eq!(calls[i - 1], format!("touch:{}", m.of(Path::new(path)).display()));
    }
    assert!(calls
        .windows(2)
        .any(|pair| pair == ["touch:/work", "readlink:/work/link"]));
    assert!(calls
        .windows(2)
        .any(|pair| pair == ["touch:/srv/data", "lstat:/srv/data"]));
}

#[test]
fn choosing_an_unknown_file_link_on_a_nonlocal_mount_opens_the_file() {
    use crate::project_chooser::{Chooser, Effect, Ev, Lang, LineEv, Mode};
    let mut fs = Fake::new();
    fs.link("link.md", "file.md");
    fs.node("file.md", DType::File, Kind::File);
    let mounts = Mounts::parse("1 0 0:1 / / rw - nfs4 server rw\n");
    let mut chooser = Chooser::new(Mode::Launch, None, Lang::En, 1);
    let mut effects = chooser.event(Ev::Line(LineEv::Changed("/work/link.md".into())));
    for round in 0..3 {
        let (serial, dir, filter) = effects
            .iter()
            .find_map(|e| match e {
                Effect::Read {
                    serial, dir, filter, ..
                } => Some((*serial, dir.clone(), filter.clone())),
                _ => None,
            })
            .expect("the choice must be read");
        let listing = read(&fs, &dir, filter.as_deref(), &mounts, &mut |_| true);
        if round < 2 {
            assert!(
                !fs.calls().iter().any(|call| call == "lstat:/work/file.md"),
                "classification must keep the mount allowlist"
            );
        } else {
            assert_eq!(listing, Listing::NotAFolder);
        }
        effects = chooser.event(Ev::Listing { serial, dir, listing });
        if round == 0 {
            effects = chooser.event(Ev::Line(LineEv::Enter));
        }
    }
    assert_eq!(effects, vec![Effect::Open("/work/link.md".into())]);
}
