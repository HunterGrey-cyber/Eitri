//! A proposal: what a language server's edit would do to the project, computed and shown while nothing on disk or in
//! a buffer changes. [`EditRun`] is the run of every tool that ends in one.
//!
//! The server's positions are about the files as they stood at the ask, and the proposal's bytes are the disk's. So
//! a proposal stands only where the two agree: a target with unsaved changes, a buffer that moved since the ask, a
//! file with no buffer at the ask that changed since, and a buffer whose text differs from the disk each refuse it.
//! What is kept is two blob ids per file in turn review's store and the hunks; the bytes stay in the store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::stamp::EDITOR_BUDGET;
use super::step::{
    encoded_len, EditorFailure, EditorReach, Step, StepEvent, ToolCx, ToolError, ToolResult, ToolRun,
    ANSWER_MAX_ENCODED,
};
use super::text_edits::{apply_text_edits, buffer_digest, final_newline_agrees, EditError, Encoding, TextEdit};
use super::tools::lsp::{self, AskCollect, Asked, Collected, Progress, RawEdit, RawFileEdit, TargetState, UNREADABLE};
use super::way::EditorCall;
use super::{paths, words};
use crate::turn_review::{FileNow, ProposalRefused, ProposalWriter, ProposedFile};

/// How many proposals a tab keeps.
pub const MAX_PROPOSALS: usize = 8;
/// How many files one proposal may touch.
pub const MAX_FILES: usize = 64;

/// The most hunk text an answer tells. The whole of it stays with the proposal for the card.
const HUNKS_TOLD_MAX: usize = 256 * 1024;
/// How long a run rests while its worker runs.
const WORK_POLL: Duration = Duration::from_millis(20);
/// How long a run waits for one worker. A worker's polls hold the tab's flight, so a store or a disk that stopped
/// answering must not hold the tab's editor calls until the turn is interrupted. Long enough for every file at its
/// largest to be read, or to be stored and compared by git.
const WORK_LIMIT: Duration = Duration::from_secs(30);
/// An id as long as any a tab can reach: the answer is fitted before its id is known.
const LONGEST_ID: &str = "p-18446744073709551615";

const NO_CHANGE: &str = "the language server proposes no change";
const NOT_ANSWERED: &str = "the language server did not answer within 10 s";
const TOO_LARGE: &str = "the edit is too large to show";
const NOTHING_LEFT: &str = "the proposal's run was asked to go on after it ended";
const UNTRUSTED: &str = "no change can be proposed in a session started without trusting this project";

/// A change kept for `apply_proposal`, its files reachable by blob id in turn review's store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub id: String,
    /// What the change is, in a few words.
    pub what: String,
    pub files: Vec<ProposedFile>,
}

/// Whether this tab may be given a proposal at all. A proposal computed for a window-2 tab could not be applied
/// there, and a session started with `n` is not offered these tools; the catalogue already keeps them from it, and
/// this holds for whatever calls anyway.
pub fn may_propose(cx: &ToolCx<'_>) -> Result<(), ToolError> {
    if cx.reach == EditorReach::Link {
        return Err(ToolError(words::MOVE_TO_EDITOR.to_owned()));
    }
    if cx.trust.refused() {
        return Err(ToolError(UNTRUSTED.to_owned()));
    }
    Ok(())
}

fn failure(text: impl Into<String>) -> Step {
    Step::Done(ToolResult {
        text: text.into(),
        is_error: true,
    })
}

/// A job off the GTK thread, as a run polls it.
struct Work<T> {
    answer: Receiver<T>,
    since: Instant,
}

impl<T: Send + 'static> Work<T> {
    fn start(job: impl FnOnce() -> T + Send + 'static) -> Work<T> {
        Work {
            answer: lsp::worker(job),
            since: Instant::now(),
        }
    }
}

impl<T> Work<T> {
    /// `Ok(None)`: still at it. A thread stuck in the kernel cannot be stopped: it ends when its call returns, and
    /// its answer goes nowhere.
    fn poll(&self) -> Result<Option<T>, String> {
        match self.answer.try_recv() {
            Ok(done) => Ok(Some(done)),
            Err(TryRecvError::Empty) if self.since.elapsed() >= WORK_LIMIT => Err(format!(
                "the proposal could not be made within {} s; nothing was proposed",
                WORK_LIMIT.as_secs()
            )),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(UNREADABLE.to_owned()),
        }
    }
}

/// A file a proposal will touch, as it stands on disk.
struct ToEdit {
    rel: PathBuf,
    /// Where it is, every link followed: what the editor is asked about.
    path: PathBuf,
    edits: Vec<TextEdit>,
    now: FileNow,
}

/// The files of an edit, read.
struct Read {
    files: Vec<ToEdit>,
    encoding: Encoding,
    writer: ProposalWriter,
}

/// What a proposal holds, and how much of it the answer tells.
struct Built {
    files: Vec<ProposedFile>,
    told: Told,
}

#[derive(Serialize)]
struct ToldFile {
    path: String,
    hunks: String,
}

struct Told {
    files: Vec<ToldFile>,
    left_out_lines: u64,
}

/// Where a server's path is under the root. A path that is not absolute is a URI, outside as it stands. A name with
/// nothing on disk comes back too, for the read to say so.
fn relative_to_root(root: &Path, home: Option<&Path>, raw: &str) -> Option<(PathBuf, PathBuf)> {
    let raw = Path::new(raw);
    if !raw.is_absolute() {
        return None;
    }
    let path = paths::name_inside(root, home, raw)?;
    let rel = path.strip_prefix(root).ok()?.to_path_buf();
    Some((rel, path))
}

/// Which files `files` names, each looked at before any is read. One outside refuses the whole edit, and so does a
/// file named twice: both entries would pass every look before the write, and the second would fail after the first
/// landed.
fn targets_of(
    root: &Path,
    home: Option<&Path>,
    files: Vec<RawFileEdit>,
) -> Result<Vec<(PathBuf, PathBuf, RawFileEdit)>, String> {
    let mut seen = BTreeSet::new();
    let mut targets = Vec::with_capacity(files.len());
    for file in files {
        let Some((rel, path)) = relative_to_root(root, home, &file.path) else {
            return Err(format!(
                "the edit touches {}, outside the project; nothing was proposed",
                file.path
            ));
        };
        if !seen.insert(rel.clone()) {
            return Err(format!("the edit names {} twice; nothing was proposed", rel.display()));
        }
        targets.push((rel, path, file));
    }
    Ok(targets)
}

