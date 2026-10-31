//! The real agent-ui panel: the page a host supplies (`panel_page::PageSurface`) running the embedded
//! `agent-ui/web` frontend, its messages in (JS -> Rust, `AgentPanelHandle::page_message`), and a fast poll
//! of every tab's backend pushed to the page (Rust -> JS).
//!
//! **One backend per tab**. The tabs themselves live in
//! `eitri_core::tab_set::TabSet`; this module spawns the workers that set only holds receivers
//! for, drains every tab on one tick (run when due, `crate::panel_wake`), and routes each inbound command to the tab it names --
//! never to "the active one". Backend construction is lazy per tab: an empty
//! tab starts its backend on its first `send_message` (or a `resume`). No mode goes into it:
//! every session is gated, and the tab's own mode is read where requests are answered,
//! never sent to the CLI. Which backend gets built is `eitri_core::agent_backend`'s decision.
//!
//! **State is server-originated.** For the sidecar backend this module folds nothing of its own:
//! `active_turn_id`, tool calls and permissions all arrive as real events through `pump()`. A
//! command that succeeds returns no events at all; the UI updates on the next poll. Nothing here
//! synthesizes an optimistic `TurnStarted` to make the UI feel faster -- doing so would put the
//! panel's idea of "a turn is running" ahead of the server's, which is the exact shadow state the
//! runtime design forbids.
//!
//! **Each page draws only the tab it shows.** The panel keeps one page per chat view: the base view's
//! page draws its active tab, an extra view's page its one bound tab, and every payload about a tab
//! goes to the page that shows it (`AgentPanelState::page_of`). A background base-view tab's events
//! feed its attention and mark it stale (`TabSet::pump_views`); a switch always sends the new active
//! tab's full snapshot. A message from a page is handled as that page's: its view comes from the
//! handler the page was built with, never from what the page says.
//!
//! **Steps change the state; the outbox makes the calls.** The functions that handle a message, a tick
//! or a verb are steps over `&mut AgentPanelState`: each records what it owes the pages and the window in
//! an `Outbox`, and only `AgentPanelHandle`'s methods hold the shared state, run a step under one borrow
//! and deliver its outbox once that borrow has ended (`outbox.rs`).
//!
//! **One file per concern.** This file holds the state, its one constructor (`AgentPanelState::new`) and
//! the names the panel exports. Each concern is a child module in its own file beside this one, so the
//! state's fields stay private to the panel and every module reads them: the handle (`handle.rs`), the
//! tick, the pages and what goes to which (`pages.rs`, `routing.rs`), a message's admission and its arms
//! (`admission.rs`, `inbound.rs`, `verbs.rs`), starts and the trust question (`starts.rs`, `trust.rs`),
//! the restore, a tab leaving with its session (`handoffs.rs`), moving to the other window (`chat_moves.rs`) or handed
//! to another window by resume (`hand_over.rs`),
//! teardown, turn review's side
//! (`review_glue.rs`), the chooser and detail rows (`detail.rs`), and links. A scan of the panel's code
//! reads every one of them.

#[path = "admission.rs"]
mod admission;
#[path = "chat_moves.rs"]
mod chat_moves;
#[path = "detail.rs"]
mod detail;
#[path = "hand_over.rs"]
mod hand_over;
#[path = "handle.rs"]
mod handle;
#[path = "handoffs.rs"]
mod handoffs;
#[path = "inbound.rs"]
mod inbound;
#[path = "links.rs"]
mod links;
#[path = "outbox.rs"]
mod outbox;
#[path = "pages.rs"]
mod pages;
#[path = "pair_review.rs"]
mod pair_review;
#[path = "restore.rs"]
mod restore;
#[path = "review_glue.rs"]
mod review_glue;
#[path = "routing.rs"]
mod routing;
#[path = "starts.rs"]
mod starts;
#[path = "teardown.rs"]
mod teardown;
#[path = "tick.rs"]
mod tick;
#[path = "trust.rs"]
mod trust;
#[path = "verbs.rs"]
mod verbs;

use crate::panel_document::{PanelDocument, IDLE_DOCUMENT, PANEL_BASE_URI};
use crate::panel_page::{PanelHost, PanelPage};
use crate::trust_gate::{AnswerOutcome, Asked, CommandAnswer, GateEvent, Route, TrustGate};
use agent::setting_sources::ProjectTrust;
use eitri_core::agent_backend::{AgentBackend, BackendError, BackendGreeting, BackendKind};
use eitri_core::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_confirm_restore_for_js, serialize_error_for_js,
    serialize_hello_with_restore_for_js, serialize_nav_key_for_js, ChooserRecord, ChooserTab, DetailRow,
    InboundMessage, NavKeyDirection, PaneNavDirection, PanelKeys, SessionModeChoice, TabVerbWire,
};
use eitri_core::keymap::TabAction;
use eitri_core::layout::Direction;
use eitri_core::lua::agent::AgentRegistration;
use eitri_core::project_trust::Profile;
use eitri_core::saved_tabs::{SavedTabs, TabWriter};
use eitri_core::tab_restore::{RestoreOffer, RestorePolicy, RestoreRun};
use eitri_core::tab_set::{
    AgentId, AwaitingTrust, BypassPolicy, DeferredStart, FailedHow, FirstTurn, PendingHandoff, PendingStart,
    RemovedTab, RestoreConnect, RestoreGo, RestorePrompt, RestoreStep, ResumeRoute, Retired, StartCollected, Tab,
    TabBackend,
};
pub use eitri_core::tab_set::{TabPlace, ViewId};
use eitri_core::tabs::TabId;
use outbox::{ask_gate, Dest, Effect, Outbox, Screen, Strip};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;

use admission::*;
pub use chat_moves::ChatMoveEvent;
use detail::*;
pub use hand_over::TabAnswer;
pub use handle::{AgentPanelHandle, PanelInit, SupervisorLink};
pub use handoffs::PanelTabHandoff;
use handoffs::*;
use inbound::*;
use links::*;
pub use links::{clicked_link_url, open_path_target};
use pages::*;
use pair_review::pair_outbound;
pub use pair_review::{take_pair_outbound, PairOutbound, ReviewSettled, ReviewStep};
use restore::*;
pub use review_glue::ReviewConfig;
use review_glue::*;
use routing::*;
use starts::*;
use teardown::*;
pub use teardown::{ShutdownWatch, CLOSE_WATCH_POLL};
use tick::*;
use trust::*;
use verbs::*;

/// The panel's two HINT messages, demuxed out of `InboundMessage` for `shell::hint::HintCoordinator`
/// -- it never sees the wire's `request_id`, only what the coordinator itself needs to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintInbound {
    /// `f` in the panel's BROWSE: start a global HINT (or cancel one already active, same as the
    /// window-level toggle).
    Request,
    /// The panel's answer to `hint_collect`: how many visible targets it froze for `session_id`.
    Targets { session_id: u64, count: usize },
}

