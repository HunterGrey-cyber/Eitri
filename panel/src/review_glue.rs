//! Turn review's side of the panel: the review and proposal jobs, the review hints a page shows, the
//! review flow's answers routed back to the page that asked, and the editor's answers to a draft edit.

use super::*;
use eitri_core::agent_prefs::ReviewHintLevel;

/// The `review_settings` envelope for the state as it is: whether `c` can open a review, and
/// how its key and change counts are offered.
pub(super) fn review_settings_payload(state: &AgentPanelState) -> String {
    eitri_core::agent_bridge::serialize_review_settings_for_js(state.review_enabled, state.review_hint)
}

/// The three values a turn review is built from.
pub(super) struct ReviewRoots {
    pub(super) state_home: Option<std::ffi::OsString>,
    pub(super) home: Option<std::ffi::OsString>,
    pub(super) canonical_root: PathBuf,
}

/// `init.lua`'s review keys, applied once after it ran.
pub struct ReviewConfig {
    pub enabled: bool,
    pub hint: ReviewHintLevel,
    pub live: bool,
}

impl AgentPanelState {
    /// The state half of [`AgentPanelHandle::set_review`], which needs no page.
    pub(super) fn apply_review(&mut self, review: ReviewConfig) {
        self.review_hint = review.hint;
        self.review_enabled = false;
        if review.enabled {
            let roots = &self.review_roots;
            let installed = eitri_core::turn_review::TurnReview::new(
                roots.state_home.as_deref(),
                roots.home.as_deref(),
                &roots.canonical_root,
            );
            match self.tabs.install_turn_review(installed) {
                Ok(()) => self.review_enabled = true,
                Err(why) => eprintln!("[agent_panel] {why}"),
            }
        } else {
            self.tabs.turn_review_off();
            eprintln!("[review] turn review is off (init.lua's review.enabled = false)");
        }
        self.tabs.set_live_edits(self.review_enabled && review.live);
    }
}

/// What follows the window's answer to a request for nvim (`Effect::AskEditor`).
pub(super) enum EditorThen {
    /// The request of `view`'s page, known to it as `request_id`, is answered: taken, or refused with
    /// why the editor could not take it.
    Answer { view: ViewId, request_id: String },
    /// The window's own request, with no page to answer: the answer is left in the cell for the caller that asked.
    Report(std::rc::Rc<std::cell::RefCell<Option<Result<(), String>>>>),
    /// The window's own view, asked for without waiting: `failed` is told why, if the editor did not take it.
    Notify(std::rc::Rc<dyn Fn(&str)>),
    /// A draft edit of `tab`: on its way, the draft is `text` and the edit is polled until it comes
    /// back; refused, the edit is undone. Then the page's request is answered.
    Draft {
        view: ViewId,
        request_id: String,
        tab: TabId,
        text: String,
        edit: eitri_core::scratch::PendingEdit,
    },
}

/// The step after the window answered a request for nvim (`answer`: `Err` says why the editor could not
/// take it).
pub(super) fn editor_answered(state: &mut AgentPanelState, then: EditorThen, answer: Result<(), String>) -> Outbox {
    let mut out = Outbox::default();
    match then {
        EditorThen::Report(slot) => {
            *slot.borrow_mut() = Some(answer);
        }
        EditorThen::Notify(failed) => {
            if let Err(why) = answer {
                failed(&why);
            }
        }
        EditorThen::Answer { view, request_id } => {
            out.send(
                view,
                serialize_command_result_for_js(&request_id, answer.as_ref().map(|_| ()).map_err(String::as_str)),
            );
        }
        EditorThen::Draft {
            view,
            request_id,
            tab,
            text,
            edit,
        } => match answer {
            Err(why) => {
                state
                    .tabs
                    .finish_scratch_edit(edit.id, &eitri_core::scratch::EditDone::Discarded);
                edit.cleanup();
                out.send(view, serialize_command_result_for_js(&request_id, Err(&why)));
            }
            Ok(()) => {
                if state.tabs.question_edit(edit.id).is_none() {
                    state.tabs.set_draft(tab, &text);
                }
                state.pending_edits.push(edit);
                out.send(view, eitri_core::agent_bridge::serialize_scratch_for_js(tab, true));
                out.send(view, serialize_command_result_for_js(&request_id, Ok(())));
            }
        },
    }
    out
}

/// The most reviews worked out at once. Each one runs git over the shadow repository, and a panel
/// that asks again before an answer arrives is told to wait rather than queueing without bound.
pub(super) const MAX_REVIEW_JOBS: usize = 4;

/// One review being worked out on a worker thread.
pub(super) struct PendingReview {
    pub(super) request_id: String,
    /// The page that asked: the answer goes there.
    pub(super) view: ViewId,
    /// The finished envelope, or why there is none.
    pub(super) rx: mpsc::Receiver<Result<String, String>>,
    /// The tab and turn whose status-band hint the page drops when this review's envelope arrives:
    /// Rust forgets the remembered hint only then, since a refused or failed review leaves the hint
    /// on the page.
    pub(super) clears_hint: Option<(u64, eitri_core::agent_bridge::ReviewTurnWire)>,
}

/// Runs `work` on a worker thread; the tick sends what it returns (see [`poll_review_jobs`]).
/// Refused while [`MAX_REVIEW_JOBS`] are already out.
pub(super) fn start_review_job(
    state: &mut AgentPanelState,
    view: ViewId,
    request_id: &str,
    clears_hint: Option<(u64, eitri_core::agent_bridge::ReviewTurnWire)>,
    work: impl FnOnce() -> Result<String, String> + Send + 'static,
) -> Result<(), String> {
    if state.review_jobs.len() >= MAX_REVIEW_JOBS {
        return Err("reviews are still being worked out; try again in a moment".to_string());
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // A window closed in the meantime has dropped the receiver; the answer has nowhere to go.
        let _ = tx.send(work());
    });
    state.review_jobs.push(PendingReview {
        request_id: request_id.to_string(),
        view,
        rx,
        clears_hint,
    });
    Ok(())
}

/// What the finished jobs of `jobs` owe the panel, in order: an envelope for a review that was
/// worked out, a failing `command_result` for one that could not be (or whose worker died). The
/// finished ones leave `jobs`. The second list is the hints those envelopes clear on the page.
#[cfg(test)]
pub(super) fn finished_reviews(
    jobs: &mut Vec<PendingReview>,
) -> (Vec<String>, Vec<(u64, eitri_core::agent_bridge::ReviewTurnWire)>) {
    let (payloads, cleared) = finished_reviews_by_page(jobs);
    (payloads.into_iter().map(|(_, payload)| payload).collect(), cleared)
}

/// What [`finished_reviews_by_page`] found: each payload with the page it goes to, and the hints the
/// reviews among them clear.
pub(super) type ReviewsDone = (
    Vec<(ViewId, String)>,
    Vec<(u64, eitri_core::agent_bridge::ReviewTurnWire)>,
);

/// [`finished_reviews`], each payload with the view of the page that asked for it.
pub(super) fn finished_reviews_by_page(jobs: &mut Vec<PendingReview>) -> ReviewsDone {
    let mut payloads = Vec::new();
    let mut cleared = Vec::new();
    jobs.retain(|job| match job.rx.try_recv() {
        Ok(Ok(envelope)) => {
            payloads.push((job.view, envelope));
            cleared.extend(job.clears_hint);
            false
        }
        Ok(Err(why)) => {
            payloads.push((job.view, serialize_command_result_for_js(&job.request_id, Err(&why))));
            false
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            payloads.push((
                job.view,
                serialize_command_result_for_js(&job.request_id, Err("the review stopped before it finished")),
            ));
            false
        }
        Err(mpsc::TryRecvError::Empty) => true,
    });
    (payloads, cleared)
}

/// A finished turn's hints reach the page at the key and full levels; off queues and dispatches none.
pub(super) fn hints_to_keep(
    level: ReviewHintLevel,
    hints: Vec<eitri_core::turn_review::ReviewHint>,
) -> Vec<eitri_core::turn_review::ReviewHint> {
    if level != ReviewHintLevel::Off {
        hints
    } else {
        Vec::new()
    }
}

/// Adds `new` to the hints still waiting for the page, in order; a tab's older hint is dropped
/// because the newer one (even `files: 0`, which clears) supersedes it.
pub(super) fn keep_newest_hint_per_tab(
    waiting: &mut Vec<eitri_core::turn_review::ReviewHint>,
    new: Vec<eitri_core::turn_review::ReviewHint>,
) {
    for hint in new {
        waiting.retain(|old| old.tab != hint.tab);
        waiting.push(hint);
    }
}

/// Records what the page was just sent, so a later document can be told again: a hint with files
/// replaces its tab's, one with none clears it.
pub(super) fn keep_shown_hints(
    shown: &mut Vec<eitri_core::turn_review::ReviewHint>,
    sent: &[eitri_core::turn_review::ReviewHint],
) {
    for hint in sent {
        shown.retain(|old| old.tab != hint.tab);
        if hint.files > 0 {
            shown.push(hint.clone());
        }
    }
}

