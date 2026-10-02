//! What changed between two snapshots, or between a snapshot and the files on disk now.
//!
//! The file list comes from two git runs over the same pair of trees (`--numstat` for the line
//! counts, `--raw` for the kind of change and the file modes), joined on the path; the patch of a
//! single file comes from a third, and is parsed by [`parse_unified`] into hunks the panel can draw
//! row by row. Every run is the shadow's own isolated git ([`Shadow::git`]), so no diff driver,
//! text conversion or colour setting from the user's configuration reaches it, and each takes the
//! store's lock shared so a collection cannot remove a tree being compared.
//!
//! Comparing with the files on disk never diffs against a live index: a throwaway tree is written
//! first ([`Shadow::scratch_tree`]) and both sides are then trees.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use super::git::{self, GitError};
use super::shadow::{is_hex_id, remaining, Limits, Shadow, ShadowError, READ_TIMEOUT};

/// A mode of `160000` is a nested repository, which a tree records as a single commit entry.
const GITLINK_MODE: &str = "160000";

/// One side of a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    /// A snapshot's commit, or any tree id the shadow holds (what [`Shadow::scratch_tree`] returns).
    Snapshot(String),
    /// The files on disk now.
    WorkTree,
}

/// How a path differs between the two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
    /// The kind of file changed (a regular file became a symlink, or the reverse).
    TypeChanged,
}

/// One file that differs, as the overview lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Relative to the project root.
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// Lines added; 0 for a binary file or a nested repository.
    pub added: u32,
    /// Lines removed; 0 for a binary file or a nested repository.
    pub removed: u32,
    pub binary: bool,
    /// A nested repository: one commit entry, with no lines to show.
    pub nested: bool,
}

/// What one line of a hunk is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`, which belongs to the line above it.
    NoNewline,
}

/// One row of a hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// The line without its marker. A line ending in `\r` keeps it.
    pub text: String,
    /// The line's number before the change; `None` for an added line and for [`LineKind::NoNewline`].
    pub old_no: Option<u32>,
    /// The line's number after the change; `None` for a removed line and for [`LineKind::NoNewline`].
    pub new_no: Option<u32>,
}

/// One `@@` block of a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Its position in the patch, from 0.
    pub id: u32,
    /// The whole `@@ -a,b +c,d @@ section` line.
    pub header: String,
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<DiffLine>,
}

/// The patch of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: PathBuf,
    pub added: u32,
    pub removed: u32,
    /// Git reports the two versions as binary and differing: there are no hunks.
    pub binary: bool,
    pub new_file: bool,
    pub deleted_file: bool,
    /// `(old, new)` when the file mode changed (an executable bit, say).
    pub mode_change: Option<(String, String)>,
    pub hunks: Vec<Hunk>,
}

/// The files that differ between snapshot `from` and `to`, in path order. `from` is a snapshot's
/// commit id; with [`Side::WorkTree`] the files on disk are captured once into a throwaway tree and
/// that tree is compared.
pub fn changed_files(shadow: &Shadow, from: &str, to: &Side) -> Result<Vec<FileChange>, ShadowError> {
    let deadline = Instant::now() + READ_TIMEOUT;
    check_id(from)?;
    // Held from before the tree is written until the last read of it.
    let _lock = shadow.lock_shared_until(deadline)?;
    let to = resolve(shadow, to)?;

    let mut numstat = shadow.git(None);
    numstat.args([
        "diff",
        "--no-renames",
        "--numstat",
        "-z",
        "--no-ext-diff",
        "--no-textconv",
    ]);
    numstat.arg(from).arg(&to).arg("--");
    let numstat = diff_output("diff --numstat", numstat, from, &to, deadline)?;

    let mut raw = shadow.git(None);
    raw.args(["diff", "--no-renames", "--raw", "-z", "--no-ext-diff", "--no-textconv"]);
    raw.arg(from).arg(&to).arg("--");
    let raw = diff_output("diff --raw", raw, from, &to, deadline)?;

    Ok(join_changes(&parse_numstat(&numstat), &parse_raw(&raw)))
}