struct AgentPanelState {
    /// The bundle every document load starts from (`PanelDocument::themed`).
    document: &'static PanelDocument,
    /// The process around the panel: where a checked link or address is opened.
    host: Rc<dyn PanelHost>,
    /// Every tab and which is active. Rust owns the tabs; the WebView
    /// draws the active one.
    tabs: eitri_core::tab_set::TabSet,
    backend_kind: BackendKind,
    project_dir: PathBuf,
    /// `project_dir.canonicalize()`, read once: the `canonical_cwd` a session lease is keyed on
    /// (`agent::lease::SessionLease::is_held`), for resume routing and the chooser.
    canonical_project_dir: String,
    /// `$XDG_STATE_HOME/eitri/agent`, where the remembered mode is written. `None`
    /// with no state directory.
    prefs_dir: Option<PathBuf>,
    /// Where the user is, asked for at send time. Never stored across turns: what the editor was
    /// showing when the LAST turn went out is not context, it is a stale claim, and the composer
    /// has no way to tell the model which it is.
    editor_context: eitri_core::editor_context::ContextSource,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    /// Set only while a freshly-spawned `eitri-supervisor` is still coming up. The pump drains
    /// it into `supervisor` above. A window that spawns the supervisor cannot connect to it
    /// synchronously -- see `PendingSupervisor` for the failure that taught us that.
    supervisor_pending: Option<std::sync::mpsc::Receiver<Option<crate::supervisor_client::SupervisorClient>>>,
    /// Set by `AgentPanelHandle::shutdown`, i.e. only on the window-close path, and never cleared.
    ///
    /// **It exists because the main loop now outlives the window.** `shutdown` holds the
    /// application so the process can finish tearing its children down, which means every
    /// main-loop source this panel installed keeps firing for up to `SESSION_CLOSE_BACKSTOP`
    /// afterwards -- against a destroyed window and a `WebView` that is no longer in a widget
    /// tree. The pump reads this and stops; `poll_activate` reads it and stops answering, because
    /// its caller's reaction is `window.present()` on a window GTK has already destroyed.
    ///
    /// **Also the guard on the tab set:** `shutdown` empties it with `TabSet::take_all`, after which
    /// `active_tab()` would panic, so every path that reaches the set checks this first.
    shutting_down: bool,
    /// What tells the host to run the tick: raised by backends' folds and by every call into the panel.
    /// `None` when the socket pair could not be made, and then the tick runs on its old steady cadence.
    wake: Option<crate::panel_wake::PanelWake>,
    /// When the last tick started: a wake runs the next one no sooner than [`AgentPanelHandle::TICK`] after.
    last_tick: Option<std::time::Instant>,
    /// Ticks that ran (past the close check), for a test counting how often an idle panel wakes.
    #[cfg(any(test, feature = "test-support"))]
    ticks_run: u64,
    /// The panel's current theme. Starts on the theme its parts carry (the start set the host read)
    /// and is replaced by `AgentPanelHandle::set_theme`. Held as tokens rather than an envelope because it feeds three
    /// things: the `ready` batch, the `<style>` inlined into every `load_html`, and the WebView's
    /// own background colour.
    theme: eitri_core::theme::ThemeTokens,
    /// The `?` overlay's rows (`serialize_keymap_for_js`), sent with the theme on every `ready` so
    /// a reloaded document is told the keymap it needs, not the one baked into a hand list.
    keymap_help: Option<String>,
    /// Why the project folder is no boundary, when it is not one: sent after the `hello` of every `ready`.
    root_note: Option<String>,
    /// One record per chat page that is attached and live, by the view it shows, and always one for
    /// the base view. A page that goes idle loses its record, so nothing here is ever sent to it again
    /// until it is attached anew.
    pages: BTreeMap<ViewId, PageState>,
    /// How many `tabs` envelopes have been recorded, over every page: each one's number
    /// ([`outbox::Strip`]), so one recorded earlier is known for stale once a later one reached its page.
    tabs_recorded: u64,
    /// Told once per extra view left holding no tab (`AgentPanelHandle::on_view_empty`).
    view_empty_hook: Option<Rc<dyn Fn(ViewId)>>,
    /// Told when a pick asks the window to show a view and give it the keys
    /// (`AgentPanelHandle::on_show_view`).
    show_view_hook: Option<Rc<dyn Fn(ViewId)>>,
    /// Extra views whose tab left since the hook was last told, in the order they emptied.
    emptied_views: Vec<ViewId>,
    /// Whether the views left empty by a settled restore have been judged (once per window).
    empty_views_judged: bool,
    /// The page the last chooser was opened in: its newer titles go there.
    chooser_view: ViewId,
    /// Every answer a page is still owed for a request that finishes later -- a tab's start, a review
    /// request, a `:trust`/`:untrust`, a terminal handoff -- with the page that sent it and the id that
    /// page knows it by. One ledger, so a page that closes is forgotten by every kind at once.
    owed: OwedAnswers,
    /// `agent.typing_cadence_hz` as last set, for a page attached after it was.
    typing_cadence: Option<u32>,
    /// Whether the editor holds the keys, for a page attached after it was last said.
    editor_has_keys: bool,
    /// How the page shows the agent's thinking (`init.lua`'s `agent.thinking`), sent on every `ready`
    /// so a reloaded document draws thinking the way the first one did.
    thinking_display: eitri_core::agent_prefs::ThinkingDisplay,
    /// Where the panel's two HINT messages (`hint_request`, `hint_targets`) go. `None` until
    /// `shell::hint::HintCoordinator` installs one via `AgentPanelHandle::on_hint`. An `Rc<dyn Fn>`
    /// rather than a plain closure field because the hook must be cloned out of the borrow before
    /// being called -- see `handle_inbound_message`'s HintRequest/HintTargets arms for why.
    hint_hook: Option<Rc<dyn Fn(HintInbound)>>,
    /// The window's check on every answer from a page (`AgentPanelHandle::set_answer_gate`), asked
    /// with that page's view: `Err(why)` refuses the answer and leaves the card or question waiting.
    /// Cloned out of the borrow before it is called, as `hint_hook` is, so a gate that reads the panel
    /// cannot find the state held.
    answer_gate: Option<AnswerGate>,
    /// The window's word on whether a float covers the chat now (`AgentPanelHandle::set_coverage`). A
    /// covered chat is off screen for attention: what finishes under a float is not seen. Cloned out
    /// of the borrow before it is called, as `answer_gate` is.
    coverage: Option<Coverage>,
    /// Told whenever the window's attention (every tab's, summed: `TabSet::attention`) or one tab's
    /// own changes, with the window's value before and after and the views handed a card: each chat
    /// chip's `⚑N`, the toast, `on_permission`.
    attention_hook: Option<AttentionHook>,
    /// Running or queued work changed in these views; called after the step releases its state.
    busy_hook: Option<Rc<dyn Fn(&[(ViewId, bool)])>>,
    reported_activity: Vec<(ViewId, eitri_core::tab_set::ViewActivity)>,
    /// Which tabs ran a turn when the hook was last told, each in its view (`report_busy`).
    reported_turns: BTreeSet<(ViewId, TabId)>,
    /// Every tab's attention as the hook was last told it (`report_attention`).
    reported_tabs: Vec<eitri_core::tabs::TabAttention>,
    /// The per-window scratch directory, `None` if it could not be made.
    scratch: Option<eitri_core::scratch::ScratchDir>,
    /// Edits out in nvim, polled by the tick until their marker appears.
    pending_edits: Vec<eitri_core::scratch::PendingEdit>,
    task_commands: Vec<inbound::PendingTaskCommand>,
    /// Reviews being worked out on worker threads, polled by the tick: git never runs on the GTK
    /// thread, so the answer comes back through a channel the tick drains.
    review_jobs: Vec<PendingReview>,
    /// Answers to Claude's questions on their way to the agent, polled by the tick: the GTK thread
    /// never waits for the agent's reply, and the page that answered is told once it came.
    question_sends: Vec<PendingQuestionSend>,
    /// Pending edits' proposals being read on worker threads, polled by the tick: the agent names the
    /// file, and nothing it names is opened on the GTK thread.
    proposal_jobs: Vec<PendingProposal>,
    /// Status-band hints not yet shown: the page was not ready (loading, or reloading) when they
    /// arrived. At most one per tab, the newest.
    review_hints: Vec<eitri_core::turn_review::ReviewHint>,
    /// The hints the page has been sent and has not since been told to drop, at most one per tab:
    /// the page is only a view, so a document that replaces it (`prefix r`, a crashed WebView) is
    /// owed these again. Dropped when the tab's next turn starts, when that tab's review is answered
    /// with a review, and when a newer hint for the tab arrives.
    shown_hints: Vec<eitri_core::turn_review::ReviewHint>,
    /// Whether a finished turn's hint reaches the status band (`review.hint`). Off until
    /// `set_review` says otherwise: the review is still one key away.
    review_hint: bool,
    /// Whether a turn review is installed, so `c` can open one: `review.enabled` and a backend that
    /// has turn boundaries. Off until `set_review` installs one. Told to the page with `review_hint`
    /// (`review_settings`), which draws nothing about the turn's changes unless both are on.
    review_enabled: bool,
    /// What `set_review` builds the turn review from: the state and home directories and the
    /// canonical project root, read once when the state is made.
    review_roots: ReviewRoots,
    /// shows and focuses the editor, then hands it these keys; `Err` says why it could not.
    editor_request_hook: Option<EditorRequestHook>,
    /// How to ask the editor something and poll the answer: the embedded pane or the companion's
    /// link. `None` until the window hands one over.
    editor_rpc: Option<Rc<dyn eitri_core::editor_rpc::EditorRpc>>,
    /// The review's reverts, undos, comments, sends and recoveries in flight; the tick drives it.
    review_flow: eitri_core::turn_review::ReviewFlow,
    /// What the window gives the flow's editor overlay: who owns the module in its nvim, and what to
    /// do once a file opened there.
    review_editor: crate::review_editor::ReviewEditor,
    /// Where what the link owes the other window of a linked pair goes after each tick; `None` in a window with no
    /// partner, where nothing is taken for one.
    pair_outbound_hook: Option<Rc<dyn Fn(PairOutbound)>>,
    /// An edit came back (or was discarded): the keys return to the chat, in INPUT, in the view of the
    /// edit's tab.
    editor_done_hook: Option<Rc<dyn Fn(ViewId)>>,
    /// Teardowns and connects of tabs that left while the window stays open, so the window-close
    /// backstop covers them too. See [`Retiring`].
    retiring: Retiring,
    /// Per tab, the session `r` is continuing in it and its old backend's teardown: that session's lease is
    /// let go only when the old backend is gone, so a start resuming that session waits for it. Taken by the
    /// tab's next external start whatever it is, and dropped when the start is put off or the tab closes.
    resume_after: std::collections::HashMap<TabId, (String, mpsc::Receiver<()>)>,
    /// Writes the project's prompts to `$XDG_STATE_HOME/eitri/history` off this thread (disabled with no
    /// state directory). `rules_dir` is `.../permissions`, `None` with no state directory.
    history_writer: eitri_core::prompt_history::HistoryWriter,
    rules_dir: Option<PathBuf>,
    /// The project's prompts, oldest first: as read at the start, with this window's own appended at once,
    /// and replaced by the file's entries each time the history's writer reports a write.
    history: Vec<String>,
    /// The last `editor_link` envelope (companion mode), kept so a fresh document is told on
    /// `ready`. `None` in a window with no companion, which never sends one.
    editor_link: Option<String>,
    /// Where a `tab_verb` message's mapped `TabAction` goes, with the view of the page that sent it.
    /// `None` until `main.rs`'s `AgentPanelHandle::on_tab_verb` installs one.
    tab_verb_hook: Option<Rc<dyn Fn(ViewId, TabAction)>>,
    /// Where a `nav_fallthrough` message's direction goes, and a `pane_nav`'s (`Ctrl+w h/j/k/l` --
    /// the same move, from a key the page read itself), with the view of the page that sent it:
    /// `None` until `main.rs`'s `AgentPanelHandle::on_nav_fallthrough` installs one, which is why
    /// `InboundMessage::NavFallthrough` and `InboundMessage::PaneNav` can be refused.
    nav_fallthrough_hook: Option<Rc<dyn Fn(ViewId, Direction)>>,
    /// Where a `keymap_closed` message goes (the opening it closes, and whether the overlay's own keys
    /// closed it): `None` in a window that never gives the keys back (the companion window, `eitri-mac`),
    /// where the message is accepted and does nothing.
    keymap_closed_hook: Option<Rc<dyn Fn(u64, bool)>>,
    /// Keeps `$XDG_STATE_HOME/eitri/tabs/<project>.json` in step with the tabs, once per tick.
    tab_memory: TabWriter,
    /// The CLI's own session titles, read on a worker; a title that changed raises the wake and `hello`
    /// is sent again.
    titles: eitri_core::transcript_titles::TitleCache,
    /// A title read since the last `hello`: it is sent again.
    hello_titles_stale: bool,
    /// The records the last chooser listed, each with the title it showed, so a title read after it was
    /// sent reaches the rows of a chooser still open.
    chooser_listed: Vec<(String, Option<String>)>,
    /// A title read since the chooser's titles were last compared with the cache.
    chooser_titles_stale: bool,
    /// This project's conversation id (`agent::conversation_id_for_cwd` of its canonical root), which
    /// every saved tab carries.
    conversation_id: String,
    /// What the last window on this project left open, read once at launch. The launch dashboard
    /// offers it, or `agent.restore = "auto"` brings it back; either way it is used up. `None` when
    /// there was nothing, it was unusable, or `agent.restore` is `"off"`.
    restore_source: Option<SavedTabs>,
    /// `agent.restore`, as `init.lua` left it (applied once, after it ran).
    restore_policy: RestorePolicy,
    /// The saved tabs the window offers, as last given after `init.lua` ran; `None` until then.
    tab_files: Option<eitri_core::saved_tabs::TabFiles>,
    /// This window's part in a group's one tabs file.
    group_tabs: restore::GroupTabs,
    /// Window 1 starting window 2 again at its launch: `agent.restore = "auto"` waits until window 2 is back or
    /// is not, so the saved tabs of a window 2 that does not come back return in the same restore as this
    /// window's own (a restore fills only a window that has started nothing).
    launch_restore_held: bool,
    /// What the restore under way says of turns that never finished, in its one message.
    restore_cut_note: Option<String>,
    /// The base page was ready while the launch restore was held: it runs once the hold is let go.
    launch_restore_owed: bool,
    /// Whether `init.lua` made bypass the default mode: the one user whose saved bypass tabs come
    /// back in bypass without being asked.
    bypass_by_config: bool,
    /// Whether this project remembers a mode Shift+Tab left, which the configured default yields to.
    mode_remembered: bool,
    /// The agent a tab made without naming one runs (`agent.default`).
    default_agent: AgentId,
    /// `agent.codex.command` as `init.lua` left it; `EITRI_CODEX` still outranks it.
    codex_command: Option<String>,
    /// The ACP agents `init.lua` registered: what `:tabnew <name>` accepts, the files each one's trust
    /// question is about, and the command its start runs.
    acp_agents: Vec<AgentRegistration>,
    /// What a connect worker has to say while it works (a first Codex start's self-check): sent to
    /// the band by the tick.
    start_notes: StartNotes,
    /// A restore whose resumes have not all returned yet.
    restore_run: Option<RestoreRun>,
    /// Where a restore's one-line result goes: `main.rs`'s window toast.
    toast_hook: Option<ToastHook>,
    /// Whether a session about to start may load the project's own Claude configuration, and the
    /// question that decides it. Every start goes through it (`start_deferred`).
    trust: TrustGate,
    /// What starts a session's backend: the real connect workers, or a test's stand-in.
    spawner: Spawner,
    /// Tabs moving to or from the other window of a linked pair (`chat_moves.rs`).
    chat_moves: chat_moves::ChatMoves,
    hand_overs: hand_over::HandOvers,
}