/// The page clears `tab`'s hint on receiving that tab's review of a turn at least as new as the
/// hint's, so a reload must not bring it back.
pub(super) fn forget_hint_for_review(
    shown: &mut Vec<eitri_core::turn_review::ReviewHint>,
    tab: u64,
    asked: eitri_core::agent_bridge::ReviewTurnWire,
) {
    use eitri_core::agent_bridge::ReviewTurnWire;
    shown.retain(|hint| {
        hint.tab != tab
            || match asked {
                ReviewTurnWire::Latest => false,
                ReviewTurnWire::N(turn) => turn < hint.turn,
            }
    });
}

/// Forgets the shown hints of a tab that closed or is running a turn: the page drops a tab's hint
/// when a turn starts, and a turn that ends without a hint of its own would otherwise leave this
/// record holding one the page no longer has.
pub(super) fn forget_hints_of_running_or_closed_tabs(state: &mut AgentPanelState) {
    let tabs = &state.tabs;
    state.shown_hints.retain(|hint| {
        tabs.get(eitri_core::tabs::TabId(hint.tab))
            .is_some_and(|tab| !tab.turn_running())
    });
}

/// The status-band hint `tab` showed in the page it left, to the ready page of `view` it moved to:
/// that page was never sent it, and the tick does not send a shown hint again. A page not ready yet is
/// sent it with the rest on its `ready` ([`hints_for_new_document`]).
pub(super) fn send_moved_hint(state: &mut AgentPanelState, tab: TabId, view: ViewId, out: &mut Outbox) {
    if !state.ready(view) {
        return;
    }
    forget_hints_of_running_or_closed_tabs(state);
    let hint = state
        .shown_hints
        .iter()
        .find(|hint| hint.tab == tab.0)
        .map(eitri_core::agent_bridge::serialize_review_hint_for_js);
    if let Some(hint) = hint {
        out.send(view, hint);
    }
}

/// What a new document in `view`'s page is owed of the hints the last one showed: only those of the
/// tabs in that view.
pub(super) fn hints_for_new_document(
    state: &mut AgentPanelState,
    view: ViewId,
) -> Vec<eitri_core::turn_review::ReviewHint> {
    forget_hints_of_running_or_closed_tabs(state);
    state
        .shown_hints
        .iter()
        .filter(|hint| state.tabs.view_of(TabId(hint.tab)) == view)
        .cloned()
        .collect()
}

/// The waiting hints whose tab's view has a ready page, each with that view; the others keep waiting.
pub(super) fn take_hints_for_ready_pages(
    state: &mut AgentPanelState,
) -> Vec<(ViewId, eitri_core::turn_review::ReviewHint)> {
    let waiting = std::mem::take(&mut state.review_hints);
    let mut ready = Vec::new();
    for hint in waiting {
        let view = state.tabs.view_of(TabId(hint.tab));
        if state.ready(view) {
            ready.push((view, hint));
        } else {
            state.review_hints.push(hint);
        }
    }
    ready
}

/// The tick's half of a review: whatever a worker finished goes to the page that asked.
pub(super) fn poll_review_jobs(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.review_jobs.is_empty() {
        return;
    }
    let (payloads, cleared) = finished_reviews_by_page(&mut state.review_jobs);
    for (tab, turn) in cleared {
        forget_hint_for_review(&mut state.shown_hints, tab, turn);
    }
    for (view, payload) in payloads {
        out.send(view, payload);
    }
}

/// The most views being written at once. A press that finds this many out is told to try again, rather than
/// queueing without bound behind a disk that has stopped answering.
pub(super) const MAX_VIEW_JOBS: usize = MAX_REVIEW_JOBS;

/// One view's scratch file being written on a worker thread.
pub(super) struct PendingView {
    pub(super) asker: ViewAsker,
    pub(super) rx: mpsc::Receiver<std::io::Result<eitri_core::scratch::ScratchRequest>>,
}

/// Who asked for a view, and so who hears how it went.
pub(super) enum ViewAsker {
    /// `view`'s page, as `request_id`: the answer goes there, whether the editor took the view or the write failed.
    Page { view: ViewId, request_id: String },
    /// The window itself, with no page to answer: `failed` is told why the view did not open.
    Window { failed: std::rc::Rc<dyn Fn(&str)> },
}

impl ViewAsker {
    /// The page this view answers, if a page asked.
    pub(super) fn page(&self) -> Option<ViewId> {
        match self {
            Self::Page { view, .. } => Some(*view),
            Self::Window { .. } => None,
        }
    }
}

/// Writes `text` as a scratch file on a worker thread and has the tick hand nvim the view (see [`poll_view_jobs`]):
/// the text may be a megabyte, and nothing waits for a disk on the GTK thread. Only the file's name is chosen
/// here. Refused while [`MAX_VIEW_JOBS`] are already out; a press made while another view is being written is
/// a view of its own, shown after the earlier one, so the last press is the one on screen.
pub(super) fn start_view_job(
    state: &mut AgentPanelState,
    asker: ViewAsker,
    title: &str,
    text: String,
) -> Result<(), String> {
    start_view_job_with_tab(state, asker, title, text, false)
}

pub(super) fn start_view_job_with_tab(
    state: &mut AgentPanelState,
    asker: ViewAsker,
    title: &str,
    text: String,
    tab: bool,
) -> Result<(), String> {
    if state.view_jobs.len() >= MAX_VIEW_JOBS {
        return Err("views are still being written; try again in a moment".to_string());
    }
    let file = state
        .scratch
        .as_mut()
        .ok_or_else(|| "the scratch directory could not be created at startup".to_string())?
        .reserve_view(title)
        .with_tab(tab);
    let rx = spawn_view_write(file, text, |job| {
        std::thread::Builder::new()
            .name("view write".into())
            .spawn(job)
            .map(|_| ())
    })?;
    state.view_jobs.push(PendingView { asker, rx });
    Ok(())
}

/// Has `spawn` run the write of `text` to `file` on a thread, and returns where its result will arrive. A thread
/// the OS refuses is the page's to hear about, as a failed write is: this runs inside the WebView's message
/// callback, where a panic is no answer. Nothing is written when the thread does not start.
fn spawn_view_write(
    file: eitri_core::scratch::ViewFile,
    text: String,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
) -> Result<mpsc::Receiver<std::io::Result<eitri_core::scratch::ScratchRequest>>, String> {
    let (tx, rx) = mpsc::channel();
    spawn(Box::new(move || {
        // A window closed in the meantime has dropped the receiver; the answer has nowhere to go.
        let _ = tx.send(file.write(&text));
    }))
    .map_err(|e| format!("could not write the scratch file: {e}"))?;
    Ok(rx)
}

/// The tick's half of a view: a finished write is handed to the editor, whose answer is the page's; a failed
/// write, or a worker that died, answers the page with why. Views are taken in the order they were asked for,
/// so one that finished early waits for the one before it.
pub(super) fn poll_view_jobs(state: &mut AgentPanelState, out: &mut Outbox) {
    while let Some(job) = state.view_jobs.first() {
        let written = match job.rx.try_recv() {
            Ok(written) => written.map_err(|e| format!("could not write the scratch file: {e}")),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("could not write the scratch file: its worker stopped".to_string())
            }
        };
        let job = state.view_jobs.remove(0);
        match (written, job.asker) {
            (Ok(request), ViewAsker::Page { view, request_id }) => out.effect(Effect::AskEditor {
                request,
                then: EditorThen::Answer { view, request_id },
            }),
            (Ok(request), ViewAsker::Window { failed }) => out.effect(Effect::AskEditor {
                request,
                then: EditorThen::Notify(failed),
            }),
            (Err(why), ViewAsker::Page { view, request_id }) => {
                out.send(view, serialize_command_result_for_js(&request_id, Err(&why)))
            }
            (Err(why), ViewAsker::Window { failed }) => failed(&why),
        }
    }
}

/// One pending edit's proposal being read on a worker thread.
pub(super) struct PendingProposal {
    pub(super) request_id: String,
    /// The page that asked: the answer goes there.
    pub(super) view: ViewId,
    /// The file's name as resolved; nvim takes the filetype from it.
    pub(super) name: String,
    pub(super) rx: mpsc::Receiver<Result<(String, String), eitri_core::scratch::ProposalRefused>>,
}

/// Runs `read` on a worker thread, which gives nvim's diff tab its two sides; the tick hands nvim the result (see
/// [`poll_proposal_jobs`]). `name` is the file's name, from which nvim takes the filetype. Refused while
/// [`MAX_REVIEW_JOBS`] are already out.
pub(super) fn start_proposal_job(
    state: &mut AgentPanelState,
    view: ViewId,
    request_id: &str,
    name: String,
    read: impl FnOnce() -> Result<(String, String), eitri_core::scratch::ProposalRefused> + Send + 'static,
) -> Result<(), String> {
    if state.proposal_jobs.len() >= MAX_REVIEW_JOBS {
        return Err("proposals are still being read; try again in a moment".to_string());
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // A window closed in the meantime has dropped the receiver; the answer has nowhere to go.
        let _ = tx.send(read());
    });
    state.proposal_jobs.push(PendingProposal {
        request_id: request_id.to_string(),
        view,
        name,
        rx,
    });
    Ok(())
}