/// The patch of `path` (relative to the project root) between `from` and `to`, parsed. `None` when
/// the patch is longer than `cap` lines (context lines count): a file that large is refused, never
/// cut short. A path that does not differ gives a [`FileDiff`] with no hunks.
pub fn file_hunks(
    shadow: &Shadow,
    from: &str,
    to: &Side,
    path: &Path,
    cap: usize,
) -> Result<Option<FileDiff>, ShadowError> {
    let deadline = Instant::now() + READ_TIMEOUT;
    check_id(from)?;
    check_path(path)?;
    let _lock = shadow.lock_shared_until(deadline)?;
    let to = resolve(shadow, to)?;

    // `top` and `literal`: the path names one file, whatever characters it holds.
    let mut spec = std::ffi::OsString::from(":(top,literal)");
    spec.push(path.as_os_str());
    let mut cmd = shadow.git(None);
    cmd.args([
        "diff",
        "--no-renames",
        "-U3",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
    ]);
    cmd.arg(from).arg(&to).arg("--").arg(spec);
    let output = diff_output("diff", cmd, from, &to, deadline)?;

    let text = String::from_utf8_lossy(&output);
    let header = parse_header(&text);
    let hunks = parse_unified(&text);
    let shown: usize = hunks.iter().map(|h| h.lines.len()).sum();
    if shown > cap {
        return Ok(None);
    }
    let count = |kind| {
        hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == kind)
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    };
    Ok(Some(FileDiff {
        path: path.to_path_buf(),
        added: count(LineKind::Added),
        removed: count(LineKind::Removed),
        binary: header.binary,
        new_file: header.new_file,
        deleted_file: header.deleted_file,
        mode_change: header.mode_change,
        hunks,
    }))
}

/// A tree-ish git can compare: the snapshot given, or the files on disk written out as a tree.
fn resolve(shadow: &Shadow, side: &Side) -> Result<String, ShadowError> {
    match side {
        Side::Snapshot(id) => {
            check_id(id)?;
            Ok(id.clone())
        }
        Side::WorkTree => shadow.scratch_tree(&Limits::default()),
    }
}

fn check_id(id: &str) -> Result<(), ShadowError> {
    if is_hex_id(id) {
        Ok(())
    } else {
        Err(ShadowError::NoSuchCommit(id.to_owned()))
    }
}

/// A path that stays inside the project: relative, non-empty and without `..`.
fn check_path(path: &Path) -> Result<(), ShadowError> {
    let ok = !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if ok {
        Ok(())
    } else {
        Err(ShadowError::InvalidPath(path.to_path_buf()))
    }
}

/// Runs a diff and returns its stdout. A side that is not an object of the store is
/// [`ShadowError::NoSuchCommit`], not a bare git failure.
fn diff_output(
    what: &str,
    cmd: std::process::Command,
    from: &str,
    to: &str,
    deadline: Instant,
) -> Result<Vec<u8>, ShadowError> {
    let output = git::run(cmd, remaining(deadline)?)?;
    match GitError::check(what, output) {
        Ok(output) => Ok(output.stdout),
        Err(GitError::Failed { stderr, .. })
            if stderr.contains("bad object") || stderr.contains("not a valid object name") =>
        {
            let missing = if stderr.contains(from) || !stderr.contains(to) {
                from
            } else {
                to
            };
            Err(ShadowError::NoSuchCommit(missing.to_owned()))
        }
        Err(e) => Err(e.into()),
    }
}

/// `-z` output cut at its NULs, without the empty piece after the last one.
fn nul_fields(bytes: &[u8]) -> Vec<&[u8]> {
    let mut fields: Vec<&[u8]> = bytes.split(|&b| b == 0).collect();
    if fields.last().is_some_and(|f| f.is_empty()) {
        fields.pop();
    }
    fields
}