/// [`AgentPanelState::toast_hook`]: shows one line in the window's toast.
type ToastHook = Rc<dyn Fn(&str)>;

/// [`AgentPanelState::attention_hook`]: called with the window's attention before a change and after
/// it, and the views handed a card since the last call (`tabs::views_handed_a_card`).
type AttentionHook = Rc<dyn Fn(eitri_core::attention::Attention, eitri_core::attention::Attention, &[ViewId])>;

/// `AgentPanelHandle::on_editor_request`: the request to hand nvim; `Err` says why it could not.
type EditorRequestHook = Rc<dyn Fn(&eitri_core::scratch::ScratchRequest) -> Result<(), String>>;

/// Holds this window's presence for every other window's reverts: its window file, and a file per
/// running turn, kept by a thread of its own. It runs for every backend (the legacy one installs
/// no review) and with the review switched off, because a window that reverts must be able to see
/// this one either way. With no state directory nothing can be held, which is said once; every
/// revert in this window is then refused as not holding the presence lock.
fn hold_window_presence(
    tabs: &mut eitri_core::tab_set::TabSet,
    state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    root: &Path,
    pid: u32,
) {
    match eitri_core::turn_review::review_dir_for(state_home, home, root) {
        Some(dir) => tabs.hold_presence(eitri_core::turn_review::PresenceHolder::start(dir, pid)),
        None => eprintln!("[review] no state directory: this window holds no presence lock, so reverts are refused"),
    }
}

/// What a panel's state is built from: what the disk, the environment and the window supply. Everything
/// else starts the same in every panel and is set in one place, [`AgentPanelState::new`].
struct StateParts {
    document: &'static PanelDocument,
    theme: eitri_core::theme::ThemeTokens,
    host: Rc<dyn PanelHost>,
    trust: TrustGate,
    tabs: eitri_core::tab_set::TabSet,
    editor_context: eitri_core::editor_context::ContextSource,
    backend_kind: BackendKind,
    project_dir: PathBuf,
    canonical_project_dir: String,
    prefs_dir: Option<PathBuf>,
    mode_remembered: bool,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    supervisor_pending: Option<std::sync::mpsc::Receiver<Option<crate::supervisor_client::SupervisorClient>>>,
    scratch: Option<eitri_core::scratch::ScratchDir>,
    review_roots: ReviewRoots,
    history_writer: eitri_core::prompt_history::HistoryWriter,
    rules_dir: Option<PathBuf>,
    history: Vec<String>,
    tab_memory: TabWriter,
    conversation_id: String,
    restore_source: Option<SavedTabs>,
    root_note: Option<String>,
}

impl AgentPanelState {
    /// The panel's state over `parts`, every other field at its start: no page ready, nothing installed
    /// by the window yet, no job running. The tab set gets the panel's wake here, so every state ticks on
    /// it the same way.
    fn new(parts: StateParts) -> Self {
        let StateParts {
            document,
            theme,
            host,
            trust,
            mut tabs,
            editor_context,
            backend_kind,
            project_dir,
            canonical_project_dir,
            prefs_dir,
            mode_remembered,
            supervisor,
            supervisor_pending,
            scratch,
            review_roots,
            history_writer,
            rules_dir,
            history,
            tab_memory,
            conversation_id,
            restore_source,
            root_note,
        } = parts;
        let wake = panel_wake(&mut tabs);
        let titles =
            eitri_core::transcript_titles::TitleCache::new(wake.as_ref().map(crate::panel_wake::PanelWake::waker));
        let mut history_writer = history_writer;
        if let Some(wake) = &wake {
            history_writer.set_wake(wake.waker());
        }
        AgentPanelState {
            document,
            host,
            trust,
            spawner: Spawner::Real,
            chat_moves: chat_moves::ChatMoves::default(),
            hand_overs: hand_over::HandOvers::default(),
            tabs,
            tab_memory,
            titles,
            hello_titles_stale: false,
            chooser_listed: Vec::new(),
            chooser_titles_stale: false,
            conversation_id,
            restore_source,
            restore_policy: RestorePolicy::Offer,
            tab_files: None,
            group_tabs: restore::GroupTabs::default(),
            launch_restore_held: false,
            restore_cut_note: None,
            launch_restore_owed: false,
            bypass_by_config: false,
            mode_remembered,
            default_agent: AgentId::Claude,
            codex_command: None,
            acp_agents: Vec::new(),
            start_notes: StartNotes::default(),
            restore_run: None,
            toast_hook: None,
            editor_context,
            backend_kind,
            project_dir,
            canonical_project_dir,
            prefs_dir,
            supervisor,
            supervisor_pending,
            shutting_down: false,
            wake,
            last_tick: None,
            #[cfg(any(test, feature = "test-support"))]
            ticks_run: 0,
            theme,
            keymap_help: None,
            root_note,
            // The base view's record is there from the start: its page is the window's, attached or not.
            pages: BTreeMap::from([(ViewId::BASE, PageState::default())]),
            tabs_recorded: 0,
            view_empty_hook: None,
            show_view_hook: None,
            emptied_views: Vec::new(),
            empty_views_judged: false,
            chooser_view: ViewId::BASE,
            owed: OwedAnswers::default(),
            typing_cadence: Some(eitri_core::panel_cadence::DEFAULT_CADENCE_HZ),
            editor_has_keys: false,
            thinking_display: eitri_core::agent_prefs::ThinkingDisplay::Collapsed,
            hint_hook: None,
            answer_gate: None,
            coverage: None,
            attention_hook: None,
            busy_hook: None,
            reported_activity: Vec::new(),
            reported_turns: BTreeSet::new(),
            reported_tabs: Vec::new(),
            scratch,
            pending_edits: Vec::new(),
            task_commands: Vec::new(),
            review_jobs: Vec::new(),
            question_sends: Vec::new(),
            proposal_jobs: Vec::new(),
            review_hints: Vec::new(),
            shown_hints: Vec::new(),
            review_hint: false,
            review_enabled: false,
            review_roots,
            editor_request_hook: None,
            editor_rpc: None,
            review_flow: eitri_core::turn_review::ReviewFlow::new(),
            review_editor: crate::review_editor::ReviewEditor::default(),
            pair_outbound_hook: None,
            editor_done_hook: None,
            retiring: Retiring::default(),
            resume_after: std::collections::HashMap::new(),
            history_writer,
            rules_dir,
            history,
            editor_link: None,
            tab_verb_hook: None,
            nav_fallthrough_hook: None,
            keymap_closed_hook: None,
        }
    }
}