/// Reads every file of `edit`, or says why it cannot be proposed. On a worker: it walks paths and reads files.
fn read_files(edit: RawEdit, root: &Path, home: Option<&Path>, writer: Option<ProposalWriter>) -> Result<Read, String> {
    if edit.too_large {
        return Err(TOO_LARGE.to_owned());
    }
    if let Some(first) = edit.resource_ops.first() {
        return Err(format!(
            "the edit creates, renames or deletes a file ({first}); only edits to existing files can be proposed"
        ));
    }
    if edit.files.len() > MAX_FILES {
        return Err(format!(
            "the edit touches {} files, and at most {MAX_FILES} can be proposed",
            edit.files.len()
        ));
    }
    let targets = targets_of(root, home, edit.files)?;
    let writer = writer.ok_or_else(|| ProposalRefused::NoStore.to_string())?;
    let mut files = Vec::with_capacity(targets.len());
    for (rel, path, file) in targets {
        let now = writer.read(&rel).map_err(|why| why.to_string())?;
        let edits = file
            .edits
            .into_iter()
            .map(|edit| TextEdit {
                start: edit.start,
                end: edit.end,
                new_text: edit.new_text,
            })
            .collect();
        files.push(ToEdit { rel, path, edits, now });
    }
    Ok(Read {
        files,
        encoding: edit.encoding,
        writer,
    })
}

/// What the editor said against what the ask recorded: the unsaved text of a target, or a target that moved since the
/// server computed its positions, even one back at the disk's text. A target with a buffer at the ask moved if its
/// changedtick did. One with none has no record there, and a stored action can be applied minutes later, so its file's
/// time speaks: the digest cannot, since a buffer opened since agrees with the disk it was opened from, and a file
/// still not loaded has no buffer to compare. A tool that restores a file's old time goes unseen.
fn editor_refuses(
    files: &[ToEdit],
    states: &[TargetState],
    ticks: &BTreeMap<u64, u64>,
    asked_ms: u64,
) -> Option<String> {
    for (file, state) in files.iter().zip(states) {
        let shown = file.rel.display();
        if state.found && state.modified {
            return Some(format!("{shown} has unsaved changes in your editor"));
        }
        let at_the_ask = state.buf.filter(|_| state.found).and_then(|buf| ticks.get(&buf));
        let moved = match at_the_ask {
            Some(tick) => Some(*tick) != state.changedtick,
            None => file.now.modified_ms > asked_ms,
        };
        if moved {
            return Some(format!("{shown} changed since the language server answered; ask again"));
        }
    }
    None
}

fn edit_refused(rel: &Path, error: EditError) -> String {
    let shown = rel.display();
    match error {
        EditError::NotText => format!("the edit to {shown} would leave something that is not text"),
        EditError::InsideCharacter { line } => {
            format!(
                "the edit to {shown} points inside a character on line {}",
                u64::from(line) + 1
            )
        }
        EditError::Reversed { line } => {
            format!(
                "the edit to {shown} has a range that ends before it starts, on line {}",
                u64::from(line) + 1
            )
        }
        EditError::Overlap { line } => {
            format!(
                "the edit to {shown} has overlapping ranges, on line {}",
                u64::from(line) + 1
            )
        }
    }
}

/// What a code action adds to its answer. The title is the server's, so it counts when the answer is fitted.
#[derive(Clone, Default)]
struct ActionNote {
    title: Option<String>,
    command_not_run: bool,
}

/// The answer with this id and these hunks.
fn answer_text(id: &str, told: &Told, note: &ActionNote) -> String {
    #[derive(Serialize)]
    struct Answer<'a> {
        proposal: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<&'a str>,
        files: &'a [ToldFile],
        left_out_lines: u64,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        command_not_run: bool,
    }
    serde_json::to_string(&Answer {
        proposal: id,
        title: note.title.as_deref(),
        files: &told.files,
        left_out_lines: told.left_out_lines,
        command_not_run: note.command_not_run,
    })
    .expect("strings and numbers serialize")
}

/// `files`, told up to their first `kept` lines of hunks over all of them. A file past them is still named.
fn told_up_to(files: &[ProposedFile], kept: usize) -> Told {
    let mut room = kept;
    let mut left_out_lines = 0u64;
    let files = files
        .iter()
        .map(|file| {
            let lines = file.hunks.split_inclusive('\n').count();
            let here = lines.min(room);
            room -= here;
            left_out_lines += (lines - here) as u64;
            ToldFile {
                path: file.rel.to_string_lossy().into_owned(),
                hunks: file.hunks.split_inclusive('\n').take(here).collect(),
            }
        })
        .collect();
    Told { files, left_out_lines }
}

/// As many whole lines of hunks as [`HUNKS_TOLD_MAX`] allows, and lines off the end until the answer fits with any id
/// a tab can reach. So the broker's own cap, which would replace the answer, can never lose the id. Escaped twice a
/// line can cost seven times its bytes, and what fits only shrinks as lines are added, so the largest count that does
/// is found by halving. With no room for even the names, nothing is proposed.
fn fitted(files: &[ProposedFile], note: &ActionNote) -> Result<Told, String> {
    let mut bytes = 0;
    let mut all = 0;
    for line in files.iter().flat_map(|file| file.hunks.split_inclusive('\n')) {
        bytes += line.len();
        if bytes > HUNKS_TOLD_MAX {
            break;
        }
        all += 1;
    }
    let fits =
        |kept: usize| encoded_len(&answer_text(LONGEST_ID, &told_up_to(files, kept), note)) <= ANSWER_MAX_ENCODED;
    if !fits(0) {
        return Err(TOO_LARGE.to_owned());
    }
    let mut kept = all;
    if !fits(all) {
        let (mut enough, mut over) = (0, all);
        while over - enough > 1 {
            let middle = enough + (over - enough) / 2;
            if fits(middle) {
                enough = middle;
            } else {
                over = middle;
            }
        }
        kept = enough;
    }
    Ok(told_up_to(files, kept))
}

/// The proposal's blobs and its answer, or why there is none. On a worker: a proposal may hold 64 files of 4 MiB.
///
/// The server's positions were computed from the buffer, so a file edited a moment ago and not yet reloaded would take
/// them at the wrong place. The digest and the final newline are compared first, for every file. Then every edit is
/// applied before the first store, so that an edit refused in a later file leaves no blobs of the earlier ones.
fn build(read: Read, states: &[TargetState], note: &ActionNote) -> Result<Built, String> {
    let Read {
        files,
        encoding,
        writer,
    } = read;
    for (file, state) in files.iter().zip(states).filter(|(_, state)| state.found) {
        let shown = file.rel.display();
        if buffer_digest(&file.now.bytes, state.format) != state.sha256 {
            return Err(format!(
                "{shown} changed on disk since your editor read it; try again in a moment"
            ));
        }
        // The digest cannot see this, and the server's text has the newline or not as the buffer says: a range at the
        // end of the last line means something else on this disk. A save that added the newline leaves the buffer's
        // `'endofline'` as it was, so only a reload settles it.
        if !final_newline_agrees(&file.now.bytes, state.eol) {
            return Err(format!(
                "{shown} ends differently on disk than in your editor; reload it there and try again"
            ));
        }
    }
    let mut changed = Vec::with_capacity(files.len());
    for file in files {
        let after =
            apply_text_edits(&file.now.bytes, &file.edits, encoding).map_err(|error| edit_refused(&file.rel, error))?;
        if after != file.now.bytes {
            changed.push((file, after));
        }
    }
    let mut proposed = Vec::with_capacity(changed.len());
    for (file, after) in &changed {
        let stored = writer.store(&file.rel, &file.now, after);
        proposed.push(stored.map_err(|why| why.to_string())?);
    }
    if proposed.is_empty() {
        return Err(NO_CHANGE.to_owned());
    }
    let told = fitted(&proposed, note)?;
    Ok(Built { files: proposed, told })
}