/// `(path, added, removed)`; `None` counts are git's `-` for a binary file.
type NumstatRow<'a> = (&'a [u8], Option<u32>, Option<u32>);

fn parse_numstat(bytes: &[u8]) -> Vec<NumstatRow<'_>> {
    nul_fields(bytes)
        .into_iter()
        .filter_map(|record| {
            let mut parts = record.splitn(3, |&b| b == b'\t');
            let added = parts.next()?;
            let removed = parts.next()?;
            let path = parts.next()?;
            let count = |field: &[u8]| std::str::from_utf8(field).ok()?.parse::<u32>().ok();
            Some((path, count(added), count(removed)))
        })
        .collect()
}

/// `(path, status, old mode, new mode)` of every `--raw -z` entry.
type RawRow<'a> = (&'a [u8], u8, &'a [u8], &'a [u8]);

fn parse_raw(bytes: &[u8]) -> Vec<RawRow<'_>> {
    let fields = nul_fields(bytes);
    let mut rows = Vec::new();
    let mut i = 0;
    while i + 1 < fields.len() {
        // `:<old mode> <new mode> <old id> <new id> <status>`, then the path as its own field.
        let meta = fields[i];
        let mut parts = meta.strip_prefix(b":").unwrap_or(meta).split(|&b| b == b' ');
        let (old_mode, new_mode) = (parts.next().unwrap_or(b""), parts.next().unwrap_or(b""));
        let status = parts.nth(2).and_then(|s| s.first().copied()).unwrap_or(b'M');
        rows.push((fields[i + 1], status, old_mode, new_mode));
        i += 2;
    }
    rows
}

fn join_changes(numstat: &[NumstatRow<'_>], raw: &[RawRow<'_>]) -> Vec<FileChange> {
    raw.iter()
        .map(|&(path, status, old_mode, new_mode)| {
            let counts = numstat.iter().find(|row| row.0 == path);
            let binary = counts.is_some_and(|row| row.1.is_none() && row.2.is_none());
            let nested = old_mode == GITLINK_MODE.as_bytes() || new_mode == GITLINK_MODE.as_bytes();
            // A nested repository's "lines" are two commit ids; they are not content to count.
            let (added, removed) = match counts {
                Some(&(_, added, removed)) if !nested => (added.unwrap_or(0), removed.unwrap_or(0)),
                _ => (0, 0),
            };
            FileChange {
                path: PathBuf::from(OsStr::from_bytes(path)),
                kind: match status {
                    b'A' => ChangeKind::Added,
                    b'D' => ChangeKind::Deleted,
                    b'T' => ChangeKind::TypeChanged,
                    _ => ChangeKind::Modified,
                },
                added,
                removed,
                binary,
                nested,
            }
        })
        .collect()
}

/// What a patch's header lines (the ones before its first `@@`) say.
#[derive(Debug, Default, PartialEq, Eq)]
struct Header {
    binary: bool,
    new_file: bool,
    deleted_file: bool,
    mode_change: Option<(String, String)>,
}

fn parse_header(text: &str) -> Header {
    let mut header = Header::default();
    let (mut old_mode, mut new_mode) = (None, None);
    for line in text.split('\n') {
        if line.starts_with("@@ ") {
            break;
        }
        if line.starts_with("Binary files ") && line.ends_with(" differ") {
            header.binary = true;
        } else if line.starts_with("new file mode ") {
            header.new_file = true;
        } else if line.starts_with("deleted file mode ") {
            header.deleted_file = true;
        } else if let Some(mode) = line.strip_prefix("old mode ") {
            old_mode = Some(mode.trim_end().to_owned());
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            new_mode = Some(mode.trim_end().to_owned());
        }
    }
    header.mode_change = old_mode.zip(new_mode);
    header
}

/// The `(start, length)` of a range such as `830,6`; a bare `830` is one line long.
fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

