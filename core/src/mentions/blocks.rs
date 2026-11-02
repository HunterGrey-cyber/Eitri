//! The text each source puts in a turn. Pure: the editor's answers in, text out.
//!
//! Each section opens with `[@name: ...]`, which says what it is, when it was read and every bound it was cut at, and
//! closes with `[@name end]`. `agent::history::composed_block` takes these back off a resumed prompt by exactly
//! those two lines.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent::read_deny::ReadRules;

use super::Kind;
use crate::editor_tools::paths;
use crate::editor_tools::tools::diagnostics::{DiagnosticEntry, DiagnosticsAnswer};
use crate::editor_tools::tools::mention_reads::{
    BufferRead, BuffersAnswer, QuickfixAnswer, QuickfixItem, BUFFERS_LISTED_MAX, BUFFERS_TOTAL_MAX, BUFFER_BYTES_MAX,
    BUFFER_LINES_MAX,
};

/// The most entries of a list the turn carries, and the most bytes of them.
pub const LIST_ENTRIES_MAX: usize = 200;
pub const LIST_BYTES_MAX: usize = 48 * 1024;
/// What every line of a list starts with.
const LIST_LINE_MARK: &str = "  ";
/// The most of one diagnostic's or quickfix entry's text.
pub const ENTRY_TEXT_MAX: usize = 512;
/// The most of a quickfix list's title.
const TITLE_MAX: usize = 200;
/// The most of a diagnostic's source or code.
const FIELD_TEXT_MAX: usize = 120;

/// What a section's first line begins with, and what its last line is.
pub fn opening(kind: Kind) -> String {
    format!("[@{}: ", kind.word())
}

pub fn closing(kind: Kind) -> String {
    format!("[@{} end]", kind.word())
}

/// Which files a section may name: the project's own, read from where their names say, and not covered by a
/// `Read` deny rule. The reader of the rules is the CLI's `permissions.deny` as `agent::read_deny` holds it.
///
/// It looks at the disk (every name is resolved through its links), so it is made and used on a worker, never on the
/// GTK thread; it keeps what it found of each name, since a diagnostic list names one file again and again.
pub struct Scope<'a> {
    root: &'a Path,
    /// The root with every link followed, which is what a resolved name is under.
    canonical: PathBuf,
    home: Option<&'a Path>,
    rules: &'a ReadRules,
    judged: RefCell<HashMap<(PathBuf, Option<PathBuf>), Result<String, Kept>>>,
}

/// Why a buffer or an entry is not told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    Told,
    Outside,
    Denied,
}

impl<'a> Scope<'a> {
    pub fn new(root: &'a Path, home: Option<&'a Path>, rules: &'a ReadRules) -> Scope<'a> {
        Scope {
            root,
            canonical: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            home,
            rules,
            judged: RefCell::default(),
        }
    }

    /// `name`'s path under the root as a sentence may name it, or why it may not be told. `read_from` is the record of
    /// what the buffer's text was read from; with none, a name already on the disk may hold anything and is kept out.
    fn judge(&self, name: &Path, read_from: Option<&Path>) -> Result<String, Kept> {
        let key = (name.to_path_buf(), read_from.map(Path::to_path_buf));
        if let Some(known) = self.judged.borrow().get(&key) {
            return known.clone();
        }
        let judged = self.judge_afresh(name, read_from);
        self.judged.borrow_mut().insert(key, judged.clone());
        judged
    }

    /// Whether a rule covers `path` as the editor spelled it, when that is under the root the window was given: a rule may
    /// name the project by the path it was opened through, which is not the one with its links followed.
    fn denies_as_given(&self, path: &Path) -> bool {
        path.starts_with(self.root) && self.rules.denies(self.root, path)
    }

    /// `path` as it reads under the root with every link followed, when it is under the root the window was given.
    fn rebased(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(self.root) {
            Ok(rest) => self.canonical.join(rest),
            Err(_) => path.to_path_buf(),
        }
    }

    fn judge_afresh(&self, name: &Path, read_from: Option<&Path>) -> Result<String, Kept> {
        if !name.is_absolute() {
            return Err(Kept::Outside);
        }
        // The project's boundary is the root with its links followed, and a name under the root as it was given is
        // read as under that one.
        let original = name.to_path_buf();
        let original_record = read_from.map(Path::to_path_buf);
        let name = self.rebased(name);
        let read_from = read_from.map(|record| self.rebased(record));
        let Some(resolved) = paths::name_inside(&self.canonical, self.home, &name) else {
            return Err(Kept::Outside);
        };
        if !paths::record_passes(&self.canonical, self.home, &name, read_from.as_deref(), None) {
            return Err(Kept::Outside);
        }
        if self.rules.denies_either(&self.canonical, &name, &resolved) || self.denies_as_given(&original) {
            return Err(Kept::Denied);
        }
        // The text was read from the record's file, whatever the buffer is called now: a rule on that file covers it.
        if let Some(record) = &read_from {
            let from = paths::name_inside(&self.canonical, self.home, record).unwrap_or_else(|| record.clone());
            if self.rules.denies_either(&self.canonical, record, &from) {
                return Err(Kept::Denied);
            }
        }
        if let Some(record) = original_record.as_deref() {
            if self.denies_as_given(record) {
                return Err(Kept::Denied);
            }
        }
        name.strip_prefix(&self.canonical)
            .ok()
            .and_then(Path::to_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or(Kept::Outside)
    }
}

/// How many were kept out, by why.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Dropped {
    outside: u32,
    denied: u32,
}

impl Dropped {
    fn note(&mut self, why: Kept) {
        match why {
            Kept::Outside => self.outside += 1,
            Kept::Denied => self.denied += 1,
            Kept::Told => {}
        }
    }