/// What the ask recorded about the moment the server's positions are from.
struct AskRecord {
    ticks: BTreeMap<u64, u64>,
    asked_ms: u64,
}

enum Stage {
    /// The ask and its collects.
    Collecting,
    /// A worker judges and reads the edit's files.
    Reading {
        work: Work<Result<Read, String>>,
        asked: AskRecord,
    },
    /// The editor is asked about each file.
    Checking {
        read: Read,
        asked: AskRecord,
    },
    /// A worker compares digests, applies the edits and stores the result.
    Building {
        work: Work<Result<Built, String>>,
    },
    Over,
}

/// The `ToolRun` of every tool that ends in a proposal: an ask, the files read, the editor asked about them, and the
/// result stored. Nothing on disk or in a buffer changes.
pub struct EditRun {
    ask: AskCollect,
    what: String,
    note: ActionNote,
    stage: Stage,
}

impl EditRun {
    /// `what` names the change in a few words, for whoever shows the proposal later.
    pub fn new(ask: AskCollect, what: String) -> EditRun {
        EditRun {
            ask,
            what,
            note: ActionNote::default(),
            stage: Stage::Collecting,
        }
    }

    fn collecting(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match self.ask.step_with_stops(event, cx) {
            Progress::Step(step) => {
                self.stage = Stage::Collecting;
                step
            }
            Progress::Failed(result) => Step::Done(result),
            Progress::Stopped(Asked::ActionGone) => {
                failure("that code action is no longer known; ask `lsp_code_actions` again")
            }
            Progress::Stopped(Asked::NoEdit { has_command }) => failure(Self::no_action_edit(has_command)),
            Progress::Stopped(_) => failure(NO_CHANGE),
            Progress::Finished(collected) => self.collected(collected, cx),
        }
    }

    fn collected(&mut self, collected: Collected, cx: &ToolCx<'_>) -> Step {
        let Collected::Done {
            edit,
            missing,
            ticks,
            asked_ms: Some(asked_ms),
            action,
            ..
        } = collected
        else {
            return failure(UNREADABLE);
        };
        if !missing.is_empty() {
            return failure(NOT_ANSWERED);
        }
        if let Some(facts) = &action {
            self.what = format!("the code action \"{}\"", facts.title);
            self.note = ActionNote {
                title: Some(facts.title.clone()),
                command_not_run: facts.has_command,
            };
        }
        let Some(edit) = edit else {
            return failure(action.map_or(NO_CHANGE, |facts| Self::no_action_edit(facts.has_command)));
        };
        let root = cx.root.to_path_buf();
        let home = cx.home.map(Path::to_path_buf);
        let writer = cx.memory.writer();
        self.stage = Stage::Reading {
            work: Work::start(move || read_files(edit, &root, home.as_deref(), writer)),
            asked: AskRecord { ticks, asked_ms },
        };
        Step::PollAfter(WORK_POLL)
    }

    fn read(&mut self, read: Read, asked: AskRecord) -> Step {
        if read.files.is_empty() {
            return failure(self.no_change());
        }
        let paths = read.files.iter().map(|file| file.path.clone()).collect();
        self.stage = Stage::Checking { read, asked };
        Step::Call {
            call: EditorCall::LspTargets { paths },
            budget: EDITOR_BUDGET,
        }
    }

    fn checked(&mut self, event: StepEvent, read: Read, asked: &AskRecord, cx: &ToolCx<'_>) -> Step {
        let StepEvent::Answer(answer) = event else {
            return failure(UNREADABLE);
        };
        let value = match answer {
            Ok(value) => value,
            Err(EditorFailure::NotAnswered) => return failure(words::NOT_READ),
            Err(EditorFailure::Unavailable(_)) if cx.reach == EditorReach::Link => {
                return failure(words::WINDOW_CLOSED)
            }
            Err(EditorFailure::Unavailable(text) | EditorFailure::Refused(text)) => return failure(text),
        };
        let states = match lsp::parse_targets(&value) {
            // A file left out of the answer would go unchecked.
            Ok(states) if states.len() == read.files.len() => states,
            Ok(_) => return failure(UNREADABLE),
            Err(why) => return failure(why),
        };
        if let Some(why) = editor_refuses(&read.files, &states, &asked.ticks, asked.asked_ms) {
            return failure(why);
        }
        let note = self.note.clone();
        self.stage = Stage::Building {
            work: Work::start(move || build(read, &states, &note)),
        };
        Step::PollAfter(WORK_POLL)
    }

    fn no_action_edit(has_command: bool) -> &'static str {
        if has_command {
            "this action runs a command of the language server, which Eitri does not run"
        } else {
            "this action changes nothing"
        }
    }

    /// The refusal for an edit that changes nothing; an action's skipped command is named when it had one.
    fn no_change(&self) -> &'static str {
        if self.note.command_not_run {
            Self::no_action_edit(true)
        } else {
            NO_CHANGE
        }
    }

    fn built(&mut self, built: Built, cx: &ToolCx<'_>) -> Step {
        let Built { files, told } = built;
        let what = std::mem::take(&mut self.what);
        let id = cx.memory.keep_proposal(cx.tab, what, files);
        Step::Done(ToolResult {
            text: answer_text(&id, &told, &self.note),
            is_error: false,
        })
    }
}

impl ToolRun for EditRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match std::mem::replace(&mut self.stage, Stage::Over) {
            Stage::Collecting => self.collecting(event, cx),
            Stage::Reading { work, asked } => match work.poll() {
                Ok(None) => {
                    self.stage = Stage::Reading { work, asked };
                    Step::PollAfter(WORK_POLL)
                }
                Ok(Some(Ok(read))) => self.read(read, asked),
                Ok(Some(Err(why))) | Err(why) => failure(why),
            },
            Stage::Checking { read, asked } => self.checked(event, read, &asked, cx),
            Stage::Building { work } => match work.poll() {
                Ok(None) => {
                    self.stage = Stage::Building { work };
                    Step::PollAfter(WORK_POLL)
                }
                Ok(Some(Ok(built))) => self.built(built, cx),
                Ok(Some(Err(why))) | Err(why) => failure(if why == NO_CHANGE {
                    self.no_change().to_owned()
                } else {
                    why
                }),
            },
            Stage::Over => failure(NOTHING_LEFT),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::catalog::declared;
    use super::super::memory::Memory;
    use super::super::step::{AgentCall, EditorSight};
    use super::super::switches::Switches;
    use super::super::testing::drive;
    use super::super::tools::lsp::{AskArgs, Budget, LspKind, LSP_BUDGET};
    use super::*;
    use crate::editor_lines::FileFormat;
    use crate::test_scratch_dir::ScratchDir;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use rmpv::Value as Rmp;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::os::unix::fs::symlink;

    /// A project on a real disk, a real store beside it.
    pub(crate) struct World {
        _scratch: ScratchDir,
        pub(crate) root: PathBuf,
        pub(crate) beside: PathBuf,
        pub(crate) memory: Memory,
    }

