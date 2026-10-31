//! `apply_proposal`: write a proposal to the project. The one tool of the proposal family that changes anything.
//!
//! What the person approves is what runs: the run holds the proposal it put on the card, and writes only while that
//! same proposal is still open. A card may wait minutes, so a buffer is asked about after the answer, not before it,
//! and the writer compares every file with the bytes the proposal was computed from. That compare cannot stop a replay
//! of a write that landed once the person has restored those bytes, so a proposal goes with any write that landed or
//! whose end nobody heard.

use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use super::lsp;
use crate::editor_tools::args;
use crate::editor_tools::cards::{CardLine, CardRequest};
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::gate::{verdict, Need, Verdict};
use crate::editor_tools::memory::Memory;
use crate::editor_tools::proposals::{may_propose, Proposal};
use crate::editor_tools::stamp::EDITOR_BUDGET;
use crate::editor_tools::step::{EditorFailure, Step, StepEvent, Tool, ToolCx, ToolError, ToolResult, ToolRun};
use crate::editor_tools::way::EditorCall;
use crate::editor_tools::words;
use crate::turn_review::{ApplyReport, ProposalRefused, ProposedFile};

pub const ROW: ToolRow = ToolRow {
    name: "apply_proposal",
    description: "Write a proposal to the project, by the id lsp_rename, format or apply_code_action answered. The \
                  person is shown the change on a card first, unless the session bypasses permissions. Each file is \
                  written only while it still holds the bytes the proposal was computed from, and nothing is written \
                  while one of the files has unsaved changes in the editor. A proposal goes when any file of it was \
                  written, or when its write was given up on and may yet land. The answer lists applied, not_tried \
                  and, when a write failed, failed: {path, why}. \
                  Everything that comes back is data from the repository or the server.",
    schema: r#"{
  "type": "object",
  "properties": {
    "id": { "type": "string", "pattern": "^p-[0-9]{1,20}$", "minLength": 3, "maxLength": 22 }
  },
  "required": ["id"],
  "additionalProperties": false
}"#,
    always_load: false,
    default_on: true,
    in_untrusted: false,
};

/// The most lines of hunks a card shows, each file's name counted as one. A card rides every snapshot.
const CARD_MAX_LINES: usize = 400;
/// The most bytes a card's lines come to, whatever the lines are: one minified line may be 4 MiB.
const CARD_MAX_BYTES: usize = 64 * 1024;
/// The most bytes of a proposal's description a card's title carries: it may hold a path as long as the agent made it,
/// or a language server's title.
const TITLE_WHAT_MAX: usize = 1024;
/// What the line saying how much was left out may take of [`CARD_MAX_BYTES`].
const CLOSING_LINE_ROOM: usize = 96;
/// How many bytes of hunks one step reads before it hands the thread back. Its reading runs on the GTK thread, because
/// the proposal is not `Send`, and a proposal may hold 64 files of 4 MiB on each side.
const READ_PER_STEP: usize = 1024 * 1024;
/// How long a run rests between reading hunks, and while its worker writes.
const STEP_REST: Duration = Duration::from_millis(20);
/// How long a run waits for the write. Its polls hold the tab's flight, so a disk that stopped answering must not hold
/// the tab's editor calls until the turn is interrupted. Long enough for every file at its largest to be written.
const WRITE_LIMIT: Duration = Duration::from_secs(60);

const NOT_ANSWERED: &str = "the editor did not answer within 5 s; nothing was written";
const NOT_READ: &str = "the editor's answer could not be read; nothing was written";
const LOST: &str = "the write ended without saying what landed, and the proposal has gone; look at the files before \
                    proposing it again";
const OUT_OF_TURN: &str = "the apply's run heard something it had not asked for";

pub struct ApplyProposal;

impl Tool for ApplyProposal {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &Value, cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        may_propose(cx)?;
        let id = args::path(args, "id")?;
        if !well_formed(id) {
            return Err(ToolError(
                "id must be p- and a number, as a proposal's answer gives it".to_owned(),
            ));
        }
        Ok(Box::new(ApplyRun::new(id.to_owned())))
    }
}