    fn sentence(self) -> Option<String> {
        let mut parts = Vec::new();
        if self.outside > 0 {
            parts.push(format!(
                "{} outside the project or read from another file",
                self.outside
            ));
        }
        if self.denied > 0 {
            parts.push(format!("{} covered by a Read deny rule in the settings", self.denied));
        }
        (!parts.is_empty()).then(|| format!("Left out: {}.", parts.join(", ")))
    }
}

/// A section: its first line, its body and its last line, all of it editor text and so all of it made inert.
fn framed(kind: Kind, head: &str, body: &str) -> String {
    let mut out = format!("{}{head}]\n", opening(kind));
    out.push_str(body);
    if !body.is_empty() && !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&closing(kind));
    agent::mention::inert(&out).into_owned()
}

/// A section for a source that could not be read: it says so, as the person sent the word expecting an answer.
pub fn unavailable(kind: Kind, why: &str) -> String {
    framed(kind, &format!("not attached: {}", one_line(why)), "")
}

/// A reader of the settings that could not be read keeps every file out; the section says why.
pub fn closed(kind: Kind, rules: &ReadRules) -> Option<String> {
    rules.closed().map(|why| {
        unavailable(
            kind,
            &format!("{why}, so no file of the project can be told apart from one a Read deny rule covers"),
        )
    })
}

/// The characters of editor text that could change what a reader makes of the section: controls other than tab and
/// newline, the bidirectional controls and the line and paragraph separators (the set a file name may not hold). Each
/// becomes U+FFFD.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| {
            if crate::agent_bridge::is_control_like(c) && c != '\t' && c != '\n' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// `plain`, on one line: a field of the editor's that sits in a sentence or after a mark may not start a line.
fn one_line(text: &str) -> String {
    plain(text).replace('\n', " ")
}

/// `text` cut to at most `max` bytes on a character boundary, with a mark when it was cut. Cut after `plain`, since
/// that is the text the bound is about.
fn cut_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} (cut)", &text[..end])
}

/// A short field of the editor's on one line (a diagnostic's source or code).
fn field(text: &str) -> String {
    cut_text(&one_line(text), FIELD_TEXT_MAX)
}