    impl World {
        pub(crate) fn new(label: &str) -> World {
            let scratch = ScratchDir::new("editor-tools-proposals", label);
            let base = fs::canonicalize(&*scratch).unwrap();
            let root = base.join("project");
            let beside = base.join("beside");
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(&beside).unwrap();
            fs::write(root.join("src/a.rs"), "fn foo() {}\nfoo();\n").unwrap();
            fs::write(root.join("src/b.rs"), "use foo;\n").unwrap();
            fs::write(beside.join("c.rs"), "foo\n").unwrap();
            let memory = Memory::default();
            memory.set_writer(Some(ProposalWriter::new(Some(base.join("review")), &root)));
            World {
                _scratch: scratch,
                root,
                beside,
                memory,
            }
        }

        pub(crate) fn path(&self, rel: &str) -> String {
            self.root.join(rel).to_string_lossy().into_owned()
        }

        pub(crate) fn with_cx<R>(&self, f: impl FnOnce(&ToolCx<'_>) -> R) -> R {
            self.with_cx_as(PermissionMode::Auto, ProjectTrust::Trusted, EditorReach::Own, f)
        }

        pub(crate) fn with_cx_as<R>(
            &self,
            mode: PermissionMode,
            trust: ProjectTrust,
            reach: EditorReach,
            f: impl FnOnce(&ToolCx<'_>) -> R,
        ) -> R {
            f(&ToolCx {
                mode,
                trust,
                root: &self.root,
                home: None,
                reach,
                sight: EditorSight::OnScreen,
                context: None,
                tab: 1,
                session: "session",
                call: "call",
                agent_call: AgentCall::Host,
                memory: &self.memory,
            })
        }

        /// The proposal the run ended in.
        pub(crate) fn proposal(&self, id: &str) -> std::rc::Rc<Proposal> {
            self.memory.proposal(1, id).expect("the proposal is kept")
        }

        pub(crate) fn writer(&self) -> ProposalWriter {
            self.memory.writer().expect("a writer")
        }
    }

    /// A run that gives its worker time between polls: a test's driver polls at once, and a step per spin of a real
    /// `git` would fill the memory.
    pub(crate) struct Patient(pub(crate) Box<dyn ToolRun>);

    impl ToolRun for Patient {
        fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
            let step = self.0.step(event, cx);
            if matches!(step, Step::PollAfter(_)) {
                std::thread::sleep(Duration::from_millis(1));
            }
            step
        }
    }

    pub(crate) fn map(entries: Vec<(&str, Rmp)>) -> Rmp {
        Rmp::Map(entries.into_iter().map(|(k, v)| (Rmp::from(k), v)).collect())
    }

    pub(crate) fn started() -> Rmp {
        map(vec![
            ("started", Rmp::from(true)),
            ("unsupported", Rmp::Array(Vec::new())),
        ])
    }

    pub(crate) fn text_edit(start: (u32, u32), end: (u32, u32), new_text: &str) -> Rmp {
        let position =
            |(line, character): (u32, u32)| map(vec![("line", Rmp::from(line)), ("character", Rmp::from(character))]);
        map(vec![
            ("start", position(start)),
            ("end", position(end)),
            ("new_text", Rmp::from(new_text)),
        ])
    }

    pub(crate) fn file_edit(path: &str, edits: Vec<Rmp>) -> Rmp {
        map(vec![("path", Rmp::from(path)), ("edits", Rmp::Array(edits))])
    }

    fn edit_of(files: Vec<Rmp>, resource_ops: &[&str], too_large: bool) -> Rmp {
        map(vec![
            ("encoding", Rmp::from("utf-16")),
            ("files", Rmp::Array(files)),
            (
                "resource_ops",
                Rmp::Array(resource_ops.iter().map(|op| Rmp::from(*op)).collect()),
            ),
            ("too_large", Rmp::from(too_large)),
        ])
    }

    /// An ask after every file a test wrote.
    const ASKED_AFTER_THE_WRITES: u64 = u64::MAX;

    /// A collect's answer holding `edit`, and the ticks recorded at the ask.
    fn done_with(edit: Option<Rmp>, missing: &[&str], ticks: &[(u64, u64)]) -> Rmp {
        asked_at(ASKED_AFTER_THE_WRITES, edit, missing, ticks)
    }

    /// [`done_with`] for an ask made at `asked_ms`.
    fn asked_at(asked_ms: u64, edit: Option<Rmp>, missing: &[&str], ticks: &[(u64, u64)]) -> Rmp {
        let mut entries = vec![
            ("status", Rmp::from("done")),
            ("parts", Rmp::Array(Vec::new())),
            (
                "missing",
                Rmp::Array(missing.iter().map(|name| Rmp::from(*name)).collect()),
            ),
            (
                "ticks",
                Rmp::Array(
                    ticks
                        .iter()
                        .map(|(buf, tick)| map(vec![("buf", Rmp::from(*buf)), ("tick", Rmp::from(*tick))]))
                        .collect(),
                ),
            ),
            ("asked_ms", Rmp::from(asked_ms)),
        ];
        if let Some(edit) = edit {
            entries.push(("edit", edit));
        }
        map(entries)
    }

    pub(crate) fn done(files: Vec<Rmp>) -> Rmp {
        done_with(Some(edit_of(files, &[], false)), &[], &[])
    }

    fn not_loaded() -> Rmp {
        map(vec![("found", Rmp::from(false))])
    }

