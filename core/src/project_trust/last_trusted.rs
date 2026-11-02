//! The instruction files as they stood at the last `y`, kept beside the record so a question about a
//! changed configuration can show what changed in them.
//!
//! The record holds hashes, which decide trust and say nothing a person can read. This copy decides
//! nothing: no answer to "is this root trusted" reads it, and a missing, stale or foreign copy changes
//! only what the question shows. A copy's text counts as "before" only when its sha256 is the one the
//! record holds for that path, so what is drawn as the trusted text is the text that was trusted.
//!
//! Text is taken from the very read discovery hashed, and bounded per file and for the whole document;
//! past a bound a file is marked and never cut. No disk here: [`super::store`] reads and writes it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::Diff;
use super::{hex, Discovery};

/// The most of one instruction file a copy holds. Claude Code itself warns at a tenth of this.
pub const MAX_KEPT_FILE_BYTES: u64 = 256 << 10;
/// The largest copy written or read, as it lies on disk.
pub const MAX_KEPT_BYTES: u64 = 1 << 20;

/// One instruction file, as far as it can be compared later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kept {
    Text(String),
    /// Past [`MAX_KEPT_FILE_BYTES`].
    TooLong,
    /// Within that, but past what the whole copy may hold.
    NoRoom,
    /// Not UTF-8, or holding a NUL, so there are no lines to compare; or a file whose own name is not
    /// UTF-8, which the record cannot tell from another file's.
    NotText,
    /// Reached through a link, its own or a directory's: the file's history is at the target's path.
    Link,
}

impl Kept {
    /// What is kept of a file discovery has just read and hashed.
    pub(super) fn of(bytes: &[u8], through_link: bool) -> Kept {
        if through_link {
            return Kept::Link;
        }
        if bytes.len() as u64 > MAX_KEPT_FILE_BYTES {
            return Kept::TooLong;
        }
        match std::str::from_utf8(bytes) {
            Ok(text) if !text.contains('\0') => Kept::Text(text.to_string()),
            _ => Kept::NotText,
        }
    }
}

/// Why a changed instruction file has no diff at the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoDiff {
    NoEarlierCopy,
    TooLong,
    NotText,
    Link,
    /// The trusted text is not there because of what the file was then; it is none of these now.
    WasTooLong,
    WasNotText,
    WasLink,
    NoRoom,
}

impl NoDiff {
    /// The line the question shows, after the file's path.
    pub fn line(self) -> String {
        match self {
            NoDiff::NoEarlierCopy => "no earlier copy to compare with".to_string(),
            NoDiff::TooLong => format!("over the {} KiB kept of a file, so no diff", MAX_KEPT_FILE_BYTES >> 10),
            NoDiff::NotText => "not text, so no diff".to_string(),
            NoDiff::Link => "a link, so no diff".to_string(),
            NoDiff::WasTooLong => format!(
                "was over the {} KiB kept of a file, so no diff",
                MAX_KEPT_FILE_BYTES >> 10
            ),
            NoDiff::WasNotText => "was not text, so no diff".to_string(),
            NoDiff::WasLink => "was a link, so no diff".to_string(),
            NoDiff::NoRoom => format!(
                "did not fit in the {} MiB kept for a project, so no diff",
                MAX_KEPT_BYTES >> 20
            ),
        }
    }

    /// Why a file as it is now has no diff.
    fn now(kept: &Kept) -> Option<NoDiff> {
        match kept {
            Kept::Text(_) => None,
            Kept::TooLong => Some(NoDiff::TooLong),
            Kept::NoRoom => Some(NoDiff::NoRoom),
            Kept::NotText => Some(NoDiff::NotText),
            Kept::Link => Some(NoDiff::Link),
        }
    }

    /// Why a file that is text now has no diff, from what its copy says of it then.
    fn then(kept: &Kept) -> Option<NoDiff> {
        match kept {
            Kept::Text(_) => None,
            Kept::TooLong => Some(NoDiff::WasTooLong),
            Kept::NoRoom => Some(NoDiff::NoRoom),
            Kept::NotText => Some(NoDiff::WasNotText),
            Kept::Link => Some(NoDiff::WasLink),
        }
    }
}

/// What the question can say about one instruction file that differs from what was trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangedText {
    Diff {
        path: PathBuf,
        before: String,
        after: String,
    },
    NoDiff {
        path: PathBuf,
        why: NoDiff,
    },
}

#[derive(Serialize, Deserialize)]
pub(super) struct KeptFiles {
    version: u32,
    root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    files: BTreeMap<String, Kept>,
}

fn written_len<T: Serialize>(value: &T) -> Option<u64> {
    serde_json::to_string(value).ok().map(|text| text.len() as u64)
}

/// The copy of `discovery` as written, or `None` when it has no instruction file to keep, cannot be
/// named, or would not fit even as marks. Every file's mark is paid for first, whatever comes of its
/// text; text then takes what is left, in path order, and one that does not fit stays a mark.
pub(super) fn encode(discovery: &Discovery, profile: Option<String>) -> Option<String> {
    if discovery.instructions.is_empty() {
        return None;
    }
    let mut copy = KeptFiles {
        version: 1,
        root: discovery.root.to_str()?.to_string(),
        profile,
        files: BTreeMap::new(),
    };
    let separators = 2;
    let mut marks = written_len(&copy)?;
    let mut sized = Vec::new();
    for (path, kept) in &discovery.instructions {
        let key = path.to_string_lossy().into_owned();
        let mark = match kept {
            Kept::Text(_) => written_len(&Kept::TooLong)?,
            other => written_len(other)?,
        };
        marks += written_len(&key)? + mark + separators;
        sized.push((key, kept, mark));
    }
    let mut room = MAX_KEPT_BYTES.saturating_sub(marks);
    for (key, kept, mark) in sized {
        let beyond_the_mark = written_len(kept)?.saturating_sub(mark);
        let fits = beyond_the_mark <= room;
        if fits {
            room -= beyond_the_mark;
        }
        let kept = match kept {
            Kept::Text(_) if !fits => Kept::NoRoom,
            kept => kept.clone(),
        };
        copy.files.insert(key, kept);
    }
    serde_json::to_string(&copy)
        .ok()
        .filter(|text| text.len() as u64 <= MAX_KEPT_BYTES)
}