fn kib(bytes: u32) -> String {
    format!("{} KiB", bytes / 1024)
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// `@buffers`: the text of each open file of the project as the editor holds it.
pub fn buffers(answer: &BuffersAnswer, scope: &Scope<'_>) -> String {
    if let Some(section) = closed(Kind::Buffers, scope.rules) {
        return section;
    }
    let mut dropped = Dropped {
        outside: answer.other,
        denied: 0,
    };
    let mut told: Vec<(String, &BufferRead)> = Vec::new();
    for buffer in &answer.buffers {
        match scope.judge(&buffer.path, buffer.read_from.as_deref()) {
            Ok(path) => told.push((path, buffer)),
            Err(why) => dropped.note(why),
        }
    }
    let unsaved = told.iter().filter(|(_, buffer)| buffer.modified).count();
    let mut head = format!(
        "the editor's open files as they are on screen when the message was sent, {}",
        plural(told.len(), "file", "files")
    );
    if unsaved > 0 {
        let _ = write!(
            head,
            ", {unsaved} with unsaved changes: for those the text below is the editor's, which differs from the file on disk"
        );
    }
    let _ = write!(
        head,
        ". Bounds: {BUFFER_LINES_MAX} lines and {} a file, {} in all, {BUFFERS_LISTED_MAX} files",
        kib(BUFFER_BYTES_MAX),
        kib(BUFFERS_TOTAL_MAX),
    );
    let mut body = String::new();
    let mut total = 0usize;
    for (path, buffer) in told {
        let mut marks = Vec::new();
        if buffer.modified {
            marks.push("unsaved changes".to_owned());
        }
        if buffer.current {
            marks.push("current".to_owned());
        }
        marks.push(plural(buffer.lines as usize, "line", "lines"));
        let _ = writeln!(body, "--- {path} ({}) ---", marks.join(", "));
        if !buffer.loaded {
            body.push_str("(not loaded in the editor: no text attached)\n");
            continue;
        }
        // The bounds are about the text as it is told: the numbers and the replaced characters count.
        let (mut written, mut emitted, mut by_total) = (0, 0, false);
        for line in &buffer.text {
            // Made inert before it is counted, as a list line is.
            let text = agent::mention::inert(&format!("{}\t{}\n", emitted + 1, plain(line))).into_owned();
            if written + text.len() > BUFFER_BYTES_MAX as usize {
                break;
            }
            if total + text.len() > BUFFERS_TOTAL_MAX as usize {
                by_total = true;
                break;
            }
            written += text.len();
            total += text.len();
            emitted += 1;
            body.push_str(&text);
        }
        if emitted < buffer.text.len() || buffer.cut.is_some() {
            let why = if by_total || buffer.budget {
                format!("the {} bound on all files together was reached", kib(BUFFERS_TOTAL_MAX))
            } else {
                format!(
                    "the bound of {BUFFER_LINES_MAX} lines or {} a file",
                    kib(BUFFER_BYTES_MAX)
                )
            };
            let _ = writeln!(
                body,
                "(lines {}-{} not attached: {why})",
                emitted + 1,
                (buffer.lines as usize).max(emitted + 1)
            );
        }
    }
    let mut left = Vec::new();
    if answer.left_out > 0 {
        left.push(format!(
            "{} more files past the {BUFFERS_LISTED_MAX}-file bound",
            answer.left_out
        ));
    }
    if let Some(sentence) = dropped.sentence() {
        left.push(sentence);
    }
    if !left.is_empty() {
        let _ = writeln!(body, "{}", left.join(" "));
    }
    framed(Kind::Buffers, &head, &body)
}

fn kind_word(kind: &str) -> &'static str {
    match kind {
        "E" | "e" => "error ",
        "W" | "w" => "warning ",
        "I" | "i" => "info ",
        "N" | "n" => "note ",
        _ => "",
    }
}

fn place(path: &str, line: u32, column: u32) -> String {
    match (line, column) {
        (0, _) => path.to_owned(),
        (line, 0) => format!("{path}:{line}"),
        (line, column) => format!("{path}:{line}:{column}"),
    }
}

/// Entries one to a line, in order, as far as the entry and byte bounds go. Returns the text and how many were shown.
fn list_lines(lines: &[String]) -> (String, usize) {
    let mut body = String::new();
    let mut shown = 0;
    for line in lines.iter().take(LIST_ENTRIES_MAX) {
        // Every line of a list starts with the same two spaces: no line of the editor's text can be a frame line, which
        // starts at the left edge, or make a blank line that a reader of the frame would take as a boundary. Made
        // inert here, before it is counted: a mention gains three bytes, and the bound is on what the model reads.
        let told = agent::mention::inert(&format!("{LIST_LINE_MARK}{line}")).into_owned();
        if body.len() + told.len() + 1 > LIST_BYTES_MAX {
            break;
        }
        body.push_str(&told);
        body.push('\n');
        shown += 1;
    }
    (body, shown)
}

fn entry_text(text: &str) -> String {
    // The later lines of a text are indented past the mark, so they are no less a part of the entry; the bound is on
    // the text as told, indent included.
    cut_text(&plain(text).replace('\n', "\n    "), ENTRY_TEXT_MAX)
}