    fn sha256(text: &str) -> String {
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// A loaded buffer, clean, holding `text`.
    fn loaded(buf: u64, tick: u64, text: &str) -> Rmp {
        map(vec![
            ("found", Rmp::from(true)),
            ("buf", Rmp::from(buf)),
            ("changedtick", Rmp::from(tick)),
            ("modified", Rmp::from(false)),
            ("fileformat", Rmp::from("unix")),
            ("eol", Rmp::from(true)),
            ("sha256", Rmp::from(sha256(text).as_str())),
        ])
    }

    /// `state`, its last line ending as `eol` says.
    fn ending(mut state: Rmp, eol: bool) -> Rmp {
        if let Rmp::Map(entries) = &mut state {
            entries
                .iter_mut()
                .find(|(key, _)| key.as_str() == Some("eol"))
                .unwrap()
                .1 = Rmp::from(eol);
        }
        state
    }

    /// The editor of a run: the ask starts, the first collect answers `done`, and `targets` says what is loaded.
    pub(crate) fn editor(
        done: Rmp,
        mut targets: impl FnMut(&[PathBuf]) -> Rmp,
    ) -> impl FnMut(&EditorCall) -> Result<Rmp, EditorFailure> {
        move |call| {
            Ok(match call {
                EditorCall::LspAsk(_) => started(),
                EditorCall::LspCollect { .. } => done.clone(),
                EditorCall::LspTargets { paths } => targets(paths),
                other => panic!("{other:?}"),
            })
        }
    }

    pub(crate) fn nothing_loaded(paths: &[PathBuf]) -> Rmp {
        Rmp::Array(paths.iter().map(|_| not_loaded()).collect())
    }

    /// A run of the shared machinery, for the cases that are the same for every tool that proposes.
    fn run_on(raw: &str) -> Box<dyn ToolRun> {
        let template = AskArgs {
            kind: LspKind::Format,
            ..Default::default()
        };
        let ask = AskCollect::new(template, Some(raw.to_owned()), Budget::new(LSP_BUDGET)).refuse_outside();
        Box::new(Patient(Box::new(EditRun::new(ask, "a change".to_owned()))))
    }

    fn steps_on(world: &World, done: Rmp, targets: impl FnMut(&[PathBuf]) -> Rmp) -> Vec<Step> {
        world.with_cx(|cx| drive(run_on("src/a.rs"), cx, editor(done, targets)))
    }

    pub(crate) fn the_result(steps: &[Step]) -> &ToolResult {
        match steps.last() {
            Some(Step::Done(result)) => result,
            other => panic!("the run did not end: {other:?}"),
        }
    }

    pub(crate) fn the_error(steps: &[Step]) -> &str {
        let result = the_result(steps);
        assert!(result.is_error, "{result:?}");
        &result.text
    }

    pub(crate) fn the_answer(steps: &[Step]) -> serde_json::Value {
        let result = the_result(steps);
        assert!(!result.is_error, "{result:?}");
        serde_json::from_str(&result.text).unwrap()
    }

    fn rename_in(path: &str) -> Rmp {
        file_edit(path, vec![text_edit((0, 3), (0, 6), "bar")])
    }

    /// Nothing stands for a refused edit: no proposal, and the project as it was.
    fn assert_nothing_proposed(world: &World) {
        assert!(world.memory.proposal(1, "p-1").is_none());
        assert_eq!(fs::read(world.root.join("src/a.rs")).unwrap(), b"fn foo() {}\nfoo();\n");
    }

    #[test]
    fn a_target_outside_refuses_the_whole_proposal() {
        let world = World::new("outside");
        let outside = world.beside.join("c.rs").to_string_lossy().into_owned();
        let done = done(vec![rename_in(&world.path("src/a.rs")), rename_in(&outside)]);
        let steps = steps_on(&world, done, nothing_loaded);
        assert_eq!(
            the_error(&steps),
            format!("the edit touches {outside}, outside the project; nothing was proposed")
        );
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_link_leaving_the_root_refuses_it() {
        let world = World::new("link-out");
        symlink(world.beside.join("c.rs"), world.root.join("src/c.rs")).unwrap();
        let link = world.path("src/c.rs");
        let steps = steps_on(&world, done(vec![rename_in(&link)]), nothing_loaded);
        assert_eq!(
            the_error(&steps),
            format!("the edit touches {link}, outside the project; nothing was proposed")
        );
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_uri_refuses_it() {
        let world = World::new("uri");
        let steps = steps_on(&world, done(vec![rename_in("jdt://contents/a.class")]), nothing_loaded);
        assert_eq!(
            the_error(&steps),
            "the edit touches jdt://contents/a.class, outside the project; nothing was proposed"
        );
    }

    #[test]
    fn a_resource_operation_refuses_it() {
        let world = World::new("resource");
        let edit = edit_of(
            vec![rename_in(&world.path("src/a.rs"))],
            &["rename a -> b", "delete c"],
            false,
        );
        let steps = steps_on(&world, done_with(Some(edit), &[], &[]), nothing_loaded);
        assert_eq!(
            the_error(&steps),
            "the edit creates, renames or deletes a file (rename a -> b); only edits to existing files can be proposed"
        );
        assert_nothing_proposed(&world);
    }

    #[test]
    fn too_many_files_refuse_it() {
        let world = World::new("many");
        let files = (0..=MAX_FILES)
            .map(|n| {
                let rel = format!("src/f{n}.rs");
                fs::write(world.root.join(&rel), "foo\n").unwrap();
                rename_in(&world.path(&rel))
            })
            .collect();
        let steps = steps_on(&world, done(files), |_| panic!("the editor is not asked about them"));
        assert_eq!(
            the_error(&steps),
            "the edit touches 65 files, and at most 64 can be proposed"
        );
    }

    #[test]
    fn as_many_files_as_are_allowed_make_a_proposal() {
        let world = World::new("allowed");
        let files = (0..MAX_FILES)
            .map(|n| {
                let rel = format!("src/f{n}.rs");
                fs::write(world.root.join(&rel), "fn foo\n").unwrap();
                rename_in(&world.path(&rel))
            })
            .collect();
        let answer = the_answer(&steps_on(&world, done(files), nothing_loaded));
        assert_eq!(answer["files"].as_array().unwrap().len(), MAX_FILES);
    }

    #[test]
    fn an_oversized_edit_refuses_it() {
        let world = World::new("oversized");
        let edit = edit_of(Vec::new(), &[], true);
        let steps = steps_on(&world, done_with(Some(edit), &[], &[]), nothing_loaded);
        assert_eq!(the_error(&steps), "the edit is too large to show");
    }

    #[test]
    fn two_edits_of_one_document_refuse_it() {
        let world = World::new("twice");
        let path = world.path("src/a.rs");
        let steps = steps_on(&world, done(vec![rename_in(&path), rename_in(&path)]), nothing_loaded);
        assert_eq!(the_error(&steps), "the edit names src/a.rs twice; nothing was proposed");
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_link_and_its_target_refuse_it() {
        let world = World::new("link-twice");
        symlink("a.rs", world.root.join("src/again.rs")).unwrap();
        let done = done(vec![
            rename_in(&world.path("src/again.rs")),
            rename_in(&world.path("src/a.rs")),
        ]);
        let steps = steps_on(&world, done, nothing_loaded);
        assert_eq!(the_error(&steps), "the edit names src/a.rs twice; nothing was proposed");
    }

    #[test]
    fn an_unsaved_new_file_refuses_it() {
        let world = World::new("new-file");
        let steps = steps_on(&world, done(vec![rename_in(&world.path("src/new.rs"))]), nothing_loaded);
        assert_eq!(the_error(&steps), "src/new.rs: no such file");
    }

    #[test]
    fn a_modified_target_refuses_it() {
        let world = World::new("modified");
        let targets = |_: &[PathBuf]| {
            Rmp::Array(vec![map(vec![
                ("found", Rmp::from(true)),
                ("buf", Rmp::from(3)),
                ("changedtick", Rmp::from(7)),
                ("modified", Rmp::from(true)),
                ("fileformat", Rmp::from("unix")),
                ("eol", Rmp::from(true)),
                ("sha256", Rmp::from("00")),
            ])])
        };
        let steps = steps_on(&world, done(vec![rename_in(&world.path("src/a.rs"))]), targets);
        assert_eq!(the_error(&steps), "src/a.rs has unsaved changes in your editor");
        assert_nothing_proposed(&world);
    }

    const A_RS: &str = "fn foo() {}\nfoo();";

    #[test]
    fn a_stale_buffer_refuses_it() {
        let world = World::new("stale");
        let targets = |_: &[PathBuf]| Rmp::Array(vec![loaded(3, 7, "what the buffer held a moment ago")]);
        let edit = edit_of(vec![rename_in(&world.path("src/a.rs"))], &[], false);
        let steps = steps_on(&world, done_with(Some(edit), &[], &[(3, 7)]), targets);
        assert_eq!(
            the_error(&steps),
            "src/a.rs changed on disk since your editor read it; try again in a moment"
        );
        assert_nothing_proposed(&world);
    }

    const ENDS_DIFFERENTLY: &str =
        "src/one.rs ends differently on disk than in your editor; reload it there and try again";

    /// `src/one.rs` on disk, and the steps of `edit` to it while its buffer, clean and unmoved, holds `foo`.
    fn on_one_line(world: &World, disk: &str, eol: bool, edit: Rmp) -> Vec<Step> {
        let path = world.root.join("src/one.rs");
        fs::write(&path, disk).unwrap();
        let targets = |_: &[PathBuf]| Rmp::Array(vec![ending(loaded(3, 7, "foo"), eol)]);
        let edit = edit_of(vec![file_edit(&path.to_string_lossy(), vec![edit])], &[], false);
        steps_on(world, done_with(Some(edit), &[], &[(3, 7)]), targets)
    }

    #[test]
    fn a_final_newline_added_on_disk_refuses_it() {
        let world = World::new("newline-added");
        // The formatter ends the line the buffer left open; on this disk that would leave a blank line.
        let steps = on_one_line(&world, "foo\n", false, text_edit((0, 0), (0, 3), "foo\n"));
        assert_eq!(the_error(&steps), ENDS_DIFFERENTLY);
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_final_newline_gone_from_disk_refuses_it() {
        let world = World::new("newline-gone");
        let steps = on_one_line(&world, "foo", true, text_edit((0, 3), (0, 3), ";"));
        assert_eq!(the_error(&steps), ENDS_DIFFERENTLY);
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_file_with_no_final_newline_can_be_proposed() {
        let world = World::new("no-newline");
        let answer = the_answer(&on_one_line(&world, "foo", false, text_edit((0, 3), (0, 3), ";")));
        assert_eq!(answer["proposal"], "p-1");
    }

    #[test]
    fn an_empty_file_can_be_proposed() {
        let world = World::new("empty");
        let path = world.root.join("src/empty.rs");
        fs::write(&path, "").unwrap();
        let targets = |_: &[PathBuf]| Rmp::Array(vec![loaded(3, 7, "")]);
        let edit = file_edit(&path.to_string_lossy(), vec![text_edit((0, 0), (0, 0), "foo\n")]);
        let answer = the_answer(&steps_on(&world, done(vec![edit]), targets));
        assert_eq!(answer["proposal"], "p-1");
    }

    #[test]
    fn a_buffer_changed_since_the_ask_refuses_it() {
        let world = World::new("tick");
        let targets = |_: &[PathBuf]| Rmp::Array(vec![loaded(3, 8, A_RS)]);
        let edit = edit_of(vec![rename_in(&world.path("src/a.rs"))], &[], false);
        let steps = steps_on(&world, done_with(Some(edit), &[], &[(3, 7)]), targets);
        assert_eq!(
            the_error(&steps),
            "src/a.rs changed since the language server answered; ask again"
        );
    }

    #[test]
    fn a_buffer_unchanged_since_the_ask_makes_a_proposal() {
        let world = World::new("same-tick");
        let targets = |_: &[PathBuf]| Rmp::Array(vec![loaded(3, 7, A_RS)]);
        let edit = edit_of(vec![rename_in(&world.path("src/a.rs"))], &[], false);
        let answer = the_answer(&steps_on(&world, done_with(Some(edit), &[], &[(3, 7)]), targets));
        assert_eq!(answer["proposal"], "p-1");
    }

    #[test]
    fn a_buffer_loaded_since_the_ask_is_compared_by_its_digest() {
        let world = World::new("loaded-since");
        let edit = || Some(edit_of(vec![rename_in(&world.path("src/a.rs"))], &[], false));

        let same = |_: &[PathBuf]| Rmp::Array(vec![loaded(4, 2, A_RS)]);
        let answer = the_answer(&steps_on(&world, done_with(edit(), &[], &[(3, 7)]), same));
        assert_eq!(answer["proposal"], "p-1", "no tick was recorded, and the texts agree");

        let other = |_: &[PathBuf]| Rmp::Array(vec![loaded(4, 2, "other text")]);
        let steps = steps_on(&world, done_with(edit(), &[], &[(3, 7)]), other);
        assert_eq!(
            the_error(&steps),
            "src/a.rs changed on disk since your editor read it; try again in a moment"
        );
    }

    const NOT_AT_THE_ASK: &str = "src/a.rs changed since the language server answered; ask again";

    /// An ask made before `src/a.rs` was last written.
    fn asked_before_the_write(world: &World, ticks: &[(u64, u64)]) -> Rmp {
        let edit = Some(edit_of(vec![rename_in(&world.path("src/a.rs"))], &[], false));
        asked_at(1, edit, &[], ticks)
    }

    #[test]
    fn a_file_with_no_buffer_at_the_ask_that_changed_since_refuses_it() {
        let world = World::new("saved-since-open");
        // Opened and saved since the ask: the buffer and the disk agree, and neither is what the server read.
        let opened = |_: &[PathBuf]| Rmp::Array(vec![loaded(4, 2, A_RS)]);
        let steps = steps_on(&world, asked_before_the_write(&world, &[(3, 7)]), opened);
        assert_eq!(the_error(&steps), NOT_AT_THE_ASK);
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_file_still_not_loaded_that_changed_since_the_ask_refuses_it() {
        let world = World::new("saved-since-unloaded");
        let steps = steps_on(&world, asked_before_the_write(&world, &[]), nothing_loaded);
        assert_eq!(the_error(&steps), NOT_AT_THE_ASK);
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_buffer_recorded_at_the_ask_is_judged_by_its_tick_not_the_files_time() {
        let world = World::new("recorded-tick");
        // `:w` of an unchanged buffer moves the file's time and no position.
        let unmoved = |_: &[PathBuf]| Rmp::Array(vec![loaded(3, 7, A_RS)]);
        let answer = the_answer(&steps_on(&world, asked_before_the_write(&world, &[(3, 7)]), unmoved));
        assert_eq!(answer["proposal"], "p-1");
    }

    #[test]
    fn a_collect_with_no_time_of_the_ask_proposes_nothing() {
        let world = World::new("no-time");
        let Rmp::Map(mut entries) = done(vec![rename_in(&world.path("src/a.rs"))]) else {
            unreachable!()
        };
        entries.retain(|(key, _)| key.as_str() != Some("asked_ms"));
        let steps = steps_on(&world, Rmp::Map(entries), nothing_loaded);
        assert_eq!(the_error(&steps), UNREADABLE);
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_buffer_of_another_format_than_the_file_refuses_it() {
        let world = World::new("format");
        let dos = |_: &[PathBuf]| {
            let mut state = loaded(3, 7, A_RS);
            if let Rmp::Map(entries) = &mut state {
                let format = entries.iter_mut().find(|(key, _)| key.as_str() == Some("fileformat"));
                format.unwrap().1 = Rmp::from("dos");
            }
            Rmp::Array(vec![state])
        };
        let steps = steps_on(&world, done(vec![rename_in(&world.path("src/a.rs"))]), dos);
        assert_eq!(
            the_error(&steps),
            "src/a.rs changed on disk since your editor read it; try again in a moment",
            "a dos buffer cannot hold this file's lines unchanged"
        );
    }

    #[test]
    fn a_buffer_not_loaded_is_not_compared() {
        let world = World::new("not-loaded");
        let answer = the_answer(&steps_on(
            &world,
            done(vec![rename_in(&world.path("src/a.rs"))]),
            nothing_loaded,
        ));
        assert_eq!(answer["proposal"], "p-1");
    }

    #[test]
    fn the_digests_run_on_a_worker() {
        let world = World::new("worker");
        // A digest that disagrees, so that comparing it before the worker starts ends the run at that step.
        let stale = |_: &[PathBuf]| Rmp::Array(vec![loaded(3, 7, "what the buffer held a moment ago")]);
        let steps = steps_on(&world, done(vec![rename_in(&world.path("src/a.rs"))]), stale);
        let targets = steps
            .iter()
            .position(|step| {
                matches!(
                    step,
                    Step::Call {
                        call: EditorCall::LspTargets { .. },
                        ..
                    }
                )
            })
            .expect("the editor was asked about the targets");
        assert!(
            matches!(steps[targets + 1], Step::PollAfter(_)),
            "{:?}",
            steps[targets + 1]
        );
        assert_eq!(
            the_error(&steps),
            "src/a.rs changed on disk since your editor read it; try again in a moment"
        );
    }

    #[test]
    fn an_editor_answering_for_fewer_files_is_not_believed() {
        let world = World::new("short-answer");
        let steps = steps_on(&world, done(vec![rename_in(&world.path("src/a.rs"))]), |_| {
            Rmp::Array(Vec::new())
        });
        assert_eq!(the_error(&steps), UNREADABLE);
    }

    #[test]
    fn an_editor_that_does_not_answer_about_the_targets_proposes_nothing() {
        let world = World::new("no-answer");
        let done = done(vec![rename_in(&world.path("src/a.rs"))]);
        let steps = world.with_cx(|cx| {
            drive(run_on("src/a.rs"), cx, |call| match call {
                EditorCall::LspAsk(_) => Ok(started()),
                EditorCall::LspCollect { .. } => Ok(done.clone()),
                _ => Err(EditorFailure::NotAnswered),
            })
        });
        assert_eq!(the_error(&steps), words::NOT_READ);
        assert_nothing_proposed(&world);
    }

    #[test]
    fn no_change_says_so() {
        let world = World::new("no-change");
        let path = world.path("src/a.rs");
        let no_edit = done_with(None, &[], &[]);
        assert_eq!(the_error(&steps_on(&world, no_edit, nothing_loaded)), NO_CHANGE);

        let no_files = done(Vec::new());
        assert_eq!(the_error(&steps_on(&world, no_files, nothing_loaded)), NO_CHANGE);

        let same_text = done(vec![file_edit(&path, vec![text_edit((0, 3), (0, 6), "foo")])]);
        assert_eq!(the_error(&steps_on(&world, same_text, nothing_loaded)), NO_CHANGE);
        assert_nothing_proposed(&world);
    }

    #[test]
    fn a_file_left_the_same_is_dropped() {
        let world = World::new("one-same");
        let same = file_edit(&world.path("src/b.rs"), vec![text_edit((0, 4), (0, 7), "foo")]);
        let done = done(vec![same, rename_in(&world.path("src/a.rs"))]);
        let answer = the_answer(&steps_on(&world, done, nothing_loaded));
        assert_eq!(answer["files"].as_array().unwrap().len(), 1);
        assert_eq!(answer["files"][0]["path"], "src/a.rs");
        assert_eq!(world.proposal("p-1").files.len(), 1);
    }

    #[test]
    fn a_server_that_never_answered_says_so() {
        let world = World::new("missing");
        let done = done_with(None, &["format"], &[]);
        assert_eq!(the_error(&steps_on(&world, done, nothing_loaded)), NOT_ANSWERED);
    }

    #[test]
    fn a_position_inside_a_character_names_the_file() {
        let world = World::new("inside-character");
        let path = world.root.join("src/wide.rs");
        fs::write(&path, "\u{1d4b3}\n").unwrap();
        let done = done(vec![file_edit(
            &path.to_string_lossy(),
            vec![text_edit((0, 1), (0, 2), "x")],
        )]);
        let steps = steps_on(&world, done, nothing_loaded);
        assert_eq!(
            the_error(&steps),
            "the edit to src/wide.rs points inside a character on line 1"
        );
    }

    #[test]
    fn an_edit_refused_in_a_later_file_stores_nothing_for_the_earlier_ones() {
        let world = World::new("later-file");
        let wide = world.root.join("src/wide.rs");
        fs::write(&wide, "\u{1d4b3}\n").unwrap();
        let done = done(vec![
            rename_in(&world.path("src/a.rs")),
            file_edit(&wide.to_string_lossy(), vec![text_edit((0, 1), (0, 2), "x")]),
        ]);
        let steps = steps_on(&world, done, nothing_loaded);
        assert_eq!(
            the_error(&steps),
            "the edit to src/wide.rs points inside a character on line 1"
        );
        let store = world.root.parent().unwrap().join("review");
        assert!(
            !store.exists(),
            "with review off nothing would ever trim what is stored here"
        );
    }

    #[test]
    fn a_nul_in_new_text_names_the_file() {
        let world = World::new("nul");
        let done = done(vec![file_edit(
            &world.path("src/a.rs"),
            vec![text_edit((0, 0), (0, 0), "\0")],
        )]);
        let steps = steps_on(&world, done, nothing_loaded);
        assert_eq!(
            the_error(&steps),
            "the edit to src/a.rs would leave something that is not text"
        );
        assert_nothing_proposed(&world);
    }

    #[test]
    fn overlapping_and_reversed_ranges_name_the_file() {
        let world = World::new("ranges");
        let path = world.path("src/a.rs");
        let overlap = done(vec![file_edit(
            &path,
            vec![text_edit((0, 0), (0, 4), "x"), text_edit((0, 2), (0, 6), "y")],
        )]);
        assert_eq!(
            the_error(&steps_on(&world, overlap, nothing_loaded)),
            "the edit to src/a.rs has overlapping ranges, on line 1"
        );
        let reversed = done(vec![file_edit(&path, vec![text_edit((1, 2), (1, 0), "x")])]);
        assert_eq!(
            the_error(&steps_on(&world, reversed, nothing_loaded)),
            "the edit to src/a.rs has a range that ends before it starts, on line 2"
        );
    }

    #[test]
    fn no_writer_says_so() {
        let world = World::new("no-writer");
        let edit = || done(vec![rename_in(&world.path("src/a.rs"))]);

        world.memory.set_writer(None);
        let steps = steps_on(&world, edit(), |_| {
            panic!("the editor is not asked: nothing could be kept")
        });
        assert_eq!(the_error(&steps), "no state directory to keep the proposal in");

        // A window with no state directory has a writer that can read and store nothing.
        world.memory.set_writer(Some(ProposalWriter::new(None, &world.root)));
        let steps = steps_on(&world, edit(), nothing_loaded);
        assert_eq!(the_error(&steps), "no state directory to keep the proposal in");
    }

    /// An edit replacing the file with `lines` copies of `line`.
    fn lines_of(world: &World, line: &str, lines: usize) -> Rmp {
        done(vec![file_edit(
            &world.path("src/a.rs"),
            vec![text_edit((0, 0), (2, 0), &format!("{line}\n").repeat(lines))],
        )])
    }

    #[test]
    fn an_answer_of_control_characters_keeps_its_proposal_id() {
        let world = World::new("control");
        let steps = steps_on(&world, lines_of(&world, &"\u{1}".repeat(63), 8000), nothing_loaded);
        let result = the_result(&steps);
        assert!(!result.is_error, "{result:?}");
        let cost = encoded_len(&result.text);
        assert!(cost <= ANSWER_MAX_ENCODED, "{cost} over {ANSWER_MAX_ENCODED}");
        assert!(
            cost > ANSWER_MAX_ENCODED / 10 * 9,
            "lines were not cut one by one: {cost}"
        );

        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(answer["proposal"], "p-1");
        let told = answer["files"][0]["hunks"].as_str().unwrap();
        assert!(told.ends_with('\n'), "cut at a line");
        let all = world.proposal("p-1").files[0].hunks.lines().count() as u64;
        assert_eq!(
            told.lines().count() as u64 + answer["left_out_lines"].as_u64().unwrap(),
            all
        );
        assert!(answer["left_out_lines"].as_u64().unwrap() > 0);
    }

    #[test]
    fn at_most_256_kib_of_hunks_are_told_and_the_proposal_keeps_them_all() {
        let world = World::new("cap");
        let steps = steps_on(&world, lines_of(&world, &"x".repeat(63), 8000), nothing_loaded);
        let answer = the_answer(&steps);
        let told = answer["files"][0]["hunks"].as_str().unwrap();
        assert!(told.len() <= HUNKS_TOLD_MAX, "{}", told.len());
        assert!(told.len() > HUNKS_TOLD_MAX - 100, "{}", told.len());
        assert!(told.ends_with('\n'), "cut at a line");
        let kept = &world.proposal("p-1").files[0].hunks;
        assert!(kept.len() > 400 * 1024, "{}", kept.len());
        assert_eq!(
            told.lines().count() + answer["left_out_lines"].as_u64().unwrap() as usize,
            kept.lines().count()
        );
    }

    #[test]
    fn a_short_answer_leaves_nothing_out() {
        let world = World::new("short");
        let answer = the_answer(&steps_on(
            &world,
            done(vec![rename_in(&world.path("src/a.rs"))]),
            nothing_loaded,
        ));
        assert_eq!(answer["left_out_lines"], 0);
        assert_eq!(
            answer["files"][0]["hunks"].as_str(),
            Some(world.proposal("p-1").files[0].hunks.as_str())
        );
    }

    #[test]
    fn names_with_no_room_beside_them_propose_nothing() {
        let file = |n: usize| ProposedFile {
            rel: PathBuf::from(format!("{n}{}", "\"".repeat(4000))),
            before: String::new(),
            after: String::new(),
            mode: 0o644,
            hunks: "+x\n".to_owned(),
        };
        let files: Vec<ProposedFile> = (0..MAX_FILES).map(file).collect();
        assert_eq!(fitted(&files, &ActionNote::default()).err().as_deref(), Some(TOO_LARGE));
    }

    #[test]
    fn a_later_file_is_named_with_none_of_its_lines() {
        let file = |name: &str, lines: usize| ProposedFile {
            rel: PathBuf::from(name),
            before: String::new(),
            after: String::new(),
            mode: 0o644,
            hunks: format!("+{}\n", "\u{1}".repeat(63)).repeat(lines),
        };
        let told = fitted(&[file("first", 8000), file("second", 3)], &ActionNote::default()).unwrap();
        assert_eq!(told.files[1].path, "second");
        assert_eq!(told.files[1].hunks, "");
        let first = told.files[0].hunks.lines().count() as u64;
        assert_eq!(told.left_out_lines, 8003 - first);
    }

    #[test]
    fn a_worker_past_its_limit_ends_the_run() {
        let (keep_it_busy, held) = std::sync::mpsc::channel::<()>();
        let mut work = Work::start(move || {
            let _ = held.recv();
        });
        assert!(matches!(work.poll(), Ok(None)));
        work.since = Instant::now().checked_sub(WORK_LIMIT).unwrap();
        let why = work.poll().unwrap_err();
        assert_eq!(why, "the proposal could not be made within 30 s; nothing was proposed");
        drop(keep_it_busy);
    }

    #[test]
    fn a_worker_that_died_ends_the_run() {
        let work = Work::<()>::start(|| panic!("a worker's panic stays on its thread"));
        assert_eq!(
            work.answer.recv_timeout(Duration::from_secs(60)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
            "the worker has unwound and dropped its sender"
        );
        assert_eq!(work.poll().unwrap_err(), UNREADABLE);
    }

    #[test]
    fn a_window_2_tab_and_an_n_session_may_not_propose() {
        let world = World::new("may");
        let may = |trust, reach| {
            world.with_cx_as(PermissionMode::Bypass, trust, reach, |cx| {
                may_propose(cx).map_err(|e| e.0)
            })
        };
        assert_eq!(may(ProjectTrust::Trusted, EditorReach::Own), Ok(()));
        assert_eq!(
            may(ProjectTrust::Untrusted, EditorReach::Own),
            Err(UNTRUSTED.to_owned())
        );
        assert_eq!(
            may(ProjectTrust::Untrusted, EditorReach::Link),
            Err(words::MOVE_TO_EDITOR.to_owned()),
            "the window comes first"
        );
    }

    #[test]
    fn a_project_nobody_was_asked_about_may_propose() {
        let world = World::new("nobody-asked");
        let may = world.with_cx_as(
            PermissionMode::Auto,
            ProjectTrust::NothingFound,
            EditorReach::Own,
            may_propose,
        );
        assert_eq!(may.map_err(|e| e.0), Ok(()));
        let names = declared(&Switches::on_by_default(), ProjectTrust::NothingFound);
        for name in [
            "lsp_rename",
            "format",
            "lsp_code_actions",
            "apply_code_action",
            "apply_proposal",
        ] {
            assert!(names.iter().any(|row| row.name == name), "{name}");
        }
    }

    #[test]
    fn an_n_session_does_not_declare_phase_c() {
        let names = |trust| {
            declared(&Switches::on_by_default(), trust)
                .iter()
                .map(|row| row.name)
                .collect::<Vec<_>>()
        };
        for name in ["lsp_rename", "format"] {
            assert!(names(ProjectTrust::Trusted).contains(&name), "{name}");
            assert!(!names(ProjectTrust::Untrusted).contains(&name), "{name}");
        }
    }

    #[test]
    fn the_digest_is_of_the_same_bytes_the_edit_starts_from() {
        // What the tests above call the buffer's text: a file's lines, joined, with no final newline.
        assert_eq!(
            buffer_digest(b"fn foo() {}\nfoo();\n", FileFormat::Unix).as_deref(),
            Some(sha256(A_RS).as_str())
        );
    }
}