/// The copy in `bytes` when it is one for `root` and `profile`.
pub(super) fn decode(bytes: &[u8], root: &Path, profile: Option<String>) -> Option<KeptFiles> {
    let copy: KeptFiles = serde_json::from_slice(bytes).ok()?;
    (copy.version == 1 && root.to_str() == Some(copy.root.as_str()) && copy.profile == profile).then_some(copy)
}

/// What the question says about each instruction file `diff` names, in path order. `recorded` is the
/// record's hash of each path. A removed file has no line: nothing of it loads, and the question lists it.
pub(super) fn compare(
    recorded: &BTreeMap<String, String>,
    copy: Option<&KeptFiles>,
    discovery: &Discovery,
    diff: &Diff,
) -> Vec<ChangedText> {
    // By the key the record and `diff` use, the later of two names that share it winning as it does there.
    let now: BTreeMap<String, &Kept> = discovery
        .instructions
        .iter()
        .map(|(path, kept)| (path.to_string_lossy().into_owned(), kept))
        .collect();
    let mut out = Vec::new();
    for path in &diff.added {
        if let Some(kept) = now.get(path.to_string_lossy().as_ref()) {
            out.push(ChangedText::NoDiff {
                path: path.clone(),
                why: NoDiff::now(kept).unwrap_or(NoDiff::NoEarlierCopy),
            });
        }
    }
    for path in &diff.changed {
        let key = path.to_string_lossy();
        // A file that could not be read has no entry, and its own row says it cannot be checked.
        let Some(&kept) = now.get(key.as_ref()) else {
            continue;
        };
        let before = copy.and_then(|copy| copy.files.get(key.as_ref()));
        let matches_record = |text: &str| recorded.get(key.as_ref()) == Some(&hex(&Sha256::digest(text.as_bytes())));
        out.push(match (kept, before) {
            (Kept::Text(after), Some(Kept::Text(before))) if matches_record(before) => ChangedText::Diff {
                path: path.clone(),
                before: before.clone(),
                after: after.clone(),
            },
            (kept, before) => ChangedText::NoDiff {
                path: path.clone(),
                why: NoDiff::now(kept)
                    .or(before.and_then(NoDiff::then))
                    .unwrap_or(NoDiff::NoEarlierCopy),
            },
        });
    }
    out.sort_by(|a, b| path_of(a).cmp(path_of(b)));
    out
}

fn path_of(text: &ChangedText) -> &Path {
    match text {
        ChangedText::Diff { path, .. } | ChangedText::NoDiff { path, .. } => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_trust::{Fingerprint, Profile};

    fn discovery(instructions: BTreeMap<PathBuf, Kept>) -> Discovery {
        Discovery {
            profile: Profile::Claude,
            root: PathBuf::from("/p"),
            top: PathBuf::from("/p"),
            walked: vec![PathBuf::from("/p")],
            entries: Vec::new(),
            findings: Vec::new(),
            fingerprint: Fingerprint([0; 32]),
            over_budget: None,
            user_mcp_servers: None,
            user_mcp_names: None,
            project_mcp_servers: Vec::new(),
            instructions,
        }
    }

    /// A long name in every mark: `name` and its text, for `count` files.
    fn named(count: usize, name_len: usize, text_len: usize) -> BTreeMap<PathBuf, Kept> {
        (0..count)
            .map(|i| {
                let path = PathBuf::from(format!("{i:05}{}.md", "n".repeat(name_len)));
                (path, Kept::Text("t".repeat(text_len)))
            })
            .collect()
    }

    /// A copy past the bound would be written and then refused by its reader, so it is not written.
    #[test]
    fn marks_alone_past_the_bound_leave_no_copy() {
        let many = discovery(named(3_000, 400, 1));
        assert_eq!(encode(&many, None), None);
    }

    /// The marks of files past the room are paid for, so they never push the copy over and cost
    /// every other file its diff.
    #[test]
    fn a_file_past_the_room_is_marked_and_the_others_keep_their_text() {
        let mut files = named(40, 200, 10);
        for i in 0..4 {
            let name = format!("{}{i}.md", "z".repeat(5));
            files.insert(PathBuf::from(name), Kept::Text("x".repeat(261_000)));
        }
        let text = encode(&discovery(files), None).expect("the copy is written");
        assert!(text.len() as u64 <= MAX_KEPT_BYTES, "{}", text.len());
        let copy: KeptFiles = serde_json::from_str(&text).unwrap();
        let small = copy
            .files
            .values()
            .filter(|kept| matches!(kept, Kept::Text(t) if t.len() == 10));
        assert_eq!(small.count(), 40);
        let large = copy
            .files
            .values()
            .filter(|kept| matches!(kept, Kept::Text(t) if t.len() == 261_000));
        assert_eq!(large.count(), 3);
        assert_eq!(copy.files.values().filter(|kept| **kept == Kept::NoRoom).count(), 1);
    }
}