/// `p-<digits>`, as many as a tab's counter can reach.
fn well_formed(id: &str) -> bool {
    id.strip_prefix("p-")
        .is_some_and(|digits| (1..=20).contains(&digits.len()) && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

fn failure(text: impl Into<String>) -> Step {
    Step::Done(ToolResult {
        text: text.into(),
        is_error: true,
    })
}

fn counted(n: u64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// What a card shows of a proposal's hunks, built a file at a time. Lines are shown whole and in order until one does
/// not fit; nothing after it is shown, so the card never has a hole in it.
struct Cut {
    title: String,
    names: String,
    code: Vec<CardLine>,
    lines_left: usize,
    bytes_left: usize,
    /// What the hunks may take in all. A line longer than this has no other way onto a card than a cut inside it.
    bytes_most: usize,
    whole_so_far: bool,
    left_out_lines: u64,
    left_out_files: u64,
}

impl Cut {
    /// A cut with what is left of the card after its title and the line naming every file.
    fn after(title: String, names: String) -> Cut {
        let bytes_most = (CARD_MAX_BYTES - CLOSING_LINE_ROOM - title.len()).saturating_sub(names.len());
        Cut {
            title,
            names,
            code: Vec::new(),
            lines_left: CARD_MAX_LINES,
            bytes_left: bytes_most,
            bytes_most,
            whole_so_far: true,
            left_out_lines: 0,
            left_out_files: 0,
        }
    }

    /// How much of `hunks`, from its start, fits in `bytes` and `lines`: the bytes and whole lines of it.
    fn fitting(&self, hunks: &str, mut bytes: usize, mut lines: usize) -> (usize, usize) {
        let mut kept = 0;
        let mut whole_lines = 0;
        for line in hunks.split_inclusive('\n') {
            if lines == 0 {
                break;
            }
            if line.len() > bytes {
                if line.len() > self.bytes_most {
                    kept += line.floor_char_boundary(bytes);
                }
                break;
            }
            bytes -= line.len();
            lines -= 1;
            kept += line.len();
            whole_lines += 1;
        }
        (kept, whole_lines)
    }

    fn add(&mut self, file: &ProposedFile) {
        let name = file.rel.to_string_lossy();
        let head = name.len() + 1;
        let fits_a_head = self.whole_so_far && self.lines_left >= 2 && self.bytes_left > head;
        let (kept, whole_lines) = if fits_a_head {
            self.fitting(&file.hunks, self.bytes_left - head, self.lines_left - 1)
        } else {
            (0, 0)
        };
        let shown = &file.hunks[..kept];
        if !shown.is_empty() {
            self.bytes_left -= head + kept;
            self.lines_left -= 1 + shown.split_inclusive('\n').count();
            let text = shown.strip_suffix('\n').unwrap_or(shown);
            self.code.push(CardLine::Code(format!("{name}\n{text}")));
        }
        let lines = file.hunks.split_inclusive('\n').count();
        if lines > whole_lines {
            self.whole_so_far = false;
            self.left_out_lines += (lines - whole_lines) as u64;
            self.left_out_files += 1;
        }
    }

    fn into_card(self, proposal: String) -> CardRequest {
        let mut body = Vec::with_capacity(self.code.len() + 2);
        body.push(CardLine::Plain(self.names));
        body.extend(self.code);
        if self.left_out_files > 0 {
            let lines = if self.left_out_lines == 1 { "line" } else { "lines" };
            body.push(CardLine::Plain(format!(
                "{} more {lines} in {}; Ctrl+g shows each file",
                self.left_out_lines,
                counted(self.left_out_files, "file"),
            )));
        }
        CardRequest {
            proposal: Some(proposal),
            ..CardRequest::new(self.title, body)
        }
    }
}

fn title_of(proposal: &Proposal) -> String {
    let what = &proposal.what;
    let shown = &what[..what.floor_char_boundary(TITLE_WHAT_MAX)];
    let mark = if shown.len() < what.len() { "..." } else { "" };
    let files = counted(proposal.files.len() as u64, "file");
    format!("Apply {shown}{mark} to {files}?")
}

/// Every path of the proposal, always whole: no file is written that the card does not name.
fn names_of(proposal: &Proposal) -> String {
    let names: Vec<_> = proposal.files.iter().map(|file| file.rel.to_string_lossy()).collect();
    format!("Files: {}", names.join(", "))
}

/// `files` with the blobs a write reads, and not their hunks, which it does not.
fn to_write(files: &[ProposedFile]) -> Vec<ProposedFile> {
    files
        .iter()
        .map(|file| ProposedFile {
            rel: file.rel.clone(),
            before: file.before.clone(),
            after: file.after.clone(),
            mode: file.mode,
            hunks: String::new(),
        })
        .collect()
}

#[derive(Serialize)]
struct Failed {
    path: String,
    why: String,
}

#[derive(Serialize)]
struct Answer {
    applied: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed: Option<Failed>,
    not_tried: Vec<String>,
}

/// A write under way, on the tab's behalf. A worker cannot be stopped, so whatever ends the waiting before the report
/// is heard (the limit, a lost worker, the agent giving the call up) leaves a write that may land later with nobody
/// listening. Dropped unheard, this takes the proposal with it.
struct Landing {
    memory: Memory,
    tab: u64,
    id: String,
    nothing_landed: bool,
}

impl Landing {
    fn new(memory: &Memory, tab: u64, id: &str) -> Landing {
        Landing {
            memory: memory.clone(),
            tab,
            id: id.to_owned(),
            nothing_landed: false,
        }
    }

    /// The report names no file as written, so the proposal stays. A failed directory sync or put-back also reports
    /// its file as failed after bytes changed; the proposal outlives that, and the next apply's compare with
    /// `before` is what refuses it.
    fn nothing_landed(mut self) {
        self.nothing_landed = true;
    }
}

impl Drop for Landing {
    fn drop(&mut self) {
        if !self.nothing_landed {
            self.memory.spend_proposal(self.tab, &self.id);
        }
    }
}

fn answer_of(report: ApplyReport) -> Step {
    let shown = |paths: Vec<std::path::PathBuf>| -> Vec<String> {
        paths.iter().map(|path| path.to_string_lossy().into_owned()).collect()
    };
    let is_error = report.failed.is_some() || !report.not_tried.is_empty();
    let answer = Answer {
        applied: shown(report.written),
        failed: report.failed.map(|(path, why)| Failed {
            path: path.to_string_lossy().into_owned(),
            why,
        }),
        not_tried: shown(report.not_tried),
    };
    Step::Done(ToolResult {
        text: serde_json::to_string(&answer).expect("strings serialize"),
        is_error,
    })
}

enum Stage {
    /// Nothing has run.
    Looking,
    /// The card is being cut, `next` being the first file not read yet.
    Cutting {
        proposal: Rc<Proposal>,
        cut: Cut,
        next: usize,
    },
    OnCard {
        proposal: Rc<Proposal>,
    },
    /// The editor is asked about each file.
    Checking {
        proposal: Rc<Proposal>,
    },
    Writing {
        landing: Landing,
        report: Receiver<Result<ApplyReport, ProposalRefused>>,
        since: Instant,
    },
    Over,
}

struct ApplyRun {
    id: String,
    stage: Stage,
}

impl ApplyRun {
    fn new(id: String) -> ApplyRun {
        ApplyRun {
            id,
            stage: Stage::Looking,
        }
    }

    fn no_proposal(&self) -> Step {
        failure(format!(
            "no proposal `{}` in this tab; it may have been replaced by newer ones, or the tab moved",
            self.id
        ))
    }

    fn look(&mut self, cx: &ToolCx<'_>) -> Step {
        let Some(proposal) = cx.memory.proposal(cx.tab, &self.id) else {
            return self.no_proposal();
        };
        match verdict(Need::Applier, cx.mode, cx.trust) {
            Verdict::Allow => self.check(proposal, cx),
            Verdict::Card => {
                let cut = Cut::after(title_of(&proposal), names_of(&proposal));
                self.cut(proposal, cut, 0)
            }
        }
    }

    fn cut(&mut self, proposal: Rc<Proposal>, mut cut: Cut, mut next: usize) -> Step {
        let mut read = 0;
        while let Some(file) = proposal.files.get(next) {
            if read >= READ_PER_STEP {
                self.stage = Stage::Cutting { proposal, cut, next };
                return Step::PollAfter(STEP_REST);
            }
            read += file.hunks.len();
            cut.add(file);
            next += 1;
        }
        let request = cut.into_card(proposal.id.clone());
        self.stage = Stage::OnCard { proposal };
        Step::Ask(request)
    }

    /// The person's answer. The card may have waited minutes: what it showed is applied only while it is still the
    /// tab's open proposal, so an apply that got there first, or newer proposals, end this one.
    fn card_answered(&mut self, allowed: bool, proposal: Rc<Proposal>, cx: &ToolCx<'_>) -> Step {
        if !allowed {
            return failure(words::NOT_ALLOWED);
        }
        let still_open = cx
            .memory
            .proposal(cx.tab, &self.id)
            .is_some_and(|now| Rc::ptr_eq(&now, &proposal));
        if !still_open {
            return self.no_proposal();
        }
        self.check(proposal, cx)
    }

    fn check(&mut self, proposal: Rc<Proposal>, cx: &ToolCx<'_>) -> Step {
        let paths = proposal.files.iter().map(|file| cx.root.join(&file.rel)).collect();
        self.stage = Stage::Checking { proposal };
        Step::Call {
            call: EditorCall::LspTargets { paths },
            budget: EDITOR_BUDGET,
        }
    }

    fn checked(&mut self, answer: Result<rmpv::Value, EditorFailure>, proposal: Rc<Proposal>, cx: &ToolCx<'_>) -> Step {
        let value = match answer {
            Ok(value) => value,
            Err(EditorFailure::NotAnswered) => return failure(NOT_ANSWERED),
            Err(EditorFailure::Unavailable(text) | EditorFailure::Refused(text)) => return failure(text),
        };
        // A file left out of the answer would go unchecked.
        let states = match lsp::parse_targets(&value) {
            Ok(states) if states.len() == proposal.files.len() => states,
            _ => return failure(NOT_READ),
        };
        let unsaved = proposal
            .files
            .iter()
            .zip(&states)
            .find(|(_, state)| state.found && state.modified);
        if let Some((file, _)) = unsaved {
            return failure(format!("{} has unsaved changes in your editor", file.rel.display()));
        }
        let Some(writer) = cx.memory.writer() else {
            return failure(ProposalRefused::NoStore.to_string());
        };
        let session = cx.session.to_owned();
        let files = to_write(&proposal.files);
        self.stage = Stage::Writing {
            landing: Landing::new(cx.memory, cx.tab, &proposal.id),
            report: lsp::worker(move || writer.apply(&session, &files)),
            since: Instant::now(),
        };
        Step::PollAfter(STEP_REST)
    }

    /// A proposal stays only on a report that nothing landed: once something has, its `before` bytes no longer hold,
    /// and every way out of here with no report drops `landing`, which takes the proposal.
    fn polled(
        &mut self,
        landing: Landing,
        report: Receiver<Result<ApplyReport, ProposalRefused>>,
        since: Instant,
    ) -> Step {
        match report.try_recv() {
            Ok(Ok(report)) => {
                if report.written.is_empty() {
                    landing.nothing_landed();
                }
                answer_of(report)
            }
            Ok(Err(refused)) => {
                landing.nothing_landed();
                failure(refused.to_string())
            }
            Err(TryRecvError::Empty) if since.elapsed() >= WRITE_LIMIT => failure(format!(
                "the write did not finish within {} s and may still land, and the proposal has gone; look at the \
                 files before proposing it again",
                WRITE_LIMIT.as_secs()
            )),
            Err(TryRecvError::Empty) => {
                self.stage = Stage::Writing { landing, report, since };
                Step::PollAfter(STEP_REST)
            }
            Err(TryRecvError::Disconnected) => failure(LOST),
        }
    }
}

impl ToolRun for ApplyRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match (std::mem::replace(&mut self.stage, Stage::Over), event) {
            (Stage::Looking, StepEvent::Start) => self.look(cx),
            (Stage::Cutting { proposal, cut, next }, StepEvent::Poll) => self.cut(proposal, cut, next),
            (Stage::OnCard { proposal }, StepEvent::Card(answer)) => self.card_answered(answer.allowed(), proposal, cx),
            (Stage::Checking { proposal }, StepEvent::Answer(answer)) => self.checked(answer, proposal, cx),
            (Stage::Writing { landing, report, since }, StepEvent::Poll) => self.polled(landing, report, since),
            _ => failure(OUT_OF_TURN),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::cards::{CardDecision, CardOrigin};
    use crate::editor_tools::catalog::declared;
    use crate::editor_tools::proposals::tests::{map, nothing_loaded, the_answer, the_error, Patient, World};
    use crate::editor_tools::step::EditorReach;
    use crate::editor_tools::switches::Switches;
    use crate::editor_tools::testing::drive_with_cards;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use rmpv::Value as Rmp;
    use serde_json::json;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use ProjectTrust::{Trusted, Untrusted};

    const MODES: [PermissionMode; 3] = [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass];
    const A_RS: &[u8] = b"fn foo() {}\nfoo();\n";
    const B_RS: &[u8] = b"use foo;\n";

    /// A proposal of the store's own making, renaming `foo` in both of the world's files.
    fn rename_both(world: &World) -> String {
        propose(
            world,
            &[("src/a.rs", "fn bar() {}\nbar();\n"), ("src/b.rs", "use bar;\n")],
        )
    }

    fn propose(world: &World, changes: &[(&str, &str)]) -> String {
        let writer = world.writer();
        let files = changes
            .iter()
            .map(|(rel, after)| {
                let rel = Path::new(rel);
                let now = writer.read(rel).unwrap();
                writer.store(rel, &now, after.as_bytes()).unwrap()
            })
            .collect();
        world.memory.keep_proposal(1, "rename foo to bar".to_owned(), files)
    }

    /// A proposal only a card reads: its blobs are not in any store.
    fn proposal_with_hunks(world: &World, files: &[(&str, String)]) -> String {
        let files = files
            .iter()
            .map(|(rel, hunks)| ProposedFile {
                rel: PathBuf::from(rel),
                before: "no blob".to_owned(),
                after: "no blob".to_owned(),
                mode: 0o644,
                hunks: hunks.clone(),
            })
            .collect();
        world.memory.keep_proposal(1, "rename foo to bar".to_owned(), files)
    }

    fn allow(_: &CardRequest) -> (CardDecision, CardOrigin) {
        (CardDecision::Allow, CardOrigin::Key)
    }

    fn deny(_: &CardRequest) -> (CardDecision, CardOrigin) {
        (CardDecision::Deny, CardOrigin::Key)
    }

    fn no_card(request: &CardRequest) -> (CardDecision, CardOrigin) {
        panic!("a card nothing should raise: {}", request.title)
    }

    fn apply_with(
        world: &World,
        id: &str,
        mode: PermissionMode,
        mut targets: impl FnMut(&[PathBuf]) -> Result<Rmp, EditorFailure>,
        cards: impl FnMut(&CardRequest) -> (CardDecision, CardOrigin),
    ) -> Vec<Step> {
        world.with_cx_as(mode, Trusted, EditorReach::Own, |cx| {
            let run = ApplyProposal.begin(&json!({ "id": id }), cx).unwrap();
            drive_with_cards(
                Box::new(Patient(run)),
                cx,
                |call| match call {
                    EditorCall::LspTargets { paths } => targets(paths),
                    other => panic!("{other:?}"),
                },
                cards,
            )
        })
    }

    fn apply(
        world: &World,
        id: &str,
        mode: PermissionMode,
        cards: impl FnMut(&CardRequest) -> (CardDecision, CardOrigin),
    ) -> Vec<Step> {
        apply_with(world, id, mode, |paths| Ok(nothing_loaded(paths)), cards)
    }

    /// A clean or unsaved buffer. The check reads only `found` and `modified`, and the rest must parse.
    fn buffer(modified: bool) -> Rmp {
        map(vec![
            ("found", true.into()),
            ("modified", modified.into()),
            ("buf", 3.into()),
            ("changedtick", 7.into()),
            ("sha256", "00".into()),
            ("fileformat", "unix".into()),
            ("eol", true.into()),
        ])
    }

    fn cards_of(steps: &[Step]) -> Vec<&CardRequest> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Ask(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    /// The card a denied run raised.
    fn card_of(world: &World, id: &str) -> Vec<CardLine> {
        let steps = apply(world, id, PermissionMode::Manual, deny);
        cards_of(&steps)[0].body.clone()
    }

    fn code_lines(body: &[CardLine]) -> Vec<&str> {
        body.iter()
            .filter_map(|line| match line {
                CardLine::Code(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn plain_lines(body: &[CardLine]) -> Vec<&str> {
        body.iter()
            .filter_map(|line| match line {
                CardLine::Plain(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn line_count(body: &[CardLine]) -> usize {
        code_lines(body).iter().map(|text| text.lines().count()).sum()
    }

    fn byte_count(body: &[CardLine]) -> usize {
        body.iter()
            .map(|line| match line {
                CardLine::Plain(text) | CardLine::Code(text) => text.len(),
                other => panic!("{other:?}"),
            })
            .sum()
    }

    fn hunks_of(lines: usize) -> String {
        let body: String = (0..lines).map(|n| format!("+line {n}\n")).collect();
        format!("@@ -0,0 +1,{lines} @@\n{body}")
    }

    fn assert_nothing_written(world: &World) {
        assert_eq!(fs::read(world.root.join("src/a.rs")).unwrap(), A_RS);
        assert_eq!(fs::read(world.root.join("src/b.rs")).unwrap(), B_RS);
    }

    #[test]
    fn manual_and_auto_show_a_card_and_bypass_does_not() {
        for mode in MODES {
            for trust in [Trusted, Untrusted] {
                let world = World::new("modes");
                rename_both(&world);
                let steps = world.with_cx_as(mode, trust, EditorReach::Own, |cx| {
                    let run = Box::new(Patient(Box::new(ApplyRun::new("p-1".to_owned()))));
                    let targets = |call: &EditorCall| match call {
                        EditorCall::LspTargets { paths } => Ok(nothing_loaded(paths)),
                        other => panic!("{other:?}"),
                    };
                    drive_with_cards(run, cx, targets, deny)
                });
                let carded = verdict(Need::Applier, mode, trust) == Verdict::Card;
                assert_eq!(cards_of(&steps).len(), usize::from(carded), "{mode:?} {trust:?}");
                assert_eq!(carded, mode != PermissionMode::Bypass, "{mode:?} {trust:?}");
            }
        }
    }

    #[test]
    fn manual_never_allows_what_auto_cards() {
        let world = World::new("manual-auto");
        rename_both(&world);
        let auto = apply(&world, "p-1", PermissionMode::Auto, deny);
        let manual = apply(&world, "p-1", PermissionMode::Manual, deny);
        assert_eq!(cards_of(&auto).len(), 1);
        assert_eq!(cards_of(&manual).len(), 1);
    }

    #[test]
    fn a_deny_writes_nothing_and_keeps_the_proposal() {
        let world = World::new("deny");
        let id = rename_both(&world);
        let steps = apply_with(
            &world,
            &id,
            PermissionMode::Manual,
            |_| panic!("the editor was asked"),
            deny,
        );
        assert_eq!(the_error(&steps), "the person did not allow it");
        assert_nothing_written(&world);
        assert!(world.memory.proposal(1, &id).is_some());
    }

    #[test]
    fn the_card_shows_the_hunks_as_text() {
        let world = World::new("card-text");
        let id = rename_both(&world);
        let steps = apply(&world, &id, PermissionMode::Manual, deny);
        let card = cards_of(&steps)[0];
        assert_eq!(card.title, "Apply rename foo to bar to 2 files?");
        assert_eq!(card.proposal.as_deref(), Some(id.as_str()));
        assert!(card.binding.is_none());
        assert_eq!(card.body[0], CardLine::Plain("Files: src/a.rs, src/b.rs".to_owned()));
        let code = code_lines(&card.body);
        assert_eq!(code.len(), 2);
        assert!(code[0].contains("-fn foo() {}\n"), "{}", code[0]);
        assert!(code[0].contains("+fn bar() {}\n"), "{}", code[0]);
        assert!(code[1].contains("-use foo;\n+use bar;"), "{}", code[1]);
        assert_eq!(card.body.len(), 3, "nothing was cut, so nothing says so");
    }

    #[test]
    fn the_title_counts_one_file() {
        let world = World::new("card-title");
        let id = propose(&world, &[("src/b.rs", "use bar;\n")]);
        let steps = apply(&world, &id, PermissionMode::Manual, deny);
        assert_eq!(cards_of(&steps)[0].title, "Apply rename foo to bar to 1 file?");
    }

    #[test]
    fn a_long_description_is_held_to_the_title_and_counts_against_the_card() {
        let world = World::new("card-what");
        // Three bytes a character, so a cut at a byte count alone would land inside one.
        let what = format!("format {}", "中".repeat(40 * 1024));
        let hunks = format!("@@ -1 +1 @@\n-{}\n+two\n", "x".repeat(1024 * 1024));
        let file = ProposedFile {
            rel: PathBuf::from("src/a.rs"),
            before: "no blob".to_owned(),
            after: "no blob".to_owned(),
            mode: 0o644,
            hunks,
        };
        let id = world.memory.keep_proposal(1, what, vec![file]);
        let steps = apply(&world, &id, PermissionMode::Manual, deny);
        let card = cards_of(&steps)[0];
        assert!(card.title.len() <= TITLE_WHAT_MAX + 64, "{}", card.title.len());
        assert!(card.title.starts_with("Apply format 中"), "{}", card.title);
        assert!(card.title.ends_with("... to 1 file?"), "{}", card.title);
        let whole = card.title.len() + byte_count(&card.body);
        assert!(whole <= CARD_MAX_BYTES, "{whole}");
    }

    #[test]
    fn the_card_names_every_file_past_the_cut() {
        let world = World::new("card-names");
        let names: Vec<String> = (0..8).map(|n| format!("src/f{n}.rs")).collect();
        let files: Vec<(&str, String)> = names.iter().map(|name| (name.as_str(), hunks_of(100))).collect();
        let id = proposal_with_hunks(&world, &files);
        let body = card_of(&world, &id);
        let code = code_lines(&body);
        assert!(code.len() < names.len(), "{}", code.len());
        let CardLine::Plain(first) = &body[0] else {
            panic!("{:?}", body[0]);
        };
        for name in &names {
            assert!(first.contains(name.as_str()), "{name} is not in {first}");
        }
    }

    #[test]
    fn a_list_of_names_past_the_card_is_still_whole() {
        let world = World::new("card-long-names");
        let names: Vec<String> = (0..64).map(|n| format!("src/{n:0>2000}.rs")).collect();
        let files: Vec<(&str, String)> = names.iter().map(|name| (name.as_str(), hunks_of(1))).collect();
        let id = proposal_with_hunks(&world, &files);
        let body = card_of(&world, &id);
        assert!(code_lines(&body).is_empty());
        let plain = plain_lines(&body);
        for name in &names {
            assert!(plain[0].contains(name.as_str()));
        }
        assert_eq!(plain[1], "128 more lines in 64 files; Ctrl+g shows each file");
    }

    #[test]
    fn each_code_line_names_its_file() {
        let world = World::new("card-heads");
        let id = rename_both(&world);
        let body = card_of(&world, &id);
        let heads: Vec<_> = code_lines(&body)
            .iter()
            .map(|text| text.lines().next().unwrap())
            .collect();
        assert_eq!(heads, ["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn one_huge_line_is_held_to_the_byte_cap() {
        let world = World::new("card-huge");
        // Three bytes a character, so a cut at a byte count alone would land inside one.
        let hunks = format!("@@ -1 +1 @@\n-{}\n+two\n", "中".repeat(1024 * 1024 / 3));
        let id = proposal_with_hunks(&world, &[("src/a.rs", hunks)]);
        let body = card_of(&world, &id);
        assert!(byte_count(&body) <= CARD_MAX_BYTES, "{}", byte_count(&body));
        let code = code_lines(&body);
        assert!(
            code[0].len() > CARD_MAX_BYTES - 200,
            "the start of the line is shown: {}",
            code[0].len()
        );
        assert!(code[0].ends_with('中'));
        assert_eq!(
            plain_lines(&body)[1],
            "2 more lines in 1 file; Ctrl+g shows each file",
            "the line that was cut, and the one after it"
        );
    }

    #[test]
    fn a_line_that_could_fit_on_its_own_is_never_cut() {
        let world = World::new("card-whole");
        let line = "x".repeat(40 * 1024);
        let hunks = format!("@@ -1,2 +1,2 @@\n-{line}\n+{line}\n");
        let id = proposal_with_hunks(&world, &[("src/a.rs", hunks)]);
        let body = card_of(&world, &id);
        let code = code_lines(&body);
        assert!(
            code[0].ends_with(&format!("-{line}")),
            "cut at a line, never inside one that could fit"
        );
        assert_eq!(plain_lines(&body)[1], "1 more line in 1 file; Ctrl+g shows each file");
    }

    #[test]
    fn a_long_proposal_says_how_much_is_left_out() {
        let world = World::new("card-long");
        let id = proposal_with_hunks(&world, &[("src/a.rs", hunks_of(1000)), ("src/b.rs", hunks_of(9))]);
        let body = card_of(&world, &id);
        assert_eq!(line_count(&body), CARD_MAX_LINES, "the name and 399 lines of hunks");
        assert_eq!(code_lines(&body).len(), 1);
        assert_eq!(
            plain_lines(&body)[1],
            "612 more lines in 2 files; Ctrl+g shows each file",
            "1001 less 399 of the first file, and the ten of the second"
        );
    }

    #[test]
    fn a_large_proposal_is_read_over_several_steps() {
        let world = World::new("card-steps");
        let big = hunks_of(READ_PER_STEP / 10);
        assert!(big.len() > READ_PER_STEP);
        let id = proposal_with_hunks(&world, &[("src/a.rs", big.clone()), ("src/b.rs", big)]);
        let steps = apply(&world, &id, PermissionMode::Manual, deny);
        assert!(matches!(steps[0], Step::PollAfter(_)), "{:?}", steps[0]);
        assert!(matches!(steps[1], Step::Ask(_)), "{:?}", steps[1]);
    }

    #[test]
    fn an_allowed_card_writes_each_file() {
        let world = World::new("allow");
        let id = rename_both(&world);
        let steps = apply(&world, &id, PermissionMode::Manual, allow);
        assert_eq!(
            the_answer(&steps),
            json!({"applied": ["src/a.rs", "src/b.rs"], "not_tried": []})
        );
        assert_eq!(fs::read(world.root.join("src/a.rs")).unwrap(), b"fn bar() {}\nbar();\n");
        assert_eq!(fs::read(world.root.join("src/b.rs")).unwrap(), b"use bar;\n");
        assert!(world.memory.proposal(1, &id).is_none());
    }

    #[test]
    fn bypass_writes_with_no_card() {
        let world = World::new("bypass");
        let id = rename_both(&world);
        let steps = apply(&world, &id, PermissionMode::Bypass, no_card);
        assert_eq!(the_answer(&steps)["applied"], json!(["src/a.rs", "src/b.rs"]));
        assert_eq!(fs::read(world.root.join("src/b.rs")).unwrap(), b"use bar;\n");
    }

    #[test]
    fn bypass_still_asks_about_the_buffers() {
        let world = World::new("bypass-buffers");
        let id = rename_both(&world);
        let steps = apply_with(
            &world,
            &id,
            PermissionMode::Bypass,
            |_| Ok(Rmp::Array(vec![buffer(false), buffer(true)])),
            no_card,
        );
        assert_eq!(the_error(&steps), "src/b.rs has unsaved changes in your editor");
        assert_nothing_written(&world);
    }

    #[test]
    fn a_file_changed_after_the_proposal_is_refused() {
        let world = World::new("changed");
        let id = rename_both(&world);
        fs::write(world.root.join("src/b.rs"), "use other;\n").unwrap();
        let steps = apply(&world, &id, PermissionMode::Manual, allow);
        let refused = the_error(&steps);
        assert!(refused.contains("src/b.rs"), "{refused}");
        assert!(refused.contains("changed"), "{refused}");
        assert_eq!(
            fs::read(world.root.join("src/a.rs")).unwrap(),
            A_RS,
            "every file is looked at first"
        );
        assert!(world.memory.proposal(1, &id).is_some());
    }

    #[test]
    fn a_buffer_modified_after_the_card_is_refused() {
        let world = World::new("modified");
        let id = rename_both(&world);
        let answered = Cell::new(false);
        let steps = apply_with(
            &world,
            &id,
            PermissionMode::Manual,
            |_| {
                assert!(
                    answered.get(),
                    "the buffers are asked about after the answer, not before it"
                );
                Ok(Rmp::Array(vec![buffer(true), buffer(false)]))
            },
            |request| {
                answered.set(true);
                allow(request)
            },
        );
        assert_eq!(the_error(&steps), "src/a.rs has unsaved changes in your editor");
        assert_nothing_written(&world);
        assert!(world.memory.proposal(1, &id).is_some());
    }

    #[test]
    fn an_editor_that_does_not_answer_writes_nothing() {
        let world = World::new("no-answer");
        let id = rename_both(&world);
        let steps = apply_with(
            &world,
            &id,
            PermissionMode::Bypass,
            |_| Err(EditorFailure::NotAnswered),
            no_card,
        );
        assert_eq!(
            the_error(&steps),
            "the editor did not answer within 5 s; nothing was written"
        );
        assert_nothing_written(&world);
        assert!(world.memory.proposal(1, &id).is_some());
    }

    #[test]
    fn no_editor_at_all_writes_nothing() {
        let world = World::new("no-editor");
        let id = rename_both(&world);
        let steps = apply_with(
            &world,
            &id,
            PermissionMode::Bypass,
            |_| Err(EditorFailure::Unavailable(words::NO_EDITOR.to_owned())),
            no_card,
        );
        assert_eq!(the_error(&steps), words::NO_EDITOR);
        assert_nothing_written(&world);
    }

    #[test]
    fn an_editor_answering_for_fewer_files_is_not_believed() {
        let world = World::new("short-answer");
        let id = rename_both(&world);
        let steps = apply_with(
            &world,
            &id,
            PermissionMode::Bypass,
            |_| Ok(Rmp::Array(vec![buffer(false)])),
            no_card,
        );
        assert_eq!(the_error(&steps), NOT_READ);
        assert_nothing_written(&world);
    }

    #[test]
    fn a_partial_write_is_reported_and_spends_the_proposal() {
        let world = World::new("partial");
        let stuck = world.root.join("stuck");
        fs::create_dir(&stuck).unwrap();
        fs::write(stuck.join("c.rs"), "use foo;\n").unwrap();
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(stuck.join("probe"), "").is_ok() {
            println!("this user writes through a read-only directory; skipped");
            fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let id = propose(
            &world,
            &[
                ("src/a.rs", "fn bar() {}\nbar();\n"),
                ("stuck/c.rs", "use bar;\n"),
                ("src/b.rs", "use bar;\n"),
            ],
        );
        let steps = apply(&world, &id, PermissionMode::Bypass, no_card);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();

        let text = the_error(&steps);
        let answer: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(answer["applied"], json!(["src/a.rs"]));
        assert_eq!(answer["failed"]["path"], "stuck/c.rs");
        assert!(!answer["failed"]["why"].as_str().unwrap().is_empty());
        assert_eq!(answer["not_tried"], json!(["src/b.rs"]));
        assert_eq!(fs::read(world.root.join("src/a.rs")).unwrap(), b"fn bar() {}\nbar();\n");
        assert_eq!(fs::read(world.root.join("src/b.rs")).unwrap(), B_RS);
        assert!(
            world.memory.proposal(1, &id).is_none(),
            "its before bytes no longer hold"
        );
    }

    #[test]
    fn a_first_file_that_fails_writes_nothing_and_keeps_the_proposal() {
        let world = World::new("first-fails");
        let stuck = world.root.join("stuck");
        fs::create_dir(&stuck).unwrap();
        fs::write(stuck.join("c.rs"), "use foo;\n").unwrap();
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(stuck.join("probe"), "").is_ok() {
            println!("this user writes through a read-only directory; skipped");
            fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let id = propose(&world, &[("stuck/c.rs", "use bar;\n"), ("src/b.rs", "use bar;\n")]);
        let steps = apply(&world, &id, PermissionMode::Bypass, no_card);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();

        let answer: serde_json::Value = serde_json::from_str(the_error(&steps)).unwrap();
        assert_eq!(answer["applied"], json!([]));
        assert_eq!(answer["not_tried"], json!(["src/b.rs"]));
        assert!(world.memory.proposal(1, &id).is_some());
    }

    #[test]
    fn a_second_apply_finds_nothing() {
        let world = World::new("second");
        let id = rename_both(&world);
        the_answer(&apply(&world, &id, PermissionMode::Bypass, no_card));
        let steps = apply(&world, &id, PermissionMode::Bypass, no_card);
        assert_eq!(
            the_error(&steps),
            format!("no proposal `{id}` in this tab; it may have been replaced by newer ones, or the tab moved")
        );
    }

    #[test]
    fn a_proposal_spent_under_an_open_card_is_not_written() {
        let world = World::new("spent-under-card");
        let id = rename_both(&world);
        let steps = apply_with(
            &world,
            &id,
            PermissionMode::Manual,
            |_| panic!("the editor was asked"),
            |request| {
                world.memory.spend_proposal(1, &id);
                allow(request)
            },
        );
        assert!(the_error(&steps).starts_with("no proposal"));
        assert_nothing_written(&world);
    }

    #[test]
    fn a_window_2_tab_is_refused_and_no_proposal_crosses() {
        let world = World::new("window-2");
        let id = rename_both(&world);
        for mode in MODES {
            let began = world.with_cx_as(mode, Trusted, EditorReach::Link, |cx| {
                ApplyProposal.begin(&json!({ "id": id }), cx)
            });
            assert_eq!(began.err().unwrap().0, words::MOVE_TO_EDITOR);
        }
        assert!(
            declared(&Switches::on_by_default(), Trusted)
                .iter()
                .any(|row| row.name == "apply_proposal"),
            "the list is fixed at the session's start and a tab may move back"
        );
        assert_nothing_written(&world);
    }

    #[test]
    fn an_n_session_is_refused() {
        let world = World::new("untrusted");
        let id = rename_both(&world);
        for mode in MODES {
            let began = world.with_cx_as(mode, Untrusted, EditorReach::Own, |cx| {
                ApplyProposal.begin(&json!({ "id": id }), cx)
            });
            assert!(began.err().unwrap().0.contains("without trusting this project"));
        }
        assert!(!declared(&Switches::on_by_default(), Untrusted)
            .iter()
            .any(|row| row.name == "apply_proposal"));
    }

    #[test]
    fn an_unknown_id_is_refused() {
        let world = World::new("unknown");
        let steps = apply_with(
            &world,
            "p-7",
            PermissionMode::Bypass,
            |_| panic!("the editor was asked"),
            no_card,
        );
        assert_eq!(
            the_error(&steps),
            "no proposal `p-7` in this tab; it may have been replaced by newer ones, or the tab moved"
        );
    }

    #[test]
    fn a_malformed_id_is_refused() {
        let world = World::new("malformed");
        for id in [
            json!(null),
            json!(1),
            json!(""),
            json!("p-"),
            json!("p1"),
            json!("P-1"),
            json!("p-1a"),
            json!("p- 1"),
            json!("p-1\n"),
            json!("p-١"),
            json!(format!("p-{}", "1".repeat(21))),
        ] {
            let began = world.with_cx(|cx| ApplyProposal.begin(&json!({ "id": id }), cx));
            assert!(began.is_err(), "{id}");
        }
        assert!(world.with_cx(|cx| ApplyProposal.begin(&json!({}), cx)).is_err());
        let most = format!("p-{}", "1".repeat(20));
        assert!(world
            .with_cx(|cx| ApplyProposal.begin(&json!({ "id": most }), cx))
            .is_ok());
    }

    #[test]
    fn a_card_never_answers_for_the_tool() {
        let world = World::new("card-answers");
        let id = rename_both(&world);
        let steps = apply(&world, &id, PermissionMode::Manual, allow);
        let on_card = steps.iter().position(|step| matches!(step, Step::Ask(_))).unwrap();
        assert!(
            matches!(
                steps[on_card + 1],
                Step::Call {
                    call: EditorCall::LspTargets { .. },
                    ..
                }
            ),
            "an allow goes on to the editor's check: {:?}",
            steps[on_card + 1]
        );

        let world = World::new("card-silence");
        let id = rename_both(&world);
        world.with_cx_as(PermissionMode::Manual, Trusted, EditorReach::Own, |cx| {
            for heard in [StepEvent::Poll, StepEvent::Answer(Ok(nothing_loaded(&[])))] {
                let mut run = ApplyProposal.begin(&json!({ "id": id }), cx).unwrap();
                assert!(matches!(run.step(StepEvent::Start, cx), Step::Ask(_)));
                match run.step(heard, cx) {
                    Step::Done(result) => assert_eq!(result.text, OUT_OF_TURN),
                    other => panic!("only a card's answer moves a run past its card: {other:?}"),
                }
            }
        });
        assert_nothing_written(&world);
        assert!(world.memory.proposal(1, &id).is_some());
    }

    /// A run left at its write, with `report` as the worker's end.
    fn writing(world: &World, report: Receiver<Result<ApplyReport, ProposalRefused>>, since: Instant) -> ApplyRun {
        let id = rename_both(world);
        ApplyRun {
            stage: Stage::Writing {
                landing: Landing::new(&world.memory, 1, &id),
                report,
                since,
            },
            id,
        }
    }

    #[test]
    fn a_write_past_its_limit_says_it_may_still_land() {
        let world = World::new("write-limit");
        let (sender, report) = std::sync::mpsc::channel();
        let past = Instant::now()
            .checked_sub(WRITE_LIMIT)
            .expect("the clock has run that long");
        let mut run = writing(&world, report, past);
        let step = world.with_cx(|cx| run.step(StepEvent::Poll, cx));
        drop(sender);
        let Step::Done(result) = step else {
            panic!("{step:?}");
        };
        assert!(result.is_error);
        assert!(result.text.contains("may still land"), "{}", result.text);
        assert_its_write_cannot_come_back(&world);
    }

    #[test]
    fn a_call_given_up_during_its_write_takes_the_proposal_with_it() {
        let world = World::new("write-given-up");
        let (sender, report) = std::sync::mpsc::channel();
        drop(writing(&world, report, Instant::now()));
        drop(sender);
        assert_its_write_cannot_come_back(&world);
    }

    /// The write of `p-1` was given up on while it may still land. The files hold their old bytes here, as they do
    /// once the person has restored them, so only the proposal's going stops a second apply undoing that.
    #[track_caller]
    fn assert_its_write_cannot_come_back(world: &World) {
        assert!(world.memory.proposal(1, "p-1").is_none());
        let steps = apply(world, "p-1", PermissionMode::Bypass, no_card);
        assert!(the_error(&steps).starts_with("no proposal"));
        assert_nothing_written(world);
    }

    #[test]
    fn a_write_that_is_still_going_is_polled_again() {
        let world = World::new("write-going");
        let (sender, report) = std::sync::mpsc::channel();
        let mut run = writing(&world, report, Instant::now());
        let step = world.with_cx(|cx| run.step(StepEvent::Poll, cx));
        assert!(matches!(step, Step::PollAfter(_)), "{step:?}");
        sender.send(Err(ProposalRefused::NoStore)).unwrap();
        let step = world.with_cx(|cx| run.step(StepEvent::Poll, cx));
        let Step::Done(result) = step else {
            panic!("{step:?}");
        };
        assert_eq!(result.text, ProposalRefused::NoStore.to_string());
        assert!(
            world.memory.proposal(1, "p-1").is_some(),
            "a refused apply wrote nothing"
        );
    }

    #[test]
    fn a_lost_worker_says_so() {
        let world = World::new("write-lost");
        let (_, report) = std::sync::mpsc::channel();
        let mut run = writing(&world, report, Instant::now());
        let step = world.with_cx(|cx| run.step(StepEvent::Poll, cx));
        let Step::Done(result) = step else {
            panic!("{step:?}");
        };
        assert_eq!(result.text, LOST);
        assert!(result.is_error);
        assert_its_write_cannot_come_back(&world);
    }

    #[test]
    fn a_write_is_given_no_hunks() {
        let world = World::new("no-hunks");
        rename_both(&world);
        let proposal = world.proposal("p-1");
        assert!(proposal.files.iter().all(|file| !file.hunks.is_empty()));
        let files = to_write(&proposal.files);
        assert!(files.iter().all(|file| file.hunks.is_empty()));
        assert_eq!(files[0].before, proposal.files[0].before);
        assert_eq!(files[1].after, proposal.files[1].after);
        assert_eq!(files[1].rel, proposal.files[1].rel);
    }

    #[test]
    fn the_tool_is_deferred_and_can_be_switched_off() {
        let row = crate::editor_tools::tools::tool_named("apply_proposal").unwrap().row();
        assert!(!row.always_load);
        assert!(row.default_on);
        assert!(!row.in_untrusted);
        let off = Switches::parse(
            None,
            &[("agent.editor.tool.apply_proposal".to_owned(), "off".to_owned())],
        )
        .unwrap();
        assert!(!declared(&off, Trusted).iter().any(|row| row.name == ROW.name));
    }
}