/// The tick's half of a proposal: a finished read is written to the scratch directory and handed to the
/// editor, whose answer is the page's; a refusal, or a worker that died, answers the page with why.
pub(super) fn poll_proposal_jobs(state: &mut AgentPanelState, out: &mut Outbox) {
    let mut i = 0;
    while i < state.proposal_jobs.len() {
        let read = match state.proposal_jobs[i].rx.try_recv() {
            Ok(read) => read.map_err(|why| why.to_string()),
            Err(mpsc::TryRecvError::Empty) => {
                i += 1;
                continue;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("the proposal could not be read: its worker stopped".to_string())
            }
        };
        let job = state.proposal_jobs.remove(i);
        let prepared = read.and_then(|(base, proposed)| match state.scratch.as_mut() {
            Some(dir) => dir
                .prepare_proposal(&job.name, &base, &proposed)
                .map_err(|e| format!("could not write the scratch file: {e}")),
            None => Err("the scratch directory could not be created at startup".to_string()),
        });
        match prepared {
            Ok(request) => out.effect(Effect::AskEditor {
                request,
                then: EditorThen::Answer {
                    view: job.view,
                    request_id: job.request_id,
                },
            }),
            Err(why) => out.send(job.view, serialize_command_result_for_js(&job.request_id, Err(&why))),
        }
    }
}

/// What the review flow owes the panel: an answer goes to the page that sent its request, under the id
/// that page knows it by, a tab's draft to the page of the tab's view, the window's interrupted reverts
/// to the base page, and a confirmed review that was sent is applied as any flush of a tab's queue is.
/// The requests the flow no longer owes an answer are forgotten afterwards.
pub(super) fn dispatch_review_outs(
    state: &mut AgentPanelState,
    screen: &Screen,
    outs: Vec<eitri_core::turn_review::Out>,
    out: &mut Outbox,
) {
    use eitri_core::turn_review::Out;
    for owed in outs {
        match owed {
            Out::Envelope(payload) => out.send(ViewId::BASE, payload),
            // The flow knows the request by the ledger's key.
            Out::Answer {
                request_id: key,
                payload,
            } => {
                let asked = state
                    .owed
                    .get(&key)
                    .filter(|answer| answer.what == Owed::ReviewRequest)
                    .map(|answer| (answer.page, answer.request_id.clone()));
                match asked {
                    Some((Some(view), request_id)) => out.send(view, answer_under(&payload, &key, &request_id)),
                    // The page that asked has closed.
                    Some((None, _)) => {}
                    None => eprintln!("[agent_panel] review answer for {key}: no page asked for it"),
                }
            }
            Out::Draft(tab, payload) => send_to_tabs_page(state, tab, payload, out),
            Out::Flushed(tab, flush) => apply_flush(state, screen, tab, flush, out),
            Out::QueueChanged(tab) => send_queue(state, tab, out),
            Out::EditorOpened => out.effect(Effect::EditorOpened),
        }
    }
    let flow = &state.review_flow;
    state
        .owed
        .retain(|key, answer| answer.what != Owed::ReviewRequest || flow.owes_answer(key));
}

/// `payload`, an answer the review flow made under the ledger's `key`, under `request_id`, the id the
/// page that asked knows it by. Every answer the flow makes names its request at the top level.
pub(super) fn answer_under(payload: &str, key: &str, request_id: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(serde_json::Value::Object(mut fields))
            if fields.get("requestId").and_then(|id| id.as_str()) == Some(key) =>
        {
            fields.insert("requestId".to_string(), serde_json::Value::from(request_id));
            serde_json::Value::Object(fields).to_string()
        }
        _ => {
            eprintln!("[agent_panel] review answer for {key} does not name it; sent as it is");
            payload.to_string()
        }
    }
}

/// `message`, a review message, under `key` in place of the id its page gave it.
pub(super) fn review_message_under(mut message: InboundMessage, key: &str) -> InboundMessage {
    match &mut message {
        InboundMessage::ReviewRevert { request_id, .. }
        | InboundMessage::ReviewUndo { request_id, .. }
        | InboundMessage::ReviewCommentAdd { request_id, .. }
        | InboundMessage::ReviewCommentRemove { request_id, .. }
        | InboundMessage::ReviewSend { request_id, .. }
        | InboundMessage::ReviewRecover { request_id, .. }
        | InboundMessage::OpenInEditor { request_id, .. } => *request_id = key.to_string(),
        _ => debug_assert!(false, "a message that is not the review flow's was given a ledger key"),
    }
    message
}

/// One review message for the flow from `view`'s page, with the window's editor handle if it has one.
/// What the flow answers, now or on a later tick, goes to that page. The flow is given the ledger's
/// key for the request, so a request id another page also has out is never mixed up with it; the
/// same page's id still out is the same request.
pub(super) fn run_review_flow(
    state: &mut AgentPanelState,
    screen: &Screen,
    view: ViewId,
    message: InboundMessage,
    out: &mut Outbox,
) {
    let request_id = message.request_id().to_string();
    let key = state
        .owed
        .key_where(|answer| {
            answer.what == Owed::ReviewRequest && answer.page == Some(view) && answer.request_id == request_id
        })
        .unwrap_or_else(|| state.owed.owe(view, &request_id, Owed::ReviewRequest));
    let message = review_message_under(message, &key);
    state.review_editor.sync(&mut state.review_flow);
    let outs = state.review_flow.handle(
        message,
        &mut state.tabs,
        state.editor_rpc.as_deref(),
        std::time::Instant::now(),
    );
    dispatch_review_outs(state, screen, outs, out);
}

/// The tick's half of the review flow: editor answers, workers that finished and tickets whose tab
/// went away, each answer for the page that sent its request.
pub(super) fn poll_review_flow(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    if state.review_flow.is_idle() {
        return;
    }
    state.review_editor.sync(&mut state.review_flow);
    let outs = state
        .review_flow
        .tick(&mut state.tabs, state.editor_rpc.as_deref(), std::time::Instant::now());
    dispatch_review_outs(state, screen, outs, out);
}

/// The tick's half of the live drawings: the edits the pump listed since the last tick, and the editor's and the
/// workers' answers for them. Nothing here answers a page; what the editor reports back comes through
/// [`poll_review_flow`], which runs after this.
pub(super) fn poll_live_review(state: &mut AgentPanelState) {
    state.review_editor.sync(&mut state.review_flow);
    state
        .review_flow
        .live_tick(&mut state.tabs, state.editor_rpc.as_deref(), std::time::Instant::now());
}

/// The return half of a scratch edit: every edit whose marker appeared goes back to the tab it came from,
/// told to the page that shows it, and the keys return to the chat view of the last such tab.
pub(super) fn poll_scratch_edits(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.pending_edits.is_empty() {
        return;
    }
    let mut finished: Vec<(eitri_core::scratch::PendingEdit, eitri_core::scratch::EditDone)> = Vec::new();
    state.pending_edits.retain(|edit| match edit.poll() {
        Some(result) => {
            finished.push((edit.clone(), result));
            false
        }
        None => true,
    });
    if finished.is_empty() {
        return;
    }
    let mut keys_to = ViewId::BASE;
    for (edit, result) in finished {
        let returned = finish_scratch_edit(state, edit.id, &result, out);
        edit.cleanup();
        if let Some(view) = returned {
            keys_to = view;
        }
    }
    out.effect(Effect::EditorDone(keys_to));
}

/// Ends the draft edit `id` with `result`: the tab's draft as it came back, told to the page that shows it, with a
/// failure's reason. The view the tab is in, or `None` when no tab has that edit out.
fn finish_scratch_edit(
    state: &mut AgentPanelState,
    id: u64,
    result: &eitri_core::scratch::EditDone,
    out: &mut Outbox,
) -> Option<ViewId> {
    let question = state.tabs.question_edit(id);
    let (tab, draft) = state.tabs.finish_scratch_edit(id, result)?;
    if let Some((_, interaction, index)) = question {
        let view = state.tabs.view_of(tab);
        if state
            .tabs
            .check_question_edit(tab, view, &interaction, Some(index))
            .is_ok()
        {
            if let eitri_core::scratch::EditDone::Written(text) = result {
                if text.len() <= 64 * 1024 {
                    // Stored as well as sent: the page may be showing another tab now, and drops an envelope
                    // for a tab it does not show; the dock restores the stored draft when the tab is shown.
                    if let Err(why) = state.tabs.store_question_other(tab, &interaction, index, text) {
                        eprintln!("[agent_panel] tab {}: edited answer not stored: {why}", tab.0);
                    }
                    out.send(
                        view,
                        serde_json::json!({
                            "kind": "question_other_edited", "tab": tab.0,
                            "interaction_id": interaction, "question_index": index, "text": text,
                        })
                        .to_string(),
                    );
                } else {
                    out.send(
                        view,
                        eitri_core::agent_bridge::serialize_notice_for_js("the question text exceeds 64 KiB"),
                    );
                }
            }
        }
    }
    let (shown, view) = (state.shown_in(tab), state.tabs.view_of(tab));
    if let Some(shown) = shown {
        if let Some(text) = &draft {
            out.send(shown, eitri_core::agent_bridge::serialize_draft_for_js(tab, text));
        }
        out.send(shown, eitri_core::agent_bridge::serialize_scratch_for_js(tab, false));
    }
    if let eitri_core::scratch::EditDone::Failed(why) = result {
        out.send(
            view,
            eitri_core::agent_bridge::serialize_notice_for_js(&format!("the nvim scratch buffer failed: {why}")),
        );
    }
    Some(view)
}