/// `@quickfix`: the editor's quickfix list, an entry to a line.
pub fn quickfix(answer: &QuickfixAnswer, scope: &Scope<'_>) -> String {
    if let Some(section) = closed(Kind::Quickfix, scope.rules) {
        return section;
    }
    let mut dropped = Dropped::default();
    let mut lines = Vec::new();
    for item in &answer.items {
        let QuickfixItem {
            path,
            read_from,
            lnum,
            col,
            kind,
            text,
            ..
        } = item;
        let shown = match path {
            Some(path) => match scope.judge(path, read_from.as_deref()) {
                Ok(shown) => Some(shown),
                Err(why) => {
                    dropped.note(why);
                    continue;
                }
            },
            // An entry that points at a file and lost it (its buffer was wiped) still holds that file's line: it is
            // not a line of compiler output, and nothing says whose it was.
            None if item.valid || item.lnum != 0 || item.end_lnum != 0 || item.col != 0 => {
                dropped.note(Kept::Outside);
                continue;
            }
            None => None,
        };
        let text = entry_text(text);
        lines.push(match shown {
            Some(shown) => format!("{} {}{text}", place(&shown, *lnum, *col), kind_word(kind)),
            None => text,
        });
    }
    let (body, shown) = list_lines(&lines);
    // A title may name a file the list was made of (`:vimgrep /KEY/ .env`): with an entry dropped, or entries past the
    // ones read that may be the dropped ones, it is left out.
    let title = if dropped == Dropped::default() && answer.left_out == 0 {
        cut_text(&one_line(&answer.title), TITLE_MAX)
    } else {
        String::new()
    };
    let mut head = if title.is_empty() {
        "the editor's quickfix list".to_owned()
    } else {
        format!("the editor's quickfix list \"{title}\"")
    };
    let _ = write!(
        head,
        " when the message was sent: {} in the list, {shown} below, in the list's order",
        answer.size
    );
    let mut notes = Vec::new();
    let more = lines.len() - shown;
    if more > 0 {
        notes.push(format!(
            "{more} more entries not attached: the bound is {LIST_ENTRIES_MAX} entries or {} KiB.",
            LIST_BYTES_MAX / 1024
        ));
    }
    if answer.left_out > 0 {
        notes.push(format!(
            "{} entries past the first {} were not read.",
            answer.left_out,
            answer.items.len()
        ));
    }
    if let Some(sentence) = dropped.sentence() {
        notes.push(sentence);
    }
    let mut body = body;
    if answer.size == 0 {
        body.push_str("(the list is empty)\n");
    }
    for note in notes {
        let _ = writeln!(body, "{note}");
    }
    framed(Kind::Quickfix, &head, &body)
}

fn diagnostic_line(path: &str, entry: &DiagnosticEntry) -> String {
    let mut source = String::new();
    match (&entry.source, &entry.code) {
        (Some(s), Some(c)) => source = format!(" [{} {}]", field(s), field(c)),
        (Some(s), None) => source = format!(" [{}]", field(s)),
        (None, Some(c)) => source = format!(" [{}]", field(c)),
        (None, None) => {}
    }
    format!(
        "{} {}{source}: {}",
        place(path, entry.line, entry.column),
        entry.severity.word(),
        entry_text(&entry.message)
    )
}

/// `@diagnostics`: what the language servers and linters said, most severe first, with file and line.
pub fn diagnostics(answer: &DiagnosticsAnswer, scope: &Scope<'_>) -> String {
    if let Some(section) = closed(Kind::Diagnostics, scope.rules) {
        return section;
    }
    let mut dropped = Dropped::default();
    let mut lines = Vec::new();
    let mut told_paths: Vec<PathBuf> = Vec::new();
    for entry in &answer.entries {
        match scope.judge(&entry.path, entry.read_from.as_deref()) {
            Ok(path) => {
                lines.push(diagnostic_line(&path, entry));
                if !told_paths.contains(&entry.path) {
                    told_paths.push(entry.path.clone());
                }
            }
            Err(why) => dropped.note(why),
        }
    }
    let (body, shown) = list_lines(&lines);
    let total = lines.len() as u64 + u64::from(answer.left_out);
    let mut head = format!(
        "what the editor's language servers and linters had said when the message was sent, most severe first, \
         with file and line: {}, {shown} below",
        plural(total as usize, "diagnostic", "diagnostics")
    );
    let unsaved: Vec<String> = answer
        .modified
        .iter()
        .filter(|name| told_paths.contains(name))
        .filter_map(|name| scope.judge(name, Some(name)).ok())
        .collect();
    if !unsaved.is_empty() {
        let _ = write!(
            head,
            ". Files with unsaved changes, whose diagnostics describe the text on screen and not the file on disk: {}",
            unsaved.join(", ")
        );
    }
    let mut notes = Vec::new();
    let more = lines.len() - shown;
    if more > 0 {
        notes.push(format!(
            "{more} more, the least severe, not attached: the bound is {LIST_ENTRIES_MAX} entries or {} KiB.",
            LIST_BYTES_MAX / 1024
        ));
    }
    if answer.left_out > 0 {
        notes.push(format!(
            "{} more, the least severe, were not read: the editor was asked for {} at most.",
            answer.left_out,
            answer.entries.len()
        ));
    }
    if let Some(sentence) = dropped.sentence() {
        notes.push(sentence);
    }
    let mut body = body;
    if total == 0 && dropped == Dropped::default() {
        body.push_str("(no diagnostics)\n");
    }
    for note in notes {
        let _ = writeln!(body, "{note}");
    }
    framed(Kind::Diagnostics, &head, &body)
}

#[cfg(test)]
mod tests;