/// The panel's state, the same with a page or without one (`build_agent_panel`'s `unavailable`):
/// the tab set in the project's remembered mode, its prompt history and saved permission rules.
fn panel_state(
    init: PanelInit,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    supervisor_pending: Option<std::sync::mpsc::Receiver<Option<crate::supervisor_client::SupervisorClient>>>,
    host: Rc<dyn PanelHost>,
) -> AgentPanelState {
    let PanelInit {
        project_dir,
        editor_context,
        scratch,
        backend_kind,
        document,
        theme,
        tab_role,
        root_note,
    } = init;
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    let prefs_dir = eitri_core::agent_prefs::state_dir(state_home.as_deref(), home.as_deref());
    let (mode, notes) = eitri_core::agent_prefs::startup_mode(prefs_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    let mode_remembered = eitri_core::agent_prefs::remembers_a_choice(prefs_dir.as_deref(), &project_dir);
    let history_dir = eitri_core::prompt_history::state_dir(state_home.as_deref(), home.as_deref());
    let rules_dir = eitri_core::permission_store::state_dir(state_home.as_deref(), home.as_deref());
    let (history, notes) = eitri_core::prompt_history::startup(history_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    let history_writer = eitri_core::prompt_history::HistoryWriter::new(history_dir, project_dir.clone());
    let (rules, notes) = eitri_core::permission_store::startup(rules_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    // The key a session lease is taken under (`AgentConversation` canonicalizes its cwd the same
    // way). `main.rs` already canonicalized the root; this only makes the string explicit.
    let canonical_root = project_dir.canonicalize().unwrap_or_else(|_| project_dir.clone());
    let mut tabs = eitri_core::tab_set::TabSet::new(backend_kind, mode);
    tabs.set_rules(rules);
    // Before anything review-related, and whatever the backend or `review.enabled` say: another
    // window's revert has to see this window, and this window's running turns.
    hold_window_presence(
        &mut tabs,
        state_home.as_deref(),
        home.as_deref(),
        &canonical_root,
        std::process::id(),
    );
    // Turn review is installed (or switched off) by `set_review`, once `init.lua` has run.
    let review_roots = ReviewRoots {
        state_home: state_home.clone(),
        home: home.clone(),
        canonical_root: canonical_root.clone(),
    };
    let canonical_project_dir = canonical_root.to_string_lossy().into_owned();
    let tabs_dir = eitri_core::saved_tabs::state_dir(state_home.as_deref(), home.as_deref());
    let trust = trust_gate_for(state_home.as_deref(), home.as_deref(), &canonical_root);
    AgentPanelState::new(StateParts {
        document,
        theme,
        host,
        trust,
        tabs,
        editor_context,
        backend_kind,
        tab_memory: eitri_core::saved_tabs::TabWriter::new_as(tabs_dir, project_dir.clone(), tab_role),
        conversation_id: agent::conversation_id_for_cwd(&canonical_root),
        project_dir,
        canonical_project_dir,
        prefs_dir,
        mode_remembered,
        supervisor,
        supervisor_pending,
        scratch,
        review_roots,
        history_writer,
        rules_dir,
        history,
        // Given once `init.lua` has run: which saved tabs the window offers depends on it.
        restore_source: None,
        root_note,
    })
}

/// A panel state with no supervisor, no disk, no trust worker and nothing installed: what the tests here,
/// and a host's own tests through [`AgentPanelHandle::for_host_test`], build on.
#[cfg(any(test, feature = "test-support"))]
fn bare_state(
    document: &'static PanelDocument,
    host: Rc<dyn PanelHost>,
    tabs: eitri_core::tab_set::TabSet,
    theme: eitri_core::theme::ThemeTokens,
) -> AgentPanelState {
    AgentPanelState::new(StateParts {
        document,
        theme,
        host,
        // No worker and no disk: every start goes on untrusted, as for a root with nothing to
        // trust. A test about the question puts a real gate in its place.
        trust: TrustGate::without_worker(PathBuf::new()),
        tabs,
        editor_context: Rc::new(|| None),
        backend_kind: BackendKind::Sidecar,
        project_dir: PathBuf::new(),
        canonical_project_dir: String::new(),
        prefs_dir: None,
        mode_remembered: false,
        supervisor: None,
        supervisor_pending: None,
        scratch: None,
        review_roots: ReviewRoots {
            state_home: None,
            home: None,
            canonical_root: PathBuf::new(),
        },
        history_writer: eitri_core::prompt_history::HistoryWriter::new(None, PathBuf::new()),
        rules_dir: None,
        history: Vec::new(),
        tab_memory: TabWriter::new(None, PathBuf::new()),
        conversation_id: String::new(),
        restore_source: None,
        root_note: None,
    })
}

/// The window's trust gate over `root` (canonical), its record kept under the state home by the rule
/// every other store follows. Touches no disk: the gate's worker does all of that.
fn trust_gate_for(state_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>, root: &Path) -> TrustGate {
    let store = eitri_core::project_trust::TrustStore::new(state_home, home);
    let home = home.map(PathBuf::from).filter(|home| home.is_absolute());
    TrustGate::new(store, root.to_path_buf(), home)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) use crate::panel_page::PageSurface;
    pub(super) use eitri_core::agent_bridge::SessionModeChoice;
    pub(super) use eitri_core::tab_set::{TabBackend, TabSet};
    pub(super) use eitri_core::tabs::TabId;
    pub(super) use eitri_core::test_providers::RecordingProvider;

    /// The panel's own code, read by the scans that hold an invariant over it: every file of `panel/src`.
    /// The panel's logic is spread over the files of this crate, so a scan reads them all; one that read
    /// a single file would pass on whatever was left in it once code moved to another.
    pub(super) fn panel_sources() -> Vec<eitri_core::source_scan::Source> {
        let sources = eitri_core::source_scan::rust_sources(&["panel/src"]);
        eitri_core::source_scan::require_anchors(&sources, &["struct AgentPanelState", "fn handle_inbound_message("]);
        sources
    }

    /// The code of every file of `sources`, comments and literals blanked and test modules left out,
    /// joined into one text a scan reads as a whole.
    pub(super) fn production_code(sources: &[eitri_core::source_scan::Source]) -> String {
        use eitri_core::source_scan as scan;
        sources
            .iter()
            .map(|source| {
                let code = scan::code_only(&source.text);
                scan::without(&code, &scan::modules(&code, "tests")) + "\n"
            })
            .collect()
    }

    #[test]
    fn the_panel_state_takes_its_theme_from_its_parts() {
        use eitri_core::source_scan as scan;
        let sources = panel_sources();
        let code: String = production_code(&sources).split_whitespace().collect();
        let at = code
            .find("implAgentPanelState{")
            .expect("the panel state implementation");
        let body = scan::braced(&code, at + code[at..].find('{').unwrap());
        assert_eq!(body.matches("fnnew(").count(), 1);
        let at = body.find("fnnew(").unwrap();
        let new = scan::braced(body, at + body[at..].find('{').unwrap());
        assert!(!new.contains("ThemeTokens::fallback("));
        let (_, state) = scan::function_in(&sources, "panel_state");
        let state: String = state.split_whitespace().collect();
        let at = state.find("letPanelInit{").expect("the panel inputs are destructured");
        let inputs = scan::braced(&state, at + state[at..].find('{').unwrap());
        assert!(!scan::word_ends(inputs, "theme").is_empty());
    }

    /// Source files for a scan's own test: `(path, text)` pairs.
    pub(super) fn fixture(files: &[(&str, &str)]) -> Vec<eitri_core::source_scan::Source> {
        files
            .iter()
            .map(|(path, text)| eitri_core::source_scan::Source {
                path: (*path).to_owned(),
                text: (*text).to_owned(),
            })
            .collect()
    }

    /// The message `check` fails with, or `None` when it passes.
    pub(super) fn scan_failure(check: impl FnOnce() + std::panic::UnwindSafe) -> Option<String> {
        let failure = std::panic::catch_unwind(check).err()?;
        Some(
            failure
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| failure.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default(),
        )
    }

    /// `production_code` reads every file it is given, not only the first, and leaves out their tests.
    #[test]
    fn the_panels_code_is_read_from_every_file() {
        let read = fixture(&[
            (
                "panel/src/a.rs",
                "fn first() {}\n#[cfg(test)]\nmod tests { fn in_a_test() {} }",
            ),
            ("panel/src/b.rs", "fn second() { let s = \"fn in_a_string() {}\"; }"),
        ]);
        let code = production_code(&read);
        for (name, seen) in [
            ("first", true),
            ("second", true),
            ("in_a_test", false),
            ("in_a_string", false),
        ] {
            assert_eq!(code.contains(&format!("fn {name}(")), seen, "{name}");
        }
        assert!(panel_sources().len() > 10, "the panel's files are read");
    }

    /// The braced body of every `impl` whose header, with the whitespace removed, is `header_flat`.
    pub(super) fn impl_blocks<'a>(code: &'a str, header_flat: &str) -> Vec<&'a str> {
        use eitri_core::source_scan as scan;
        code.match_indices("impl")
            .filter(|(at, _)| {
                !code[..*at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
            .filter_map(|(at, _)| {
                let open = at + code[at..].find('{')?;
                let flat: String = code[at..open].split_whitespace().collect();
                (flat == header_flat).then(|| scan::braced(code, open))
            })
            .collect()
    }

    pub(super) fn live_backend(dir: &std::path::Path) -> (std::sync::Arc<RecordingProvider>, AgentBackend) {
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = agent::AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    /// A page that records what the panel asked of it.
    #[derive(Default)]
    pub(super) struct RecordingPage {
        pub(super) sent: RefCell<Vec<String>>,
        pub(super) batches: RefCell<Vec<Vec<String>>>,
        pub(super) documents: RefCell<Vec<(String, String)>>,
        pub(super) backgrounds: RefCell<Vec<[u8; 3]>>,
        pub(super) visible: Cell<bool>,
    }

    impl PageSurface for RecordingPage {
        fn send(&self, envelope_json: &str) {
            self.sent.borrow_mut().push(envelope_json.to_owned());
        }
        fn send_many(&self, envelopes: &[&str]) {
            self.batches
                .borrow_mut()
                .push(envelopes.iter().map(|s| (*s).to_owned()).collect());
            for envelope in envelopes {
                self.send(envelope);
            }
        }
        fn is_visible(&self) -> bool {
            self.visible.get()
        }
        fn load_document(&self, html: &str, base_uri: &str) {
            self.documents.borrow_mut().push((html.to_owned(), base_uri.to_owned()));
        }
        fn set_background(&self, rgb: [u8; 3]) {
            self.backgrounds.borrow_mut().push(rgb);
        }
    }

    /// A host that records the addresses it was asked to open.
    #[derive(Default)]
    pub(super) struct RecordingHost {
        pub(super) opened: RefCell<Vec<String>>,
    }

    impl PanelHost for RecordingHost {
        fn open_external(&self, url: &str) {
            self.opened.borrow_mut().push(url.to_owned());
        }
    }

    pub(super) fn kind_of(envelope: &str) -> String {
        serde_json::from_str::<serde_json::Value>(envelope).unwrap()["kind"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A small stand-in for the bundle: a `<head>` for the policy and the theme, and one inline script.
    pub(super) static TEST_DOCUMENT: std::sync::LazyLock<PanelDocument> = std::sync::LazyLock::new(|| {
        PanelDocument::new("<!doctype html><html><head></head><body><script type=\"module\">1</script></body></html>")
    });

    /// A handle over a recording page and a recording host. Not built through `AgentPanelHandle::new`,
    /// which would connect to (or start) the supervisor.
    pub(super) fn handle_with_recording_page() -> (AgentPanelHandle, Rc<RecordingPage>, Rc<RecordingHost>) {
        handle_with_recording_page_in_theme(eitri_core::theme::ThemeTokens::fallback())
    }

    pub(super) fn handle_with_recording_page_in_theme(
        theme: eitri_core::theme::ThemeTokens,
    ) -> (AgentPanelHandle, Rc<RecordingPage>, Rc<RecordingHost>) {
        let host = Rc::new(RecordingHost::default());
        let mut state = bare_state(
            &TEST_DOCUMENT,
            host.clone(),
            TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto),
            theme,
        );
        state.spawner = spawn_nothing();
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            Rc::new(RefCell::new(state)),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        (handle, page, host)
    }

    /// A page that panics if a send happens inside another send: the contract every host keeps.
    #[derive(Default)]
    pub(super) struct StrictPage {
        pub(super) inner: RecordingPage,
        pub(super) sending: Cell<bool>,
    }

    impl PageSurface for StrictPage {
        fn send(&self, envelope_json: &str) {
            assert!(!self.sending.replace(true), "a send re-entered the page");
            self.inner.send(envelope_json);
            self.sending.set(false);
        }
        fn is_visible(&self) -> bool {
            self.inner.is_visible()
        }
        fn load_document(&self, html: &str, base_uri: &str) {
            self.inner.load_document(html, base_uri);
        }
        fn set_background(&self, rgb: [u8; 3]) {
            self.inner.set_background(rgb);
        }
    }

    /// `handle_with_recording_page` over a [`StrictPage`].
    pub(super) fn handle_with_strict_page() -> (AgentPanelHandle, Rc<StrictPage>) {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let page = Rc::new(StrictPage::default());
        let handle = AgentPanelHandle::over(
            state,
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        (handle, page)
    }

    /// A ready panel with one empty tab and the page on screen, its first ticks run.
    pub(super) fn settled_panel() -> (AgentPanelHandle, Rc<RecordingPage>) {
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let now = std::time::Instant::now();
        handle.take_wake(now);
        assert_eq!(
            handle.run_tick(now),
            crate::panel_wake::NextTick::OnWake,
            "a settled panel waits"
        );
        (handle, page)
    }

    /// Waits up to 5 s for the wake's descriptor to have been raised, and takes it.
    pub(super) fn wait_for_wake(handle: &AgentPanelHandle) -> std::time::Instant {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(due) = handle.take_wake(std::time::Instant::now()) {
                return due;
            }
            assert!(std::time::Instant::now() < deadline, "the wake never came");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// A minimal `AgentPanelState` for tests that only care about window-level hooks (`tab_verb`)
    /// and never touch the WebView, which lives on `AgentPanelHandle` and not on this struct --
    /// so no display is required to run them.
    pub(super) fn state_for_hooks(tabs: TabSet) -> Rc<RefCell<AgentPanelState>> {
        let mut state = bare_state(
            &TEST_DOCUMENT,
            Rc::new(RecordingHost::default()),
            tabs,
            eitri_core::theme::ThemeTokens::fallback(),
        );
        state.spawner = spawn_nothing();
        Rc::new(RefCell::new(state))
    }

    /// Runs `step` over `state` and delivers what it owes, to no page: for a test that drives one step
    /// and reads what the window was told.
    pub(super) fn stepped<T>(
        state: &Rc<RefCell<AgentPanelState>>,
        step: impl FnOnce(&mut AgentPanelState, &mut Outbox) -> T,
    ) -> T {
        let mut out = Outbox::default();
        let value = step(&mut state.borrow_mut(), &mut out);
        out.deliver(state, &Pages::default());
        value
    }

    /// A spawner whose every start fails at once (its sender is gone), so no test starts a process.
    pub(super) fn spawn_nothing() -> Spawner {
        Spawner::Fake(Rc::new(RefCell::new(|_call: SpawnCall| mpsc::channel().1)))
    }

    /// A scratch directory under the build's target directory, which is where tests of this crate
    /// keep what they write.
    pub(super) fn scratch_under_target(label: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let dir = exe
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("review-flow-tests")
            .join(format!(
                "{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        dir.canonicalize().unwrap()
    }

    /// What another window (a pid no real one here has) sees of the window that holds `dir`.
    pub(super) fn seen_from_another_window(dir: &Path) -> Option<eitri_core::turn_review::Busy> {
        eitri_core::turn_review::busy_elsewhere(dir, 999_999).unwrap()
    }

    /// `cycle_default_mode`'s own reading of a tab's mode field, without going through
    /// `TabSet::cycle_mode` (which would change it): a thin accessor so the test above can assert
    /// on the untouched tab without reaching into a private field from two call sites.
    pub(super) fn set_mode(state: &Rc<RefCell<AgentPanelState>>, tab: TabId) -> SessionModeChoice {
        state.borrow().tabs.get(tab).unwrap().mode()
    }

    pub(super) fn a_command() -> agent::handoff::ClaudeResumeCommand {
        agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2").unwrap()
    }

    pub(super) fn kinds(payloads: &[String]) -> Vec<String> {
        payloads
            .iter()
            .map(|p| {
                serde_json::from_str::<serde_json::Value>(p).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    pub(super) fn restore_panel(
        label: &str,
        saved: &[(&str, Option<&str>, SessionModeChoice)],
        records: &[&str],
    ) -> (Rc<RefCell<AgentPanelState>>, PathBuf) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let conversation_id = agent::conversation_id_for_cwd(&dir);
        for id in records {
            agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
                conversation_id: conversation_id.clone(),
                provider: "claude".to_string(),
                provider_session_id: id.to_string(),
                canonical_cwd: dir.to_string_lossy().into_owned(),
                created_at: "1".to_string(),
                updated_at: "2".to_string(),
                provider_advertised_resume: true,
                title: Some(format!("title of {id}")),
                name: None,
            })
            .unwrap();
        }
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        {
            let mut s = state.borrow_mut();
            s.project_dir = dir.clone();
            s.canonical_project_dir = dir.to_string_lossy().into_owned();
            s.conversation_id = conversation_id.clone();
            // A real gate over the project, its home the directory above it and its record kept
            // beside it, so the restore waits for the check as it does in a window.
            let workspaces = dir.parent().unwrap().to_path_buf();
            let records = workspaces.join(format!("{}.trust", dir.file_name().unwrap().to_string_lossy()));
            s.trust = TrustGate::new(
                eitri_core::project_trust::TrustStore::at(Some(records)),
                dir.clone(),
                Some(workspaces),
            );
            s.restore_source = Some(SavedTabs::from_parts(
                saved
                    .iter()
                    .map(|(id, name, mode)| eitri_core::saved_tabs::SavedTab {
                        agent: AgentId::Claude,
                        conversation_id: conversation_id.clone(),
                        provider_session_id: id.to_string(),
                        name: name.map(str::to_string),
                        mode: *mode,
                        view: None,
                        window: None,
                        turn_running: false,
                    })
                    .collect(),
                0,
                Vec::new(),
                None,
            ));
        }
        (state, dir)
    }

    /// Spawns nothing: records each session it is asked to resume and the lease it was handed, and
    /// keeps the sender so the test decides how that resume ends.
    #[derive(Default)]
    pub(super) struct FakeResumes {
        pub(super) asked: Vec<String>,
        pub(super) leases: Vec<agent::lease::SessionLease>,
        pub(super) trusts: Vec<ProjectTrust>,
        pub(super) senders: Vec<mpsc::Sender<Result<AgentBackend, BackendError>>>,
        pub(super) held_when_first_asked: Option<Vec<bool>>,
        pub(super) all: Vec<String>,
    }

    impl FakeResumes {
        pub(super) fn spawner(
            &mut self,
            canonical: String,
        ) -> impl FnMut(
            &str,
            agent::lease::SessionLease,
            ProjectTrust,
        ) -> mpsc::Receiver<Result<AgentBackend, BackendError>>
               + '_ {
            move |id, lease, project| {
                if self.held_when_first_asked.is_none() {
                    self.held_when_first_asked = Some(
                        self.all
                            .iter()
                            .map(|other| {
                                agent::lease::SessionLease::is_held("claude", &canonical, other).unwrap_or(false)
                            })
                            .collect(),
                    );
                }
                self.asked.push(id.to_string());
                self.leases.push(lease);
                self.trusts.push(project);
                let (tx, rx) = mpsc::channel();
                self.senders.push(tx);
                rx
            }
        }
    }

    pub(super) fn ask(
        state: &Rc<RefCell<AgentPanelState>>,
        resumes: &mut FakeResumes,
        how: RestoreAsk,
    ) -> RestoreResult {
        let result = {
            let mut s = state.borrow_mut();
            let from = s.tabs.active();
            ask_for_restore(&mut s, from, how)
        };
        settle(state, resumes, result)
    }

    pub(super) fn answer(
        state: &Rc<RefCell<AgentPanelState>>,
        resumes: &mut FakeResumes,
        nonce: u64,
        keep: bool,
    ) -> RestoreResult {
        let result = answer_restore_question(&mut state.borrow_mut(), nonce, keep);
        settle(state, resumes, result)
    }

    /// Puts `resumes` in as the panel's spawner for as long as `f` runs, and takes it back after.
    pub(super) fn with_resumes<T>(
        state: &Rc<RefCell<AgentPanelState>>,
        resumes: &mut FakeResumes,
        f: impl FnOnce() -> T,
    ) -> T {
        let canonical = state.borrow().canonical_project_dir.clone();
        let shared = Rc::new(RefCell::new(std::mem::take(resumes)));
        let fake = shared.clone();
        state.borrow_mut().spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| match call {
            SpawnCall::Resume { id, lease, project } => {
                let mut fake = fake.borrow_mut();
                let mut spawn = fake.spawner(canonical.clone());
                spawn(&id, lease, project)
            }
            SpawnCall::Connect { .. } => panic!("a restore resumes; it never connects afresh"),
            SpawnCall::External { .. } => panic!("a restore starts only Claude tabs"),
        })));
        let result = f();
        state.borrow_mut().spawner = spawn_nothing();
        *resumes = Rc::try_unwrap(shared)
            .ok()
            .expect("the spawner was put back")
            .into_inner();
        result
    }

    /// The gate's tick, run until `done` says so (at most two seconds), with every step's output.
    pub(super) fn tick_until(state: &Rc<RefCell<AgentPanelState>>, done: impl Fn(&[Outbox]) -> bool) -> Vec<Outbox> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut seen = Vec::new();
        loop {
            seen.push(trust_events(&mut state.borrow_mut()));
            if done(&seen) {
                return seen;
            }
            assert!(std::time::Instant::now() < deadline, "the gate never got there");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A restore the gate holds is let through once its check comes back (these roots have nothing to
    /// trust), with `resumes` as the spawner; what `start_restore` said is the result.
    pub(super) fn settle(
        state: &Rc<RefCell<AgentPanelState>>,
        resumes: &mut FakeResumes,
        result: RestoreResult,
    ) -> RestoreResult {
        if !matches!(result, RestoreResult::Deferred) {
            return result;
        }
        let seen = with_resumes(state, resumes, || {
            tick_until(state, |seen| seen.iter().any(|o| o.restored.is_some()))
        });
        let notice = seen.into_iter().find_map(|o| o.restored).expect("restored");
        RestoreResult::Started { notice }
    }

    pub(super) fn started(result: RestoreResult) -> Option<String> {
        match result {
            RestoreResult::Started { notice } => notice,
            RestoreResult::Refused(why) => panic!("refused: {why}"),
            RestoreResult::Question(prompt) => panic!("asked: {:?}", prompt.lines),
            RestoreResult::Deferred => panic!("still waiting for the trust question"),
        }
    }

    pub(super) fn mode(state: &Rc<RefCell<AgentPanelState>>, session: &str) -> SessionModeChoice {
        let s = state.borrow();
        let tab = s
            .tabs
            .tabs()
            .iter()
            .find(|t| t.provider_session_id().as_deref() == Some(session))
            .map(|t| t.id)
            .unwrap();
        s.tabs.get(tab).unwrap().mode()
    }

    pub(super) use SessionModeChoice::{Auto, Bypass};

    pub(super) fn handle_for(state: &Rc<RefCell<AgentPanelState>>) -> AgentPanelHandle {
        AgentPanelHandle::over(state.clone(), None)
    }

    /// A project with its own home and state home, all under one scratch directory the test owns.
    pub(super) struct TrustFx {
        pub(super) outer: PathBuf,
        pub(super) state: PathBuf,
        pub(super) root: PathBuf,
        /// Every start the panel's spawner was asked for: the session it resumes, if any, and the
        /// tiers it was given.
        pub(super) spawned: Rc<RefCell<Vec<Spawned>>>,
        /// Every start of another agent's tab: the agent, its trust plan, its mode and the session
        /// it resumes.
        pub(super) external: Rc<RefCell<Vec<ExternalSpawned>>>,
    }

    /// One start of another agent's tab, as the counting spawner saw it.
    #[derive(Debug, Clone)]
    pub(super) struct ExternalSpawned {
        pub(super) agent: AgentId,
        pub(super) plan: agent::TrustPlan,
        pub(super) mode: SessionModeChoice,
        pub(super) resume: Option<String>,
        /// The command and mode a registered agent's start was given.
        pub(super) acp: Option<(Vec<std::ffi::OsString>, Option<String>)>,
        /// Whether the start waits for the tab's old backend to be gone (`r` continuing its session).
        pub(super) waits_for_old: bool,
    }

    /// One start a counting spawner was asked for: the session it resumes, if any, and its tiers.
    pub(super) type Spawned = (Option<String>, ProjectTrust);

    impl TrustFx {
        pub(super) fn write(&self, relative: &str, bytes: &[u8]) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        pub(super) fn record_file(&self) -> PathBuf {
            self.state
                .join("eitri/trust")
                .join(format!("{}.json", &agent::conversation_id_for_cwd(&self.root)[..16]))
        }
    }

    impl Drop for TrustFx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.outer);
        }
    }

    pub(super) const PROJECT_SETTINGS: &str = ".claude/settings.json";

    pub(super) fn session_start_hook(command: &str) -> Vec<u8> {
        format!(r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#)
            .into_bytes()
    }

    /// A panel over a fresh project below a fake home, with a real gate and a spawner that only
    /// counts. The page is ready, so a question can be shown.
    pub(super) fn trust_panel(label: &str) -> (Rc<RefCell<AgentPanelState>>, TrustFx) {
        let outer = agent::state_dirs::test_workspace_dir(label);
        let home = outer.join("home");
        let state_home = outer.join("state");
        let root = home.join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state_home).unwrap();
        let spawned = Rc::new(RefCell::new(Vec::new()));
        let external = Rc::new(RefCell::new(Vec::new()));
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        {
            let mut s = state.borrow_mut();
            s.project_dir = root.clone();
            s.canonical_project_dir = root.to_string_lossy().into_owned();
            s.page_mut(ViewId::BASE).document_ready = true;
            s.trust = trust_gate_for(Some(state_home.as_os_str()), Some(home.as_os_str()), &root);
            let counted = spawned.clone();
            let counted_external = external.clone();
            s.spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
                match call {
                    SpawnCall::Connect { resume, project } => counted.borrow_mut().push((resume, project)),
                    SpawnCall::Resume { id, project, .. } => counted.borrow_mut().push((Some(id), project)),
                    SpawnCall::External {
                        agent,
                        plan,
                        mode,
                        resume,
                        acp,
                        after_old,
                    } => counted_external.borrow_mut().push(ExternalSpawned {
                        agent,
                        plan,
                        mode,
                        resume,
                        acp: acp.map(|spec| (spec.argv, spec.auto_mode)),
                        waits_for_old: after_old.is_some(),
                    }),
                }
                mpsc::channel().1
            })));
        }
        let fx = TrustFx {
            outer,
            state: state_home,
            root,
            spawned,
            external,
        };
        (state, fx)
    }

    /// One trust prompt as the page received it.
    #[derive(Debug, Clone)]
    pub(super) struct Prompt {
        pub(super) tab: TabId,
        pub(super) nonce: u64,
        pub(super) fingerprint: String,
        pub(super) digest: String,
        pub(super) remember: String,
        pub(super) state: String,
    }

    pub(super) fn json_of(payload: &str) -> serde_json::Value {
        serde_json::from_str(payload).unwrap()
    }

    pub(super) fn prompts(payloads: &[String]) -> Vec<Prompt> {
        payloads
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "trust_prompt")
            .map(|v| Prompt {
                tab: TabId(v["tab"].as_u64().unwrap()),
                nonce: v["nonce"].as_u64().unwrap(),
                fingerprint: v["fingerprint"].as_str().unwrap().to_string(),
                digest: v["findingsDigest"].as_str().unwrap().to_string(),
                remember: v["remember"].as_str().unwrap().to_string(),
                state: v["state"].as_str().unwrap().to_string(),
            })
            .collect()
    }

    pub(super) fn all_payloads(seen: &[Outbox]) -> Vec<String> {
        seen.iter().flat_map(|o| o.payloads()).collect()
    }

    /// The last prompt the tick sent, waiting for one.
    pub(super) fn next_prompt(state: &Rc<RefCell<AgentPanelState>>) -> Prompt {
        let seen = tick_until(state, |seen| !prompts(&all_payloads(seen)).is_empty());
        prompts(&all_payloads(&seen)).pop().unwrap()
    }

    /// The `command_result` for `request_id` among `payloads`: `Ok` or the error text.
    pub(super) fn result_for(payloads: &[String], request_id: &str) -> Option<Result<(), String>> {
        payloads.iter().map(|p| json_of(p)).find_map(|v| {
            (v["kind"] == "command_result" && v["requestId"] == request_id).then(|| {
                if v["ok"] == true {
                    Ok(())
                } else {
                    Err(v["error"].as_str().unwrap_or("").to_string())
                }
            })
        })
    }

    pub(super) fn notices(payloads: &[String]) -> Vec<String> {
        payloads
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "notice")
            .map(|v| v["text"].as_str().unwrap().to_string())
            .collect()
    }

    pub(super) fn first_send(request_id: &str, text: &str) -> DeferredStart {
        DeferredStart::FirstSend {
            request_id: request_id.to_string(),
            first_turn: FirstTurn {
                typed_by_person: true,
                wire: format!("wire: {text}"),
                typed: text.to_string(),
            },
        }
    }

    pub(super) fn answer_prompt(
        state: &Rc<RefCell<AgentPanelState>>,
        request_id: &str,
        p: &Prompt,
        trust: bool,
    ) -> Outbox {
        answer_trust(
            &mut state.borrow_mut(),
            request_id,
            p.tab,
            p.nonce,
            &p.fingerprint,
            &p.digest,
            trust,
            ViewId::BASE,
        )
    }

    /// A registered agent as `init.lua` would give it: a command, two project files and a mode.
    pub(super) fn gem_registration() -> AgentRegistration {
        AgentRegistration {
            name: "gem".into(),
            command: vec!["gem-acp".into(), "--stdio".into()],
            project_files: vec![".gem/".into(), "GEM.md".into()],
            auto_mode: Some("auto_edit".into()),
            read_only_mode: None,
        }
    }

    /// Runs the tick on its own cadence until it says it waits for the wake (at most ten seconds of real
    /// time: workers answer meanwhile); the moment of the last tick.
    pub(super) fn tick_until_quiet(handle: &AgentPanelHandle, mut at: std::time::Instant) -> std::time::Instant {
        use crate::panel_wake::NextTick;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match handle.run_tick(at) {
                NextTick::OnWake => return at,
                NextTick::At(next) => at = next,
                NextTick::Stop => panic!("the tick stopped"),
            }
            assert!(std::time::Instant::now() < deadline, "the tick never went quiet");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    pub(super) const READY: &str = r#"{"type":"ready","request_id":"r-ready"}"#;

    pub(super) fn view(n: u8) -> ViewId {
        ViewId::new(n).unwrap()
    }

    /// A recording page attached to `handle` as `view`'s, its document not loaded.
    pub(super) fn attach_recording_page(handle: &AgentPanelHandle, view: ViewId) -> Rc<RecordingPage> {
        let page = Rc::new(RecordingPage::default());
        handle.attach_page(view, Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>)));
        page
    }

    /// [`handle_with_recording_page`] with a second recording page attached as view 2, both past `ready`.
    pub(super) fn handle_with_two_pages() -> (AgentPanelHandle, Rc<RecordingPage>, Rc<RecordingPage>) {
        let (handle, base, _host) = handle_with_recording_page();
        let second = attach_recording_page(&handle, view(2));
        handle.page_message(ViewId::BASE, READY);
        handle.page_message(view(2), READY);
        (handle, base, second)
    }

    /// Whether `page` was sent an envelope of `kind` about `tab`.
    pub(super) fn sent_about(page: &RecordingPage, kind: &str, tab: TabId) -> bool {
        page.sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .any(|v| v["kind"] == kind && v["tab"] == tab.0)
    }

    /// A new tab fed by a provider of its own, opened (so selected) in the base view.
    pub(super) fn live_tab(handle: &AgentPanelHandle, dir: &Path) -> (TabId, std::sync::Arc<RecordingProvider>) {
        let (provider, backend) = live_backend(dir);
        let mut state = handle.state.borrow_mut();
        let tab = state.tabs.open(AgentId::Claude);
        state.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        (tab, provider)
    }

    /// Ticks until `done` says so, at most five seconds.
    pub(super) fn tick_handle_until(handle: &AgentPanelHandle, what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub(super) fn stream(provider: &RecordingProvider, turn: &str, text: &str) {
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        provider.queue(agent::AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: agent::ContentKind::Text,
            text: text.into(),
        });
    }

    /// A `Write` that needs a human, so the pump leaves it a card.
    pub(super) fn write_card(permission_id: &str) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::PermissionRequested {
            permission_id: permission_id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
            provider_prompt: None,
        }
    }

    pub(super) fn clear(pages: &[&Rc<RecordingPage>]) {
        for page in pages {
            page.sent.borrow_mut().clear();
        }
    }

    /// A gate that records the view of every answer it is asked about and refuses those of `refused`
    /// views while `covered` is set.
    pub(super) fn recording_gate(
        handle: &AgentPanelHandle,
        refused: &'static [u8],
        covered: Rc<Cell<bool>>,
    ) -> Rc<RefCell<Vec<ViewId>>> {
        const COVERED: &str = "agent 2 is under terminal 2: C-b M-- from terminal 2 stows it";
        let asked = Rc::new(RefCell::new(Vec::new()));
        handle.set_answer_gate(Rc::new({
            let asked = asked.clone();
            move |view: ViewId| {
                asked.borrow_mut().push(view);
                if covered.get() && refused.contains(&view.number()) {
                    Err(COVERED.to_string())
                } else {
                    Ok(())
                }
            }
        }));
        asked
    }

    pub(super) fn permission_response(
        handle: &AgentPanelHandle,
        from: ViewId,
        request: &str,
        tab: TabId,
        permission: &str,
    ) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"permission_response","request_id":"{request}","tab":{},"permission_id":"{permission}","decision":"allow"}}"#,
                tab.0
            ),
        );
    }

    pub(super) fn waiting(handle: &AgentPanelHandle, tab: TabId, permission: &str) -> bool {
        handle.state.borrow().tabs.get(tab).unwrap().card_is_waiting(permission)
    }

    pub(super) fn pending_cards(handle: &AgentPanelHandle, tab: TabId) -> usize {
        handle
            .state
            .borrow()
            .tabs
            .get(tab)
            .unwrap()
            .attention
            .attention()
            .pending
    }

    pub(super) fn shut_down_every_tab(handle: &AgentPanelHandle) {
        for t in handle.state.borrow_mut().tabs.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// The last `confirm_bypass` prompt `page` was sent: its scope's tab and its nonce.
    pub(super) fn last_bypass_prompt(page: &RecordingPage) -> Option<(Option<u64>, u64)> {
        page.sent
            .borrow()
            .iter()
            .rev()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "confirm_bypass")
            .map(|v| (v["tab"].as_u64(), v["nonce"].as_u64().unwrap()))
    }

    pub(super) fn confirm_bypass_from(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId, nonce: u64) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"confirm_bypass","request_id":"{request}","tab":{},"scope":"tab","nonce":{nonce}}}"#,
                tab.0
            ),
        );
    }

    pub(super) fn cycle_mode_from(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId) {
        handle.page_message(
            from,
            &format!(r#"{{"type":"cycle_mode","request_id":"{request}","tab":{}}}"#, tab.0),
        );
    }

    pub(super) fn mode_of(handle: &AgentPanelHandle, tab: TabId) -> SessionModeChoice {
        handle.state.borrow().tabs.get(tab).unwrap().mode()
    }

    /// An untrusted project ([`trust_panel`] with a project hook) behind a handle with two recording
    /// pages past `ready` and a new empty tab bound to view 2. The gate's worker waits on `hold`
    /// before each job; it starts open.
    pub(super) struct TrustViews {
        pub(super) handle: AgentPanelHandle,
        pub(super) base: Rc<RecordingPage>,
        pub(super) second: Rc<RecordingPage>,
        pub(super) tab: TabId,
        pub(super) fx: TrustFx,
        pub(super) hold: crate::trust_gate::WorkerHold,
    }

    pub(super) fn trust_views(label: &str) -> TrustViews {
        let (state, fx) = trust_panel(label);
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let hold = crate::trust_gate::WorkerHold::default();
        {
            let mut s = state.borrow_mut();
            let home = fx.root.parent().unwrap().to_path_buf();
            let store = eitri_core::project_trust::TrustStore::new(Some(fx.state.as_os_str()), Some(home.as_os_str()));
            s.trust = TrustGate::new_held(store, fx.root.clone(), Some(home), hold.clone());
        }
        let base = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state,
            Some(Rc::new(PanelPage::new(base.clone() as Rc<dyn PageSurface>))),
        );
        let second = attach_recording_page(&handle, view(2));
        handle.page_message(ViewId::BASE, READY);
        handle.page_message(view(2), READY);
        let tab = handle.open_in(view(2)).unwrap();
        TrustViews {
            handle,
            base,
            second,
            tab,
            fx,
            hold,
        }
    }

    /// The gate's tick, delivering to the pages, until `done` says so (at most five seconds).
    pub(super) fn poll_trust_until(handle: &AgentPanelHandle, what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            handle.run_seen(poll_trust);
            if done() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The gate's tick until it owes nothing.
    pub(super) fn poll_trust_until_idle(handle: &AgentPanelHandle) {
        poll_trust_until(handle, "the gate to go idle", || !handle.state.borrow().trust.is_busy());
    }

    pub(super) fn trust_answer_from(handle: &AgentPanelHandle, from: ViewId, request: &str, p: &Prompt, trust: bool) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"trust_answer","request_id":"{request}","tab":{},"nonce":{},"fingerprint":"{}","findings_digest":"{}","trust":{trust}}}"#,
                p.tab.0, p.nonce, p.fingerprint, p.digest
            ),
        );
    }

    pub(super) fn awaiting_trust(handle: &AgentPanelHandle, tab: TabId) -> bool {
        matches!(
            handle.state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::AwaitingTrust(AwaitingTrust { recording: false, .. })
        )
    }

    pub(super) fn trust_command_from(handle: &AgentPanelHandle, from: ViewId, request: &str, action: &str) {
        handle.page_message(
            from,
            &format!(r#"{{"type":"trust_command","request_id":"{request}","action":"{action}","agent":"claude"}}"#),
        );
    }

    /// A window holds its presence from the moment its state is made, on every backend and with the
    /// review off: the legacy backend installs no review at all, and `review.enabled = false`
    /// installs none either, yet another window's revert must still see both.
    #[test]
    fn presence_is_held_from_window_start_on_every_backend() {
        use eitri_core::turn_review::{review_dir_for, Busy, TurnReview};
        let scratch = scratch_under_target("presence-every-backend");
        let state_home = scratch.join("state");
        let pid = std::process::id();
        let wait_for_window = |dir: &Path| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while seen_from_another_window(dir) != Some(Busy::OtherWindow { pid }) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the window never showed in {dir:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };

        let legacy_root = scratch.join("project");
        let mut legacy = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
        let refused = TurnReview::with_options(None, &legacy_root, Default::default());
        assert!(
            legacy.install_turn_review(refused).is_err(),
            "the legacy backend installs no review"
        );
        hold_window_presence(&mut legacy, Some(state_home.as_os_str()), None, &legacy_root, pid);
        assert!(legacy.presence_guard().is_some());
        wait_for_window(&review_dir_for(Some(state_home.as_os_str()), None, &legacy_root).unwrap());

        let off_root = scratch.join("off-project");
        std::fs::create_dir_all(&off_root).unwrap();
        let mut off = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        off.turn_review_off();
        hold_window_presence(&mut off, Some(state_home.as_os_str()), None, &off_root, pid);
        assert!(off.turn_review().is_none());
        wait_for_window(&review_dir_for(Some(state_home.as_os_str()), None, &off_root).unwrap());

        // With no state directory there is nothing to hold, and a revert is refused for it.
        let mut nowhere = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        hold_window_presence(&mut nowhere, None, None, &legacy_root, pid);
        assert!(nowhere.presence_guard().is_none());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// `agent.default_mode`: applied to the window's default and its empty tab, unless the project
    /// remembers a mode Shift+Tab left; naming bypass is also what lets saved bypass tabs come back
    /// unasked; naming nothing changes nothing.
    #[test]
    fn the_configured_default_mode_applies_unless_the_project_remembers_one() {
        let (state, _dir) = restore_panel("default-mode", &[], &[]);
        let handle = handle_for(&state);
        handle.set_default_mode(None);
        assert_eq!(state.borrow().tabs.default_mode(), Auto);
        assert!(!state.borrow().bypass_by_config);

        handle.set_default_mode(Some(Bypass));
        assert_eq!(state.borrow().tabs.default_mode(), Bypass);
        let tab = state.borrow().tabs.active();
        assert_eq!(
            state.borrow().tabs.get(tab).unwrap().mode(),
            Bypass,
            "the empty tab it already has"
        );
        assert!(state.borrow().bypass_by_config);

        // A project that remembers a mode: the setting does not override it.
        let (remembered, _dir) = restore_panel("default-mode-remembered", &[], &[]);
        remembered.borrow_mut().mode_remembered = true;
        handle_for(&remembered).set_default_mode(Some(Bypass));
        assert_eq!(remembered.borrow().tabs.default_mode(), Auto);
        let tab = remembered.borrow().tabs.active();
        assert_eq!(remembered.borrow().tabs.get(tab).unwrap().mode(), Auto);
    }

    // ---- the trust gate on every session path ----------------------------------------------------

    /// The panel's state is built in one place, `AgentPanelState::new`: a field added to it is given its
    /// start there once, not once per copy of the literal. Every other `AgentPanelState {` in the panel's
    /// code is its definition, an `impl`, a return type or a destructure.
    fn the_state_is_built_only_by_its_constructor(sources: &[eitri_core::source_scan::Source]) {
        use eitri_core::source_scan as scan;
        let code = production_code(sources);
        let built: Vec<usize> = scan::word_ends(&code, "AgentPanelState")
            .into_iter()
            .filter(|end| code[*end..].trim_start().starts_with('{'))
            .filter(|end| {
                let before = code[..end - "AgentPanelState".len()].trim_end();
                !["struct", "impl", "for", "let", "->"]
                    .iter()
                    .any(|word| before.ends_with(word))
            })
            .collect();
        let constructors: Vec<&str> = impl_blocks(&code, "implAgentPanelState")
            .into_iter()
            .flat_map(|block| scan::functions(block, "new"))
            .collect();
        assert_eq!(constructors.len(), 1, "one `AgentPanelState::new`");
        let inside = constructors[0].as_ptr() as usize - code.as_ptr() as usize;
        let inside = inside..inside + constructors[0].len();
        assert!(
            built.iter().all(|at| inside.contains(at)),
            "the panel's state is built outside `AgentPanelState::new`"
        );
        assert_eq!(built.len(), 1, "`AgentPanelState::new` builds the state once");
    }

    #[test]
    fn the_panels_state_has_one_constructor() {
        the_state_is_built_only_by_its_constructor(&panel_sources());
    }

    #[test]
    fn a_state_built_by_hand_in_a_second_file_fails_the_scan() {
        let constructor = "impl AgentPanelState {\n    fn new(parts: StateParts) -> Self {\n        AgentPanelState { tabs }\n    }\n}\n\
                           fn read(state: &AgentPanelState) { let AgentPanelState { tabs, .. } = state; }\n";
        let clean = fixture(&[("panel/src/a.rs", constructor), ("panel/src/b.rs", "fn other() {}")]);
        assert_eq!(
            scan_failure(|| the_state_is_built_only_by_its_constructor(&clean)),
            None
        );
        let read = fixture(&[
            ("panel/src/a.rs", constructor),
            (
                "panel/src/b.rs",
                "fn bare() -> AgentPanelState { AgentPanelState { tabs } }",
            ),
        ]);
        let why = scan_failure(|| the_state_is_built_only_by_its_constructor(&read)).expect("the second file is read");
        assert!(why.contains("built outside"), "{why}");
    }

    /// The companion window builds its panel with `build_agent_panel`, which builds the state with
    /// `panel_state`, which puts in the gate over the window's own canonical root; and that gate is
    /// about the root it was given.
    #[test]
    fn companion_mode_builds_the_same_gate() {
        use eitri_core::source_scan as scan;
        const SCANNED: &[&str] = &["shell/src", "panel/src"];
        let companion = scan::code_only(&scan::read_required("shell/src/companion/window.rs").text);
        let flat: String = companion.split_whitespace().collect();
        assert!(flat.contains("crate::agent_panel::build_agent_panel("));
        let read = scan::rust_sources(SCANNED);
        let panel = production_code(&panel_sources());
        let builder = scan::code_only(&scan::file_with(&read, "fn build_agent_panel(").text);
        let build = scan::functions(&builder, "build_agent_panel");
        assert_eq!(build.len(), 1);
        // The builder goes through `AgentPanelHandle::new` (once for the page, once for a window that shows
        // no chat, once for none), and that is the one place the state is built.
        let flat_build: String = build[0].split_whitespace().collect();
        assert_eq!(flat_build.matches("AgentPanelHandle::new(").count(), 3);
        assert_eq!(scan::calls(build[0], "panel_state"), 0);
        let handle_impls = impl_blocks(&panel, "implAgentPanelHandle");
        let new: Vec<&str> = handle_impls
            .iter()
            .flat_map(|block| scan::functions(block, "new"))
            .collect();
        assert_eq!(new.len(), 1);
        assert!(scan::calls(new[0], "panel_state") > 0);
        let state = scan::functions(&panel, "panel_state");
        assert_eq!(state.len(), 1);
        let flat: String = state[0].split_whitespace().collect();
        assert!(
            flat.contains("trust:trust") || flat.contains("trust,"),
            "the state holds the gate"
        );
        assert!(flat.contains("trust_gate_for(state_home.as_deref(),home.as_deref(),&canonical_root)"));

        let outer = agent::state_dirs::test_workspace_dir("trust-companion");
        let root = outer.join("home/p");
        std::fs::create_dir_all(&root).unwrap();
        let gate = trust_gate_for(
            Some(outer.join("state").as_os_str()),
            Some(outer.join("home").as_os_str()),
            &root,
        );
        assert_eq!(gate.root(), root.as_path());
        let _ = std::fs::remove_dir_all(&outer);
    }

    // ---- Chat views: one page per view, each told only of the tab it shows. ----

    /// A window hook, the coverage probe and the answer gate are read out of the state only where the
    /// outbox is delivered and the window is asked before a step: a step that took one out itself could
    /// call it while the state is borrowed, and a hook that calls back into the panel would find it so.
    #[test]
    fn window_hooks_are_read_only_by_the_outbox() {
        use eitri_core::source_scan as scan;
        let sources = scan::rust_sources(&["panel/src"]);
        scan::require_anchors(&sources, &["struct AgentPanelState", "fn deliver(", "fn ask_gate("]);
        let mut readers: Vec<&str> = Vec::new();
        for source in &sources {
            let code = scan::code_only(&source.text);
            let code = scan::without(&code, &scan::modules(&code, "tests"));
            let flat: String = code.split_whitespace().collect();
            if [
                "_hook.clone()",
                ".answer_gate.clone()",
                ".coverage.clone()",
                ".opened_hook()",
            ]
            .iter()
            .any(|read| flat.contains(read))
            {
                readers.push(&source.path);
            }
        }
        assert_eq!(readers.len(), 1, "{readers:?}");
        assert!(readers[0].ends_with("outbox.rs"), "{readers:?}");
    }
}