/// A draft edit that a chat brought with it from the other window of a pair ended in this window's editor: the
/// tab holding `session` takes the result as it takes one of its own, and the keys return to its chat view.
/// `false` when no tab here holds that session with an edit out.
pub(super) fn finish_moved_scratch_edit(
    state: &mut AgentPanelState,
    session: &str,
    result: &eitri_core::scratch::EditDone,
    out: &mut Outbox,
) -> bool {
    let edit = state
        .tabs
        .tabs()
        .iter()
        .find(|tab| tab.editing_draft.is_some() && tab.provider_session_id().as_deref() == Some(session))
        .and_then(|tab| tab.editing_draft);
    let Some(id) = edit else { return false };
    match finish_scratch_edit(state, id, result, out) {
        Some(view) => {
            out.effect(Effect::EditorDone(view));
            true
        }
        None => false,
    }
}

/// Ends every draft edit still out in nvim, for an editor that went away: the body file's text
/// becomes the draft when it differs from what was handed out, else the draft keeps its own text.
/// A marker nvim wrote wins; without one the body is read as it stands. Each edit's files go after
/// it is read. Returns the tabs that had an edit out, oldest edit first, with the new draft where
/// there is one.
pub(super) fn end_pending_edits(state: &mut AgentPanelState) -> Vec<(TabId, Option<String>)> {
    use eitri_core::scratch::EditDone;
    let edits = std::mem::take(&mut state.pending_edits);
    let results: Vec<(u64, EditDone)> = edits
        .iter()
        .map(|edit| {
            let done = edit
                .poll()
                .unwrap_or_else(|| match std::fs::read_to_string(&edit.body) {
                    Ok(raw) => EditDone::Written(raw.strip_suffix('\n').unwrap_or(&raw).to_string()),
                    Err(_) => EditDone::Discarded,
                });
            (edit.id, done)
        })
        .collect();
    let ended = state.tabs.cancel_scratch_edits(&results);
    for edit in &edits {
        edit.cleanup();
    }
    ended
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    /// The review switches reach the page: off before `init.lua`'s values are applied, at once to a
    /// page already listening when they are, before the tabs on every `ready`, and again to a document
    /// that replaces the first.
    #[test]
    fn the_review_settings_are_told_at_once_and_again_on_every_ready() {
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let settings = |page: &std::cell::RefCell<Vec<String>>| -> Vec<(bool, String)> {
            page.borrow()
                .iter()
                .filter(|e| kind_of(e) == "review_settings")
                .map(|e| {
                    let v = serde_json::from_str::<serde_json::Value>(e).unwrap();
                    (v["enabled"].as_bool().unwrap(), v["hint"].as_str().unwrap().to_owned())
                })
                .collect()
        };
        let kinds: Vec<String> = page.inner.sent.borrow().iter().map(|e| kind_of(e)).collect();
        let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
        assert!(at("review_settings") < at("tabs"), "{kinds:?}");
        assert_eq!(settings(&page.inner.sent), vec![(false, "off".into())]);
        handle.set_review(ReviewConfig {
            enabled: true,
            hint: ReviewHintLevel::Full,
            live: false,
        });
        assert_eq!(
            settings(&page.inner.sent),
            vec![(false, "off".into()), (true, "full".into())]
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        assert_eq!(
            settings(&page.inner.sent),
            vec![(false, "off".into()), (true, "full".into()), (true, "full".into())]
        );
        handle.set_review(ReviewConfig {
            enabled: false,
            hint: ReviewHintLevel::Full,
            live: false,
        });
        assert_eq!(settings(&page.inner.sent).last(), Some(&(false, "full".into())));
    }

    /// `Ctrl+g` on a pending `Write` of a file that is not there yet: the page's message goes through the
    /// arm and the worker, and the tick hands the editor one `diff` request against an empty file, then
    /// answers the page. The file itself is never made.
    #[test]
    fn a_view_proposal_for_a_new_file_reaches_the_editor() {
        let (handle, page) = settled_panel();
        let workspace = agent::state_dirs::test_workspace_dir("review-proposal");
        let scratch = eitri_core::scratch::ScratchDir::in_dir(&workspace).expect("a scratch directory");
        let project = scratch.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let scratch_root = scratch.path().to_path_buf();
        // The panel's own scratch directory, made beside the project so both go with the test's cleanup.
        let panel_scratch = eitri_core::scratch::ScratchDir::in_dir(&scratch_root).expect("a panel scratch directory");
        {
            let mut state = handle.state.borrow_mut();
            state.project_dir = project.clone();
            state.scratch = Some(panel_scratch);
        }
        let asked: Rc<RefCell<Vec<eitri_core::scratch::ScratchRequest>>> = Rc::default();
        let recorder = asked.clone();
        handle.on_editor_request(move |request| {
            recorder.borrow_mut().push(request.clone());
            Ok(())
        });
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"view_proposal","request_id":"vp1","path":"src/new.rs","change":{"kind":"content","content":"fn main() {}\n"}}"#,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while asked.borrow().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the proposal never reached the editor"
            );
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(asked.borrow().len(), 1, "one editor request");
        let hex = asked.borrow()[0].hex().to_string();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["op"], "diff");
        assert_eq!(value["name"], project.join("src/new.rs").to_string_lossy().as_ref());
        assert_eq!(std::fs::read_to_string(value["base"].as_str().unwrap()).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(value["proposed"].as_str().unwrap()).unwrap(),
            "fn main() {}\n"
        );
        assert!(
            !project.join("src/new.rs").exists(),
            "the proposal never writes the file"
        );
        let answered = page
            .sent
            .borrow()
            .iter()
            .any(|p| p.contains(r#""requestId":"vp1""#) && p.contains(r#""ok":true"#));
        assert!(answered, "the page's request was answered");
        assert!(handle.state.borrow().proposal_jobs.is_empty());
        let panel_scratch = handle.state.borrow_mut().scratch.take();
        if let Some(dir) = panel_scratch {
            dir.cleanup();
        }
        scratch.cleanup();
    }

    /// A panel whose scratch directory is made beside a throwaway one, and whose editor requests are recorded.
    fn panel_with_recording_editor() -> (
        AgentPanelHandle,
        Rc<RecordingPage>,
        eitri_core::scratch::ScratchDir,
        Rc<RefCell<Vec<serde_json::Value>>>,
    ) {
        let (handle, page) = settled_panel();
        let workspace = agent::state_dirs::test_workspace_dir("review-editor");
        let root = eitri_core::scratch::ScratchDir::in_dir(&workspace).expect("a scratch directory");
        handle.state.borrow_mut().scratch =
            Some(eitri_core::scratch::ScratchDir::in_dir(root.path()).expect("a panel scratch directory"));
        let asked: Rc<RefCell<Vec<serde_json::Value>>> = Rc::default();
        let recorder = asked.clone();
        handle.on_editor_request(move |request| {
            let hex = request.hex();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            recorder.borrow_mut().push(serde_json::from_slice(&bytes).unwrap());
            Ok(())
        });
        (handle, page, root, asked)
    }

    fn view_message(request_id: &str, title: &str, text: &str) -> String {
        serde_json::json!({ "type": "view_in_editor", "request_id": request_id, "title": title, "text": text })
            .to_string()
    }

    /// Ticks until `done`, for at most five seconds.
    fn tick_until(handle: &AgentPanelHandle, what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// `Ctrl+g` on a long row sends up to a megabyte: the message returns with the write still to be done, the
    /// editor is asked on a tick once it has been, and the file holds all of the text.
    #[test]
    fn a_view_is_written_on_a_worker_and_the_editor_is_asked_after() {
        let (handle, page, root, asked) = panel_with_recording_editor();
        let text = "输出的一行\n".repeat(100_000);
        assert!(text.len() > 1 << 20);
        handle.page_message(ViewId::BASE, &view_message("v1", "Bash: ls", &text));
        assert!(
            asked.borrow().is_empty(),
            "the editor is asked from the tick, after the write, not from the message"
        );
        assert_eq!(
            result_for(&page.sent.borrow(), "v1"),
            None,
            "not answered before the editor is"
        );
        tick_until(&handle, "the view never reached the editor", || {
            !asked.borrow().is_empty()
        });
        assert_eq!(asked.borrow().len(), 1, "one editor request");
        let request = asked.borrow()[0].clone();
        assert_eq!(request["op"], "view");
        assert_eq!(
            std::fs::read_to_string(request["path"].as_str().unwrap()).unwrap(),
            text,
            "the whole text"
        );
        assert_eq!(result_for(&page.sent.borrow(), "v1"), Some(Ok(())));
        assert!(handle.state.borrow().view_jobs.is_empty());
        let panel_scratch = handle.state.borrow_mut().scratch.take();
        if let Some(dir) = panel_scratch {
            dir.cleanup();
        }
        root.cleanup();
    }

    /// The window's own view (the init.lua line of an offered page): the call returns with the write still to do, the
    /// editor is asked from a tick once the file holds the text, and only a failure is reported.
    #[test]
    fn a_window_view_is_written_on_a_worker_and_reports_only_a_failure() {
        let (handle, page, root, asked) = panel_with_recording_editor();
        let failures: Rc<RefCell<Vec<String>>> = Rc::default();
        let seen = failures.clone();
        let text = "line\n".repeat(200_000);
        let results = || {
            page.sent
                .borrow()
                .iter()
                .filter(|p| p.contains("command_result"))
                .count()
        };
        let answered_before = results();
        handle
            .view_in_editor_later("init.lua line", text.clone(), move |why| {
                seen.borrow_mut().push(why.to_string())
            })
            .unwrap();
        assert!(
            asked.borrow().is_empty(),
            "the editor is asked from the tick, after the write"
        );
        assert_eq!(handle.state.borrow().view_jobs.len(), 1);
        tick_until(&handle, "the view never reached the editor", || {
            !asked.borrow().is_empty()
        });
        let request = asked.borrow()[0].clone();
        assert_eq!(request["op"], "view");
        assert_eq!(
            std::fs::read_to_string(request["path"].as_str().unwrap()).unwrap(),
            text
        );
        assert!(failures.borrow().is_empty(), "{:?}", failures.borrow());
        assert!(handle.state.borrow().view_jobs.is_empty());
        assert_eq!(results(), answered_before, "no page asked, so none is answered");

        // A write that failed is the window's to hear about, and the editor is not asked.
        let (tx, rx) = mpsc::channel();
        let seen = failures.clone();
        handle.state.borrow_mut().view_jobs.push(PendingView {
            asker: ViewAsker::Window {
                failed: Rc::new(move |why: &str| seen.borrow_mut().push(why.to_string())),
            },
            rx,
        });
        tx.send(Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "no space left on device",
        )))
        .unwrap();
        tick_until(&handle, "the failure never arrived", || !failures.borrow().is_empty());
        assert_eq!(asked.borrow().len(), 1, "the editor was not asked again");
        assert!(
            failures.borrow()[0].contains("no space left on device"),
            "{:?}",
            failures.borrow()
        );
        let panel_scratch = handle.state.borrow_mut().scratch.take();
        if let Some(dir) = panel_scratch {
            dir.cleanup();
        }
        root.cleanup();
    }

    /// A second press while the first is still being written is a view of its own, shown after the first, so the
    /// last press is what ends up on screen -- however the two writes finish.
    #[test]
    fn views_asked_in_turn_are_shown_in_turn_even_when_a_later_write_finishes_first() {
        let (handle, page, root, asked) = panel_with_recording_editor();
        let mut senders = Vec::new();
        for id in ["first", "second"] {
            let (tx, rx) = mpsc::channel();
            senders.push(tx);
            handle.state.borrow_mut().view_jobs.push(PendingView {
                asker: ViewAsker::Page {
                    view: ViewId::BASE,
                    request_id: id.into(),
                },
                rx,
            });
        }
        let request = |name: &str| {
            let mut dir = handle.state.borrow_mut().scratch.take().unwrap();
            let made = dir.prepare_view(name, name).unwrap();
            handle.state.borrow_mut().scratch = Some(dir);
            made
        };
        let (one, two) = (request("one"), request("two"));
        senders[1].send(Ok(two)).unwrap();
        handle.tick();
        assert!(asked.borrow().is_empty(), "the second waits for the first");
        assert_eq!(handle.state.borrow().view_jobs.len(), 2);
        senders[0].send(Ok(one)).unwrap();
        tick_until(&handle, "both views never reached the editor", || {
            asked.borrow().len() == 2
        });
        let names: Vec<String> = asked
            .borrow()
            .iter()
            .map(|r| r["path"].as_str().unwrap().rsplit('/').next().unwrap().to_string())
            .collect();
        assert!(
            names[0].ends_with("one.md") && names[1].ends_with("two.md"),
            "{names:?}"
        );
        assert_eq!(result_for(&page.sent.borrow(), "first"), Some(Ok(())));
        assert_eq!(result_for(&page.sent.borrow(), "second"), Some(Ok(())));
        let panel_scratch = handle.state.borrow_mut().scratch.take();
        if let Some(dir) = panel_scratch {
            dir.cleanup();
        }
        root.cleanup();
    }

    /// Two real presses made back to back give two views, the last press shown last.
    #[test]
    fn two_presses_in_a_row_give_two_views_in_order() {
        let (handle, page, root, asked) = panel_with_recording_editor();
        handle.page_message(ViewId::BASE, &view_message("a", "Bash: first", "first text"));
        handle.page_message(ViewId::BASE, &view_message("b", "Bash: second", "second text"));
        tick_until(&handle, "both views never reached the editor", || {
            asked.borrow().len() == 2
        });
        let texts: Vec<String> = asked
            .borrow()
            .iter()
            .map(|r| std::fs::read_to_string(r["path"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(texts, ["first text", "second text"]);
        assert_eq!(result_for(&page.sent.borrow(), "a"), Some(Ok(())));
        assert_eq!(result_for(&page.sent.borrow(), "b"), Some(Ok(())));
        let panel_scratch = handle.state.borrow_mut().scratch.take();
        if let Some(dir) = panel_scratch {
            dir.cleanup();
        }
        root.cleanup();
    }

    /// A write that fails is said where the press was made: the page's request is refused with the reason, and
    /// the editor is never asked. A worker that died says so the same way.
    #[test]
    fn a_view_that_could_not_be_written_answers_the_page_with_why() {
        let (handle, page, root, asked) = panel_with_recording_editor();
        let (failed_tx, failed_rx) = mpsc::channel();
        let (dead_tx, dead_rx) = mpsc::channel::<std::io::Result<eitri_core::scratch::ScratchRequest>>();
        for (id, rx) in [("failed", failed_rx), ("dead", dead_rx)] {
            handle.state.borrow_mut().view_jobs.push(PendingView {
                asker: ViewAsker::Page {
                    view: ViewId::BASE,
                    request_id: id.into(),
                },
                rx,
            });
        }
        failed_tx
            .send(Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "no space left on device",
            )))
            .unwrap();
        drop(dead_tx);
        tick_until(&handle, "the failures never reached the page", || {
            result_for(&page.sent.borrow(), "failed").is_some() && result_for(&page.sent.borrow(), "dead").is_some()
        });
        let failed = result_for(&page.sent.borrow(), "failed").unwrap().unwrap_err();
        assert!(
            failed.starts_with("could not write the scratch file:") && failed.contains("no space left"),
            "{failed}"
        );
        let dead = result_for(&page.sent.borrow(), "dead").unwrap().unwrap_err();
        assert!(dead.contains("its worker stopped"), "{dead}");
        assert!(
            asked.borrow().is_empty(),
            "no editor request for a view that is not there"
        );
        assert!(handle.state.borrow().view_jobs.is_empty());
        let panel_scratch = handle.state.borrow_mut().scratch.take();
        if let Some(dir) = panel_scratch {
            dir.cleanup();
        }
        root.cleanup();
    }

    /// A thread the OS will not start is a refusal the page hears, with nothing queued and nothing written.
    #[test]
    fn a_view_whose_thread_cannot_start_answers_the_page_with_why() {
        let workspace = agent::state_dirs::test_workspace_dir("review-write-refused");
        let mut dir = eitri_core::scratch::ScratchDir::in_dir(&workspace).expect("a scratch directory");
        let file = dir.reserve_view("Bash: ls");
        let refused =
            spawn_view_write(file, "text".into(), |_job| Err(std::io::Error::from_raw_os_error(11))).unwrap_err();
        assert!(refused.starts_with("could not write the scratch file:"), "{refused}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "only the snippet is there"
        );
        let file = dir.reserve_view("Bash: ls");
        let rx = spawn_view_write(file, "text".into(), |job| {
            job();
            Ok(())
        })
        .unwrap();
        assert!(rx.recv().unwrap().is_ok());
        dir.cleanup();
    }

    /// A press made while as many views as the panel allows are still being written is refused with a line
    /// saying to try again, and queues nothing.
    #[test]
    fn a_view_pressed_with_the_writes_all_busy_is_refused() {
        let (handle, page, root, asked) = panel_with_recording_editor();
        let mut senders = Vec::new();
        for i in 0..MAX_VIEW_JOBS {
            let (tx, rx) = mpsc::channel();
            senders.push(tx);
            handle.state.borrow_mut().view_jobs.push(PendingView {
                asker: ViewAsker::Page {
                    view: ViewId::BASE,
                    request_id: format!("busy{i}"),
                },
                rx,
            });
        }
        handle.page_message(ViewId::BASE, &view_message("one-too-many", "Bash: ls", "text"));
        let refused = result_for(&page.sent.borrow(), "one-too-many").expect("answered at once");
        assert_eq!(
            refused,
            Err("views are still being written; try again in a moment".to_string())
        );
        assert_eq!(handle.state.borrow().view_jobs.len(), MAX_VIEW_JOBS);
        handle.state.borrow_mut().view_jobs.clear();
        assert!(asked.borrow().is_empty());
        let panel_scratch = handle.state.borrow_mut().scratch.take();
        if let Some(dir) = panel_scratch {
            dir.cleanup();
        }
        root.cleanup();
    }

    /// A view still being written for a page that went idle is dropped with it: the editor is not raised for
    /// a page nobody sees.
    #[test]
    fn a_view_still_being_written_is_dropped_when_its_page_goes_idle() {
        let (handle, base, second) = handle_with_two_pages();
        let asked = Rc::new(RefCell::new(0usize));
        handle.on_editor_request({
            let asked = asked.clone();
            move |_request| {
                *asked.borrow_mut() += 1;
                Ok(())
            }
        });
        let mut senders = Vec::new();
        for (page_view, id) in [(view(2), "v-2"), (ViewId::BASE, "v-base")] {
            let (tx, rx) = mpsc::channel();
            senders.push(tx);
            handle.state.borrow_mut().view_jobs.push(PendingView {
                asker: ViewAsker::Page {
                    view: page_view,
                    request_id: id.into(),
                },
                rx,
            });
        }
        handle.idle_page(view(2));
        let left: Vec<String> = handle
            .state
            .borrow()
            .view_jobs
            .iter()
            .map(|job| match &job.asker {
                ViewAsker::Page { request_id, .. } => request_id.clone(),
                ViewAsker::Window { .. } => "window".to_string(),
            })
            .collect();
        assert_eq!(left, vec!["v-base".to_string()], "only the idle page's view goes");
        clear(&[&base, &second]);
        handle.tick();
        assert_eq!(*asked.borrow(), 0);
        assert_eq!(
            result_for(&second.sent.borrow(), "v-2"),
            None,
            "the idle page is answered nothing"
        );
        handle.state.borrow_mut().view_jobs.clear();
    }

    /// `review.enabled` and `review.hint` reach the panel's state: enabled installs a review and the hint
    /// follows its key; disabled leaves none and a request for one says why.
    #[test]
    fn the_review_keys_reach_the_panel_state() {
        use eitri_core::turn_review::{Scope, TurnRef};
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        state.borrow_mut().apply_review(ReviewConfig {
            enabled: true,
            hint: ReviewHintLevel::Off,
            live: false,
        });
        assert!(state.borrow().tabs.turn_review().is_some());
        assert!(state.borrow().review_enabled);
        assert_eq!(state.borrow().review_hint, ReviewHintLevel::Off);
        state.borrow_mut().apply_review(ReviewConfig {
            enabled: true,
            hint: ReviewHintLevel::Full,
            live: false,
        });
        assert_eq!(state.borrow().review_hint, ReviewHintLevel::Full);

        state.borrow_mut().apply_review(ReviewConfig {
            enabled: true,
            hint: ReviewHintLevel::Key,
            live: false,
        });
        assert_eq!(state.borrow().review_hint, ReviewHintLevel::Key);

        // A backend without turn boundaries installs none, so the page is not told `c` would open one.
        let state = state_for_hooks(TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto));
        state.borrow_mut().apply_review(ReviewConfig {
            enabled: true,
            hint: ReviewHintLevel::Full,
            live: false,
        });
        assert!(state.borrow().tabs.turn_review().is_none());
        assert!(!state.borrow().review_enabled);

        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        state.borrow_mut().apply_review(ReviewConfig {
            enabled: false,
            hint: ReviewHintLevel::Full,
            live: false,
        });
        let state = state.borrow();
        assert!(state.tabs.turn_review().is_none());
        assert!(!state.review_enabled);
        let tab = state.tabs.active();
        assert_eq!(
            state
                .tabs
                .review_overview_job(tab, TurnRef::Latest, Scope::Turn)
                .err()
                .as_deref(),
            Some(eitri_core::tab_set::TURN_REVIEW_OFF)
        );
    }

    /// Off drops hints before they are queued; key and full both keep every hint.
    #[test]
    fn hints_are_kept_unless_off() {
        use eitri_core::turn_review::ReviewHint;
        let hints = || {
            vec![
                ReviewHint {
                    tab: 1,
                    turn: 1,
                    files: 3,
                },
                ReviewHint {
                    tab: 2,
                    turn: 1,
                    files: 0,
                },
            ]
        };
        assert!(hints_to_keep(ReviewHintLevel::Off, hints()).is_empty());
        assert_eq!(hints_to_keep(ReviewHintLevel::Full, hints()), hints());
        assert_eq!(hints_to_keep(ReviewHintLevel::Key, hints()), hints());
    }

    /// A hint that arrives while the page is not ready is kept for it, one per tab, the newest.
    #[test]
    fn a_waiting_review_hint_is_replaced_only_by_the_same_tabs_newer_one() {
        use eitri_core::turn_review::ReviewHint;
        let hint = |tab, turn, files| ReviewHint { tab, turn, files };
        let mut waiting = Vec::new();
        keep_newest_hint_per_tab(&mut waiting, vec![hint(1, 1, 3), hint(2, 1, 1)]);
        keep_newest_hint_per_tab(&mut waiting, vec![hint(1, 2, 0)]);
        assert_eq!(waiting, vec![hint(2, 1, 1), hint(1, 2, 0)]);
        keep_newest_hint_per_tab(&mut waiting, Vec::new());
        assert_eq!(waiting.len(), 2);
    }

    /// What the page was sent is kept for the document that replaces it: a hint with files stands for its
    /// tab until a newer one, and one with none clears it.
    #[test]
    fn a_shown_review_hint_is_kept_until_a_newer_one_for_its_tab_replaces_or_clears_it() {
        use eitri_core::turn_review::ReviewHint;
        let hint = |tab, turn, files| ReviewHint { tab, turn, files };
        let mut shown = Vec::new();
        keep_shown_hints(&mut shown, &[hint(1, 1, 2), hint(2, 1, 5)]);
        keep_shown_hints(&mut shown, &[hint(1, 2, 3)]);
        assert_eq!(shown, vec![hint(2, 1, 5), hint(1, 2, 3)]);
        keep_shown_hints(&mut shown, &[hint(2, 2, 0)]);
        assert_eq!(shown, vec![hint(1, 2, 3)]);
        keep_shown_hints(&mut shown, &[]);
        assert_eq!(shown, vec![hint(1, 2, 3)]);
    }

    /// Opening a tab's review of the turn the hint is about, or a newer one, is what clears the hint on the
    /// page; a review of an older turn is not.
    #[test]
    fn asking_for_a_tabs_review_forgets_its_shown_hint_only_when_the_page_would_clear_it() {
        use eitri_core::agent_bridge::ReviewTurnWire;
        use eitri_core::turn_review::ReviewHint;
        let hint = |tab, turn, files| ReviewHint { tab, turn, files };
        let fresh = || vec![hint(1, 4, 2), hint(2, 4, 2)];
        let mut shown = fresh();
        forget_hint_for_review(&mut shown, 1, ReviewTurnWire::Latest);
        assert_eq!(shown, vec![hint(2, 4, 2)]);
        let mut shown = fresh();
        forget_hint_for_review(&mut shown, 1, ReviewTurnWire::N(3));
        assert_eq!(shown, fresh());
        forget_hint_for_review(&mut shown, 1, ReviewTurnWire::N(4));
        assert_eq!(shown, vec![hint(2, 4, 2)]);
    }

    /// A reloaded document is told again the hints the last one showed, for the tabs that still exist; a
    /// closed tab's is forgotten, and asking twice (two reloads) gives the same answer.
    #[test]
    fn a_new_document_is_owed_the_hints_the_last_one_showed() {
        use eitri_core::turn_review::ReviewHint;
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let tab = state.borrow().tabs.active().0;
        let live = ReviewHint { tab, turn: 2, files: 3 };
        keep_shown_hints(
            &mut state.borrow_mut().shown_hints,
            &[
                live.clone(),
                ReviewHint {
                    tab: tab + 100,
                    turn: 1,
                    files: 1,
                },
            ],
        );
        assert_eq!(
            hints_for_new_document(&mut state.borrow_mut(), ViewId::BASE),
            vec![live.clone()]
        );
        assert_eq!(
            hints_for_new_document(&mut state.borrow_mut(), ViewId::BASE),
            vec![live]
        );
    }

    /// A review's answer is the worker's envelope as it is; a job that failed or whose worker died
    /// is a failing `command_result` for its own request; one still running is left in the list.
    #[test]
    fn finished_reviews_answer_each_request_once_and_leave_the_running_one() {
        let (done_tx, done_rx) = mpsc::channel();
        let (failed_tx, failed_rx) = mpsc::channel();
        let (died_tx, died_rx) = mpsc::channel::<Result<String, String>>();
        let (_running_tx, running_rx) = mpsc::channel::<Result<String, String>>();
        done_tx
            .send(Ok(r#"{"kind":"review","requestId":"a"}"#.to_string()))
            .unwrap();
        failed_tx.send(Err("no such turn".to_string())).unwrap();
        drop(died_tx);
        let mut jobs = vec![
            PendingReview {
                request_id: "a".into(),
                view: ViewId::BASE,
                rx: done_rx,
                clears_hint: Some((7, eitri_core::agent_bridge::ReviewTurnWire::Latest)),
            },
            PendingReview {
                request_id: "b".into(),
                view: ViewId::BASE,
                rx: failed_rx,
                clears_hint: Some((7, eitri_core::agent_bridge::ReviewTurnWire::Latest)),
            },
            PendingReview {
                request_id: "c".into(),
                view: ViewId::BASE,
                rx: died_rx,
                clears_hint: Some((7, eitri_core::agent_bridge::ReviewTurnWire::Latest)),
            },
            PendingReview {
                request_id: "d".into(),
                view: ViewId::BASE,
                rx: running_rx,
                clears_hint: Some((7, eitri_core::agent_bridge::ReviewTurnWire::Latest)),
            },
        ];
        let (payloads, cleared) = finished_reviews(&mut jobs);
        // Only the review that was worked out clears a hint; a failed or dead one leaves it on the page.
        assert_eq!(cleared, vec![(7, eitri_core::agent_bridge::ReviewTurnWire::Latest)]);
        assert_eq!(payloads.len(), 3);
        assert_eq!(payloads[0], r#"{"kind":"review","requestId":"a"}"#);
        let failed: serde_json::Value = serde_json::from_str(&payloads[1]).unwrap();
        assert_eq!(
            failed,
            serde_json::json!({ "kind": "command_result", "requestId": "b", "ok": false, "error": "no such turn" })
        );
        let died: serde_json::Value = serde_json::from_str(&payloads[2]).unwrap();
        assert_eq!(
            (died["requestId"].as_str(), died["ok"].as_bool()),
            (Some("c"), Some(false))
        );
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].request_id, "d");
        let (again, cleared_again) = finished_reviews(&mut jobs);
        assert!(again.is_empty() && cleared_again.is_empty(), "nothing is sent twice");
    }

    /// An editor that went away takes its pending draft edits with it: a body saved in nvim becomes
    /// the draft, an unsaved one leaves it alone, and the files are gone afterwards.
    #[test]
    fn ending_pending_edits_keeps_a_saved_body_and_removes_the_files() {
        let dir = std::env::temp_dir().join(format!("ap-{}-edits", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let saved = set.active();
        let unsaved = set.open(eitri_core::tab_set::AgentId::Claude);
        set.set_draft(saved, "old");
        set.set_draft(unsaved, "kept\n");
        let edit = |id: u64, body: &str| {
            let edit = eitri_core::scratch::PendingEdit {
                id,
                body: dir.join(format!("{id}-draft.md")),
                done: dir.join(format!("{id}-draft.done")),
            };
            std::fs::write(&edit.body, body).unwrap();
            edit
        };
        let (first, second) = (edit(1, "new text\n"), edit(2, "kept\n"));
        set.begin_scratch_edit(saved, 1).unwrap();
        set.begin_scratch_edit(unsaved, 2).unwrap();
        let state = state_for_hooks(set);
        state.borrow_mut().pending_edits = vec![first.clone(), second.clone()];

        let ended = end_pending_edits(&mut state.borrow_mut());

        assert_eq!(ended, vec![(saved, Some("new text".to_string())), (unsaved, None)]);
        let state = state.borrow();
        assert!(state.pending_edits.is_empty());
        assert_eq!(state.tabs.get(saved).unwrap().draft(), "new text");
        assert_eq!(state.tabs.get(unsaved).unwrap().draft(), "kept\n");
        assert!(
            !first.body.exists() && !second.body.exists(),
            "the scratch files are removed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A chat that came from the other window while its draft was out in this window's editor: the edit's end
    /// reaches the tab by its session, and a failed one leaves the draft as it was.
    #[test]
    fn a_draft_edit_brought_by_a_moved_chat_ends_in_the_tab_that_holds_it() {
        use eitri_core::scratch::EditDone;
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let moved = set.active();
        let other = set.open(eitri_core::tab_set::AgentId::Claude);
        let mut senders = Vec::new();
        for (tab, session) in [(moved, "s-moved"), (other, "s-other")] {
            let (tx, rx) = mpsc::channel();
            senders.push(tx);
            set.start(
                tab,
                eitri_core::tab_set::PendingStart {
                    request_id: format!("r-{session}"),
                    result_rx: rx,
                    first_turn: None,
                    resume: Some(session.into()),
                    resumed_title: None,
                    resumed_name: None,
                },
            );
        }
        set.set_draft(moved, "before");
        set.set_draft(other, "untouched");
        // The edit's number is the other window's: this window never handed it out.
        set.begin_scratch_edit(moved, 41).unwrap();
        let state = state_for_hooks(set);
        let mut out = Outbox::default();

        assert!(!finish_moved_scratch_edit(
            &mut state.borrow_mut(),
            "s-other",
            &EditDone::Written("x".into()),
            &mut out
        ));
        assert!(finish_moved_scratch_edit(
            &mut state.borrow_mut(),
            "s-moved",
            &EditDone::Written("edited in nvim".into()),
            &mut out
        ));
        {
            let state = state.borrow();
            assert_eq!(state.tabs.get(moved).unwrap().draft(), "edited in nvim");
            assert_eq!(state.tabs.get(other).unwrap().draft(), "untouched");
            assert_eq!(
                state.tabs.get(moved).unwrap().editing_draft,
                None,
                "its next Ctrl+g works"
            );
        }
        assert!(
            !finish_moved_scratch_edit(
                &mut state.borrow_mut(),
                "s-moved",
                &EditDone::Written("again".into()),
                &mut out
            ),
            "an edit ends once"
        );

        state.borrow_mut().tabs.begin_scratch_edit(moved, 42).unwrap();
        assert!(finish_moved_scratch_edit(
            &mut state.borrow_mut(),
            "s-moved",
            &EditDone::Failed("E212".into()),
            &mut out
        ));
        let state = state.borrow();
        assert_eq!(state.tabs.get(moved).unwrap().draft(), "edited in nvim");
        assert_eq!(state.tabs.get(moved).unwrap().editing_draft, None);
        drop(senders);
    }

    #[test]
    fn a_proposal_still_being_read_is_dropped_when_its_page_goes_idle() {
        let (handle, base, second) = handle_with_two_pages();
        let asked = Rc::new(RefCell::new(0usize));
        handle.on_editor_request({
            let asked = asked.clone();
            move |_request| {
                *asked.borrow_mut() += 1;
                Ok(())
            }
        });
        let mut senders = Vec::new();
        for (page_view, id) in [(view(2), "p-2"), (ViewId::BASE, "p-base")] {
            let (tx, rx) = mpsc::channel();
            senders.push(tx);
            handle.state.borrow_mut().proposal_jobs.push(PendingProposal {
                request_id: id.into(),
                view: page_view,
                name: "a.rs".into(),
                rx,
            });
        }
        handle.idle_page(view(2));
        let left: Vec<String> = handle
            .state
            .borrow()
            .proposal_jobs
            .iter()
            .map(|job| job.request_id.clone())
            .collect();
        assert_eq!(left, vec!["p-base".to_string()], "only the idle page's proposal goes");
        clear(&[&base, &second]);
        assert!(
            senders[0].send(Ok(("a\n".into(), "b\n".into()))).is_err(),
            "the idle page's worker has no one to answer"
        );
        handle.tick();
        assert_eq!(
            *asked.borrow(),
            0,
            "nothing is raised in the editor for a page that is gone"
        );
        assert_eq!(
            result_for(&second.sent.borrow(), "p-2"),
            None,
            "the idle page is answered nothing"
        );
        handle.state.borrow_mut().proposal_jobs.clear();
    }

    /// The review flow's answers go to the page whose request they answer, a tab's draft to the page
    /// that shows the tab, and the window's interrupted reverts to the base page; a request the flow
    /// no longer owes is forgotten.
    #[test]
    fn review_flow_answers_go_to_the_page_that_asked() {
        use eitri_core::turn_review::Out;
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.open_in(view(2)).unwrap();
        let first = handle
            .state
            .borrow_mut()
            .owed
            .owe(view(2), "req-1", Owed::ReviewRequest);
        let other = handle
            .state
            .borrow_mut()
            .owed
            .owe(ViewId::BASE, "req-2", Owed::ReviewRequest);
        clear(&[&base, &second]);
        // The flow answers under the key it was given.
        let answer = |key: &str| Out::Answer {
            request_id: key.to_string(),
            payload: serialize_command_result_for_js(key, Ok(())),
        };
        handle.run_seen(|state, screen, out| {
            dispatch_review_outs(
                state,
                screen,
                vec![
                    answer(&first),
                    answer(&other),
                    Out::Draft(tab, format!(r#"{{"kind":"review_draft","tab":{}}}"#, tab.0)),
                    Out::Envelope(r#"{"kind":"review_recovery","entries":[]}"#.to_string()),
                ],
                out,
            )
        });
        assert_eq!(result_for(&second.sent.borrow(), "req-1"), Some(Ok(())));
        assert_eq!(result_for(&second.sent.borrow(), "req-2"), None);
        assert_eq!(result_for(&base.sent.borrow(), "req-2"), Some(Ok(())));
        assert_eq!(result_for(&base.sent.borrow(), "req-1"), None);
        assert_eq!(
            result_for(&second.sent.borrow(), &first),
            None,
            "the key stays the window's"
        );
        assert!(sent_about(&second, "review_draft", tab));
        assert!(!sent_about(&base, "review_draft", tab));
        let kinds = |page: &RecordingPage| -> Vec<String> {
            page.sent
                .borrow()
                .iter()
                .map(|e| json_of(e)["kind"].as_str().unwrap_or("").to_string())
                .collect()
        };
        assert!(kinds(&base).contains(&"review_recovery".to_string()));
        assert!(!kinds(&second).contains(&"review_recovery".to_string()));
        assert!(handle.state.borrow().owed.is_empty(), "the flow owes neither any more");
    }

    /// How many `command_result`s for `request_id` `page` was sent.
    fn results_named(page: &RecordingPage, request_id: &str) -> usize {
        page.sent
            .borrow()
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "command_result" && v["requestId"] == request_id)
            .count()
    }

    /// Two pages number their review requests alike: page 2's request with the id of one the base page
    /// still has out is worked on as its own, and each answer reaches only the page that asked, once.
    #[test]
    fn two_pages_reviewing_under_one_request_id_are_each_answered_in_their_own_page() {
        use eitri_core::turn_review::Out;
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.open_in(view(2)).unwrap();
        handle
            .state
            .borrow_mut()
            .owed
            .owe(ViewId::BASE, "req-7", Owed::ReviewRequest);
        clear(&[&base, &second]);
        // The tab has no review session: the flow refuses page 2's undo at once, as its own.
        handle.page_message(
            view(2),
            &format!(r#"{{"type":"review_undo","request_id":"req-7","tab":{}}}"#, tab.0),
        );
        assert_eq!(results_named(&second, "req-7"), 1, "{:?}", second.sent.borrow());
        assert_eq!(results_named(&base, "req-7"), 0);

        // Both out at once, answered in one batch with different words: neither takes the other's.
        let seconds = handle
            .state
            .borrow_mut()
            .owed
            .owe(view(2), "req-7", Owed::ReviewRequest);
        let bases = handle
            .state
            .borrow_mut()
            .owed
            .owe(ViewId::BASE, "req-7", Owed::ReviewRequest);
        clear(&[&base, &second]);
        handle.run_seen(|state, screen, out| {
            dispatch_review_outs(
                state,
                screen,
                vec![
                    Out::Answer {
                        request_id: seconds.clone(),
                        payload: serialize_command_result_for_js(&seconds, Err("page two's")),
                    },
                    Out::Answer {
                        request_id: bases.clone(),
                        payload: serialize_command_result_for_js(&bases, Ok(())),
                    },
                ],
                out,
            )
        });
        assert_eq!(
            result_for(&second.sent.borrow(), "req-7"),
            Some(Err("page two's".to_string()))
        );
        assert_eq!(result_for(&base.sent.borrow(), "req-7"), Some(Ok(())));
        assert_eq!(results_named(&second, "req-7"), 1);
        assert_eq!(results_named(&base, "req-7"), 1);
    }

    /// A page that closes with a start, a review request, a `:trust` and a terminal handoff still out
    /// is sent none of their answers, not even when its view is given a new page whose requests reuse
    /// the old ids; the base page, with one of each out under the same id, gets each of its own.
    #[test]
    fn a_closed_page_owes_nothing_and_the_rest_are_answered() {
        use eitri_core::turn_review::Out;
        agent::state_dirs::redirect_state_to_a_test_root();
        let (handle, base, second) = handle_with_two_pages();
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);
        let started_from_base = handle.state.borrow().tabs.active();
        // Shown in page 3: what page 2 asked about it must not land there either.
        let started_from_second = handle.open_in(view(3)).unwrap();
        let handed_from_base = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let handed_from_second = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let mut closes = Vec::new();
        for (tab, asked_from) in [(handed_from_base, ViewId::BASE), (handed_from_second, view(2))] {
            let (closed_tx, closed_rx) = mpsc::channel();
            let (_dropped_tx, dropped_rx) = mpsc::channel();
            let mut state = handle.state.borrow_mut();
            state.owed.owe(asked_from, "r1", Owed::Handoff(tab));
            state.tabs.get_mut(tab).unwrap().pending_handoff = Some(PendingHandoff {
                request_id: "r1".into(),
                command: a_command(),
                closed_rx,
                dropped_rx,
            });
            closes.push(closed_tx);
        }
        let (reviews, trusts) = {
            let mut state = handle.state.borrow_mut();
            note_start_page(&mut state, started_from_base, "r1", ViewId::BASE);
            note_start_page(&mut state, started_from_second, "r1", view(2));
            let reviews = [
                state.owed.owe(ViewId::BASE, "r1", Owed::ReviewRequest),
                state.owed.owe(view(2), "r1", Owed::ReviewRequest),
            ];
            let trusts = [
                state.owed.owe(
                    ViewId::BASE,
                    "r1",
                    Owed::TrustCommand {
                        tab: started_from_base,
                        typed_in: ViewId::BASE,
                    },
                ),
                state.owed.owe(
                    view(2),
                    "r1",
                    Owed::TrustCommand {
                        tab: started_from_second,
                        typed_in: view(2),
                    },
                ),
            ];
            (reviews, trusts)
        };
        assert_eq!(handle.state.borrow().owed.owed_to(view(2)), 4);

        handle.idle_page(view(2));
        // The view's page is used again: its new document numbers its requests from the start.
        let reopened = attach_recording_page(&handle, view(2));
        handle.page_message(view(2), READY);
        assert_eq!(
            handle.state.borrow().owed.owed_to(view(2)),
            0,
            "the closed page is owed nothing"
        );
        assert_eq!(handle.state.borrow().owed.owed_to(ViewId::BASE), 4);
        clear(&[&base, &second, &third, &reopened]);

        for tab in [started_from_base, started_from_second] {
            handle.run(|state, out| {
                send_start_answer(state, tab, "r1", serialize_command_result_for_js("r1", Ok(())), out)
            });
        }
        handle.run_seen(|state, screen, out| {
            dispatch_review_outs(
                state,
                screen,
                reviews
                    .iter()
                    .map(|key| Out::Answer {
                        request_id: key.clone(),
                        payload: serialize_command_result_for_js(key, Ok(())),
                    })
                    .collect(),
                out,
            )
        });
        let mut out = Outbox::default();
        for key in &trusts {
            let ended = end_trust_command(&mut handle.state.borrow_mut(), key);
            if let Some((typed_in, request_id)) = ended {
                out.push(typed_in, serialize_command_result_for_js(&request_id, Ok(())));
            }
        }
        out.deliver(&handle.state, &handle.pages);
        for closed in closes {
            closed.send(()).unwrap();
        }
        tick_handle_until(&handle, "both handoffs collected", || {
            let state = handle.state.borrow();
            [handed_from_base, handed_from_second]
                .iter()
                .all(|tab| state.tabs.get(*tab).unwrap().pending_handoff.is_none())
        });

        assert_eq!(results_named(&base, "r1"), 4, "{:?}", base.sent.borrow());
        for (page, which) in [
            (&reopened, "page 2's new document"),
            (&second, "page 2's old one"),
            (&third, "page 3"),
        ] {
            assert_eq!(results_named(page, "r1"), 0, "{which}: {:?}", page.sent.borrow());
        }
        assert!(
            handle.state.borrow().owed.is_empty(),
            "every answer is spent or dropped"
        );
    }

    /// A tab whose turn left a review hint in page 2 brings it to the base page when it joins back, and
    /// takes it to page 2 when it breaks out again: the page it arrives in was never sent it.
    #[test]
    fn a_moved_tab_brings_its_review_hint_to_its_new_page() {
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.open_in(view(2)).unwrap();
        let hint = eitri_core::turn_review::ReviewHint {
            tab: tab.0,
            turn: 1,
            files: 2,
        };
        handle.state.borrow_mut().shown_hints.push(hint.clone());
        clear(&[&base, &second]);
        assert_eq!(handle.join_back(view(2)), Ok(tab));
        assert!(sent_about(&base, "review_hint", tab), "{:?}", base.sent.borrow());
        clear(&[&base, &second]);
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        assert!(sent_about(&second, "review_hint", tab), "{:?}", second.sent.borrow());
        assert!(!sent_about(&base, "review_hint", tab));
    }
}