/// `@@ -a,b +c,d @@ section` into `(a, b, c, d)`.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split_once(" @@").map(|(new, _)| new)?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    Some((old_start, old_len, new_start, new_len))
}

/// A hunk being read: how many lines it still owes on each side, and where the next ones fall.
struct Open {
    hunk: Hunk,
    old_left: u32,
    new_left: u32,
    next_old: u32,
    next_new: u32,
}

/// The hunks of a unified diff of one file.
///
/// The header lines before the first `@@` (`diff --git`, `index`, mode lines, `---`/`+++`,
/// `Binary files ... differ`) are skipped. Inside a hunk the counts in its `@@` line say where it
/// ends, so a removed line that reads `-- x` is never mistaken for a file header. A
/// `\ No newline at end of file` line becomes a [`LineKind::NoNewline`] row after the line it
/// qualifies, and carriage returns stay in the text.
pub fn parse_unified(text: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut open: Option<Open> = None;
    let text = text.strip_suffix('\n').unwrap_or(text);
    for line in text.split('\n') {
        if let Some(o) = open.as_mut() {
            if line.starts_with('\\') {
                o.hunk.lines.push(DiffLine {
                    kind: LineKind::NoNewline,
                    text: line.trim_start_matches('\\').trim().to_owned(),
                    old_no: None,
                    new_no: None,
                });
                continue;
            }
            if o.old_left > 0 || o.new_left > 0 {
                let (marker, body) = match line.chars().next() {
                    Some(c @ (' ' | '+' | '-')) => (c, &line[1..]),
                    // A context line whose leading space was lost on the way.
                    None => (' ', ""),
                    Some(_) => ('?', line),
                };
                match marker {
                    ' ' if o.old_left > 0 && o.new_left > 0 => {
                        o.hunk.lines.push(DiffLine {
                            kind: LineKind::Context,
                            text: body.to_owned(),
                            old_no: Some(o.next_old),
                            new_no: Some(o.next_new),
                        });
                        o.next_old += 1;
                        o.next_new += 1;
                        o.old_left -= 1;
                        o.new_left -= 1;
                        continue;
                    }
                    '-' if o.old_left > 0 => {
                        o.hunk.lines.push(DiffLine {
                            kind: LineKind::Removed,
                            text: body.to_owned(),
                            old_no: Some(o.next_old),
                            new_no: None,
                        });
                        o.next_old += 1;
                        o.old_left -= 1;
                        continue;
                    }
                    '+' if o.new_left > 0 => {
                        o.hunk.lines.push(DiffLine {
                            kind: LineKind::Added,
                            text: body.to_owned(),
                            old_no: None,
                            new_no: Some(o.next_new),
                        });
                        o.next_new += 1;
                        o.new_left -= 1;
                        continue;
                    }
                    // Not a line this hunk can still take: the hunk is malformed, so it ends here.
                    _ => {}
                }
            }
        }
        // Past the body of the open hunk (or in the file header): a new `@@` starts the next one,
        // anything else closes the hunk.
        if let Some((old_start, old_len, new_start, new_len)) = parse_hunk_header(line) {
            if let Some(done) = open.take() {
                hunks.push(done.hunk);
            }
            open = Some(Open {
                hunk: Hunk {
                    id: u32::try_from(hunks.len()).unwrap_or(u32::MAX),
                    header: line.to_owned(),
                    old_start,
                    old_len,
                    new_start,
                    new_len,
                    lines: Vec::new(),
                },
                old_left: old_len,
                new_left: new_len,
                next_old: old_start,
                next_new: new_start,
            });
        } else if let Some(done) = open.take() {
            hunks.push(done.hunk);
        }
    }
    if let Some(done) = open {
        hunks.push(done.hunk);
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(hunk: &Hunk) -> Vec<LineKind> {
        hunk.lines.iter().map(|l| l.kind).collect()
    }

    #[test]
    fn a_modified_file_has_numbered_context_added_and_removed_lines() {
        let text = "diff --git a/x b/x\nindex 111..222 100644\n--- a/x\n+++ b/x\n\
                    @@ -830,4 +830,5 @@ fn section()\n a\n-b\n+B\n+B2\n c\n";
        let hunks = parse_unified(text);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.id, 0);
        assert_eq!(h.header, "@@ -830,4 +830,5 @@ fn section()");
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (830, 4, 830, 5));
        assert_eq!(
            kinds(h),
            [
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Added,
                LineKind::Context
            ]
        );
        let numbers: Vec<_> = h.lines.iter().map(|l| (l.old_no, l.new_no)).collect();
        assert_eq!(
            numbers,
            [
                (Some(830), Some(830)),
                (Some(831), None),
                (None, Some(831)),
                (None, Some(832)),
                (Some(832), Some(833))
            ]
        );
        assert_eq!(h.lines[2].text, "B");
    }

    #[test]
    fn several_hunks_are_numbered_in_order() {
        let text = "@@ -1,2 +1,2 @@\n a\n-b\n+c\n@@ -20 +20 @@\n-x\n+y\n";
        let hunks = parse_unified(text);
        assert_eq!(hunks.iter().map(|h| h.id).collect::<Vec<_>>(), [0, 1]);
        assert_eq!((hunks[1].old_start, hunks[1].old_len), (20, 1));
        assert_eq!(hunks[1].lines[1].new_no, Some(20));
    }

    #[test]
    fn a_new_file_starts_at_line_one_with_nothing_before_it() {
        let text = "diff --git a/n b/n\nnew file mode 100644\nindex 000..111\n--- /dev/null\n+++ b/n\n\
                    @@ -0,0 +1,2 @@\n+one\n+two\n";
        let hunks = parse_unified(text);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].old_len), (0, 0));
        assert_eq!(hunks[0].lines[0].new_no, Some(1));
        assert_eq!(hunks[0].lines[1].new_no, Some(2));
        assert!(hunks[0].lines.iter().all(|l| l.old_no.is_none()));
        let header = parse_header(text);
        assert!(header.new_file && !header.deleted_file && !header.binary);
    }

    #[test]
    fn a_deleted_file_ends_at_line_zero_on_the_new_side() {
        let text = "diff --git a/d b/d\ndeleted file mode 100644\nindex 111..000\n--- a/d\n+++ /dev/null\n\
                    @@ -1,2 +0,0 @@\n-one\n-two\n";
        let hunks = parse_unified(text);
        assert_eq!((hunks[0].new_start, hunks[0].new_len), (0, 0));
        assert_eq!(hunks[0].lines[1].old_no, Some(2));
        assert!(hunks[0].lines.iter().all(|l| l.new_no.is_none()));
        assert!(parse_header(text).deleted_file);
    }

    #[test]
    fn no_newline_at_end_of_file_is_its_own_row_after_the_line_it_qualifies() {
        let text = "@@ -1,2 +1,2 @@\n a\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
        let h = &parse_unified(text)[0];
        assert_eq!(
            kinds(h),
            [
                LineKind::Context,
                LineKind::Removed,
                LineKind::NoNewline,
                LineKind::Added,
                LineKind::NoNewline
            ]
        );
        assert_eq!(h.lines[2].text, "No newline at end of file");
        assert_eq!((h.lines[2].old_no, h.lines[2].new_no), (None, None));
        // The marker takes no line number: the added line is still the second of the new side.
        assert_eq!(h.lines[3].new_no, Some(2));
    }

    #[test]
    fn a_binary_patch_has_no_hunks_and_says_so_in_its_header() {
        let text = "diff --git a/b.bin b/b.bin\nindex 111..222 100644\nBinary files a/b.bin and b/b.bin differ\n";
        assert!(parse_unified(text).is_empty());
        let header = parse_header(text);
        assert!(header.binary);
        assert_eq!(header.mode_change, None);
    }

    #[test]
    fn a_mode_only_change_has_no_hunks_and_names_both_modes() {
        let text = "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
        assert!(parse_unified(text).is_empty());
        assert_eq!(
            parse_header(text).mode_change,
            Some(("100644".to_owned(), "100755".to_owned()))
        );
    }

    #[test]
    fn carriage_returns_stay_in_the_text() {
        let text = "@@ -1,2 +1,2 @@\n keep\r\n-old\r\n+new\r\n";
        let h = &parse_unified(text)[0];
        assert_eq!(h.lines[0].text, "keep\r");
        assert_eq!(h.lines[1].text, "old\r");
        assert_eq!(h.lines[2].text, "new\r");
    }

    #[test]
    fn a_removed_line_that_looks_like_a_file_header_is_still_a_line() {
        // Removing "-- x" and adding "++ y" reads `--- x` and `+++ y` in the patch.
        let text = "@@ -1,2 +1,2 @@\n--- x\n+++ y\n z\n";
        let h = &parse_unified(text)[0];
        assert_eq!(kinds(h), [LineKind::Removed, LineKind::Added, LineKind::Context]);
        assert_eq!(h.lines[0].text, "-- x");
        assert_eq!(h.lines[1].text, "++ y");
    }

    #[test]
    fn a_context_line_that_lost_its_space_is_an_empty_context_line() {
        let text = "@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
        let h = &parse_unified(text)[0];
        assert_eq!(h.lines[1].kind, LineKind::Context);
        assert_eq!(h.lines[1].text, "");
        assert_eq!(h.lines[2].old_no, Some(3));
    }

    #[test]
    fn a_malformed_hunk_ends_where_it_stops_making_sense() {
        // The header promises three lines; the text offers one and then a new file's header.
        let text = "@@ -1,3 +1,3 @@\n a\ndiff --git a/y b/y\n@@ -5 +5 @@\n-q\n+r\n";
        let hunks = parse_unified(text);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].lines.len(), 1);
        assert_eq!(hunks[1].old_start, 5);
    }

    #[test]
    fn empty_and_header_only_text_have_no_hunks() {
        assert!(parse_unified("").is_empty());
        assert!(parse_unified("diff --git a/x b/x\n--- a/x\n+++ b/x\n").is_empty());
    }

    #[test]
    fn numstat_and_raw_rows_join_on_the_path() {
        let numstat = b"3\t1\ta.txt\0-\t-\tb.bin\0".to_vec();
        let raw = b":100644 100644 aaa bbb M\0a.txt\0:000000 100644 000 ccc A\0b.bin\0\
                    :000000 160000 000 ddd A\0sub\0"
            .to_vec();
        let changes = join_changes(&parse_numstat(&numstat), &parse_raw(&raw));
        assert_eq!(changes.len(), 3);
        assert_eq!(
            (changes[0].kind, changes[0].added, changes[0].removed),
            (ChangeKind::Modified, 3, 1)
        );
        assert!(changes[1].binary && changes[1].kind == ChangeKind::Added);
        assert!(changes[2].nested && !changes[2].binary);
        assert_eq!((changes[2].added, changes[2].removed), (0, 0));
    }

    #[test]
    fn a_tab_in_a_path_survives_the_numstat_split() {
        let changes = join_changes(
            &parse_numstat(b"1\t0\ta\tb.txt\0"),
            &parse_raw(b":000000 100644 000 aaa A\0a\tb.txt\0"),
        );
        assert_eq!(changes[0].path, PathBuf::from("a\tb.txt"));
        assert_eq!(changes[0].added, 1);
    }

    #[test]
    fn paths_that_leave_the_project_are_refused() {
        for bad in ["", "/etc/passwd", "../x", "a/../../x"] {
            assert!(check_path(Path::new(bad)).is_err(), "{bad:?}");
        }
        assert!(check_path(Path::new("a/b.rs")).is_ok());
    }
}
