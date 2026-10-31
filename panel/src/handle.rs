//! `AgentPanelHandle`, the host's handle on the panel, and every method a host calls on it.
//!
//! A method that changes the state runs one step under one borrow of it (`run`, or `run_seen` when the step
//! needs the screen) and delivers the step's outbox once that borrow has ended.

use super::*;

#[cfg(any(test, feature = "test-support"))]
pub(super) static HOST_TEST_DOCUMENT: std::sync::LazyLock<PanelDocument> = std::sync::LazyLock::new(|| {
    PanelDocument::new("<!doctype html><html><head></head><body><script type=\"module\">1</script></body></html>")
});

/// A host that opens nothing, for [`AgentPanelHandle::for_host_test`].
#[cfg(any(test, feature = "test-support"))]
pub(super) struct NoHost;

#[cfg(any(test, feature = "test-support"))]
impl PanelHost for NoHost {
    fn open_external(&self, _url: &str) {}
}

#[cfg(any(test, feature = "test-support"))]
impl AgentPanelHandle {
    /// A panel over `page` with one empty tab and no supervisor, disk, trust worker or review: what a
    /// host's own tests drive, to see how its loop runs the tick.
    pub fn for_host_test(page: Rc<dyn crate::panel_page::PageSurface>) -> Self {
        let tabs =
            eitri_core::tab_set::TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let state = bare_state(
            &HOST_TEST_DOCUMENT,
            Rc::new(NoHost),
            tabs,
            eitri_core::theme::ThemeTokens::fallback(),
        );
        AgentPanelHandle::over(Rc::new(RefCell::new(state)), Some(Rc::new(PanelPage::new(page))))
    }

    /// [`for_host_test`](Self::for_host_test) with no page at all, as a hub's or a page window's panel.
    pub fn for_host_test_without_pages() -> Self {
        let tabs =
            eitri_core::tab_set::TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let state = bare_state(
            &HOST_TEST_DOCUMENT,
            Rc::new(NoHost),
            tabs,
            eitri_core::theme::ThemeTokens::fallback(),
        );
        AgentPanelHandle::over(Rc::new(RefCell::new(state)), None)
    }

    /// [`for_host_test`](Self::for_host_test) with its tabs kept in `files` for `project`: one tab that keeps
    /// the session `session` (a resume that failed, so the close has nothing to tear down and owes only the
    /// write), and `before_write` run on the writer thread before each write. A host's test of its close holds
    /// the last write with it and sees the close wait for it.
    pub fn for_host_test_with_saved_tabs(
        page: Rc<dyn crate::panel_page::PageSurface>,
        files: PathBuf,
        project: PathBuf,
        session: &str,
        before_write: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let handle = Self::for_host_test(page);
        {
            let mut state = handle.state.borrow_mut();
            let state = &mut *state;
            state.canonical_project_dir = project.to_string_lossy().into_owned();
            state.conversation_id = agent::conversation_id_for_cwd(&project);
            let mut writer = TabWriter::new(Some(files), project.clone());
            writer.set_before_write(before_write);
            state.tab_memory = writer;
            state.project_dir = project;
            let tab = state.tabs.active();
            let tab = state.tabs.get_mut(tab).expect("the active tab");
            tab.failed_resume = Some(session.to_owned());
            tab.set_backend(TabBackend::Failed {
                reason: "stopped".to_owned(),
            });
        }
        handle
    }

    /// Hands the panel a supervisor connection over `stream`, as a host's test plays the supervisor.
    pub fn connect_test_supervisor(&self, stream: std::os::unix::net::UnixStream) {
        self.state.borrow_mut().supervisor = crate::supervisor_client::SupervisorClient::over_stream(stream);
    }

    /// How many ticks have run.
    pub fn ticks_run(&self) -> u64 {
        self.state.borrow().ticks_run
    }
}

/// A handle back into this panel's session state, held by `main.rs` alongside the `gtk4::Widget`
/// so a real window close can shut down whatever `AgentSession` exists -- without this, the
/// `Rc<RefCell<AgentPanelState>>` that owns the session is only ever reachable from closures
/// internal to this module (the script-message handler and the pump timer), neither of which
/// ever runs `AgentSession::shutdown()` on its own, so a normal window close would otherwise
/// leak the child process, its hook socket, and any in-memory-only settings backup.
#[derive(Clone)]
pub struct AgentPanelHandle {
    pub(super) state: Rc<RefCell<AgentPanelState>>,
    /// Every chat page, by the view it shows, held so a document can be re-injected without going
    /// back through the widget tree. Each page and the pacer in front of it are one value, shared
    /// with the pump and the page's own message handler. Pages are attached after the handle is
    /// built ([`AgentPanelHandle::attach_page`]).
    pub(super) pages: Pages,
    /// `false` when WebKit's sandbox cannot start in this process (`webkit_sandbox::Decision::
    /// Unavailable`): no `WebView` is built, the
    /// panel's place is a GTK notice, and nothing ever starts a session -- sessions start from the
    /// page's own messages, and there is no page. Every method below still runs against the inert
    /// state `main.rs`'s focus, HINT, prefix and tab-verb paths call into; what would reach a page
    /// does nothing, and the tab verbs refuse (`closing`), so `main.rs` flashes rather than opening
    /// tabs nobody can see. Off in a window whose page is not built yet, or never will be: on from
    /// [`open_pages`](Self::open_pages), for every copy of the handle.
    pub(super) with_pages: Rc<std::cell::Cell<bool>>,
    /// A window with no page whose editor serves other windows' turn reviews: its tick does that and nothing else.
    drives_review: Rc<std::cell::Cell<bool>>,
    /// The wake's raise, held outside the state so raising it never needs the state's cell.
    pub(super) raise: Option<agent::UiWake>,
    partner: Rc<RefCell<PartnerChooser>>,
    arrived_moves: Rc<RefCell<BTreeMap<u64, TabId>>>,
}

/// The linked process, identified across process-id reuse, and the number its rows show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partner {
    pub pid: u32,
    pub start: u64,
    pub window: u8,
}

impl From<(u32, u64, u8)> for Partner {
    fn from((pid, start, window): (u32, u64, u8)) -> Self {
        Self { pid, start, window }
    }
}

type PartnerPick = Rc<dyn Fn(ViewId, String)>;

#[derive(Default)]
struct PartnerChooser {
    partner: Option<Partner>,
    generation: u64,
    sessions: Vec<String>,
    pick: Option<PartnerPick>,
    pending: Option<std::sync::mpsc::Receiver<(u64, ViewId, String, Vec<String>)>>,
    pending_tab: Option<TabId>,
}

/// Where the window's link to `eitri-supervisor` stands ([`AgentPanelHandle::supervisor_link`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorLink {
    /// The supervisor is being started and connected to, off the window's thread: ask again later.
    Connecting,
    /// Connected: an activation arrives on this descriptor, which stays open while it is reported.
    Socket(std::os::fd::RawFd),
    /// None now and none to come (none running, its connection gone, or the window closing).
    Absent,
}

/// What `AgentPanelHandle::new` builds the panel's state from.
pub struct PanelInit {
    pub project_dir: PathBuf,
    pub editor_context: eitri_core::editor_context::ContextSource,
    pub scratch: Option<eitri_core::scratch::ScratchDir>,
    pub backend_kind: BackendKind,
    pub document: &'static PanelDocument,
    pub theme: eitri_core::theme::ThemeTokens,
    /// Whose saved-tabs file in the project's pair the panel keeps its tabs in. Which saved tabs it offers back
    /// is given once `init.lua` has run ([`AgentPanelHandle::set_tab_files`]).
    pub tab_role: eitri_core::window_link::pair::Role,
    /// Why the project folder is no boundary (`agent::project_boundary::note`), when it is not one. Told to
    /// every page right after its `hello`, so a reloaded page says it again.
    pub root_note: Option<String>,
}

/// Connects to (or starts) the supervisor for a window on `project_dir`; a supervisor being started comes back as the
/// receiver the pump drains.
#[allow(clippy::type_complexity)]
fn connect_supervisor(
    project_dir: &std::path::Path,
    backend_kind: BackendKind,
) -> (
    Option<crate::supervisor_client::SupervisorClient>,
    Option<std::sync::mpsc::Receiver<Option<crate::supervisor_client::SupervisorClient>>>,
) {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| project_dir.to_string_lossy().to_string());
    let connected =
        match crate::supervisor_client::SupervisorClient::connect_or_spawn(instance_id, project_name, project_dir) {
            crate::supervisor_client::PendingSupervisor::Ready(client) => (client, None),
            crate::supervisor_client::PendingSupervisor::Connecting(rx) => (None, Some(rx)),
        };
    println!("[agent_panel] backend: {}", backend_kind.as_str());
    connected
}

impl AgentPanelHandle {
    /// A handle over `state`, holding the state's wake, with `page` attached as the base view's when
    /// there is one; with none, the handle has no pages at all.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn over(state: Rc<RefCell<AgentPanelState>>, page: Option<Rc<PanelPage>>) -> Self {
        let raise = state.borrow().wake.as_ref().map(crate::panel_wake::PanelWake::waker);
        let handle = AgentPanelHandle {
            state,
            pages: Pages::default(),
            with_pages: Rc::new(std::cell::Cell::new(page.is_some())),
            drives_review: Rc::default(),
            raise,
            partner: Rc::default(),
            arrived_moves: Rc::default(),
        };
        if let Some(page) = page {
            handle.attach_page(ViewId::BASE, page);
        }
        handle
    }

    /// Runs `step` over the state under one borrow, then delivers what it owes with that borrow ended
    /// (`Outbox::deliver`); what the step returns.
    pub(super) fn run<T>(&self, step: impl FnOnce(&mut AgentPanelState, &mut Outbox) -> T) -> T {
        let mut out = Outbox::default();
        let value = step(&mut self.state.borrow_mut(), &mut out);
        out.deliver(&self.state, &self.pages);
        value
    }

    /// [`run`](Self::run) for a step that needs to know which pages are on screen, asked of the pages
    /// and the window before the step, with the state released.
    pub(super) fn run_seen<T>(&self, step: impl FnOnce(&mut AgentPanelState, &Screen, &mut Outbox) -> T) -> T {
        let screen = Screen::read(&self.state, &self.pages);
        self.run(|state, out| step(state, &screen, out))
    }

    /// With pages (`with_pages`): connects to (or starts) the supervisor and builds the state; the pages
    /// are attached afterwards ([`attach_page`](Self::attach_page)). Without: the state alone, no pump and no
    /// supervisor client, and every page verb does nothing until [`open_pages`](Self::open_pages), which a window
    /// with no page ever (WebKit's sandbox cannot start; a hub; a page window) never calls.
    pub fn new(init: PanelInit, with_pages: bool, host: Rc<dyn PanelHost>) -> Self {
        let (supervisor, supervisor_pending) = if with_pages {
            connect_supervisor(&init.project_dir, init.backend_kind)
        } else {
            (None, None)
        };
        let state = Rc::new(RefCell::new(panel_state(init, supervisor, supervisor_pending, host)));
        let raise = state.borrow().wake.as_ref().map(crate::panel_wake::PanelWake::waker);
        AgentPanelHandle {
            state,
            pages: Pages::default(),
            with_pages: Rc::new(std::cell::Cell::new(with_pages)),
            drives_review: Rc::default(),
            raise,
            partner: Rc::default(),
            arrived_moves: Rc::default(),
        }
    }

    /// This window will have pages after all: what [`new`](Self::new) does with pages, done now that the window knows.
    /// Called before its first page is attached.
    pub fn open_pages(&self) {
        self.wake();
        if self.with_pages.replace(true) {
            return;
        }
        let (project_dir, backend_kind) = {
            let state = self.state.borrow();
            (state.project_dir.clone(), state.backend_kind)
        };
        let (supervisor, supervisor_pending) = connect_supervisor(&project_dir, backend_kind);
        let mut state = self.state.borrow_mut();
        state.supervisor = supervisor;
        state.supervisor_pending = supervisor_pending;
    }

    /// Says on the log that this window has a backend and no panel: WebKit's sandbox cannot start here.
    pub fn say_no_panel(&self) {
        self.wake();
        println!(
            "[agent_panel] backend: {} (no panel: WebKit's sandbox cannot start here)",
            self.state.borrow().backend_kind.as_str()
        );
    }

    /// This window keeps no saved tabs: it offers none, claims none and writes none, at its close too. Its project's
    /// tabs are the other windows' to keep.
    pub fn keep_no_tabs(&self) {
        self.wake();
        let mut state = self.state.borrow_mut();
        state.tab_memory.disable();
        state.restore_source = None;
    }

    /// This window's tabs are window 1's to save: `hook` hears each new set, compared on every tick as the writer's
    /// are, and must not reach back into the panel.
    pub fn report_tabs(&self, hook: impl Fn(&eitri_core::saved_tabs::SavedTabs) + 'static) {
        self.wake();
        self.state.borrow_mut().group_tabs.report_to(Rc::new(hook));
    }

    /// The tabs as they stand, whatever the last tick saw; `None` while the panel is busy or closing.
    pub fn tabs_now(&self) -> Option<eitri_core::saved_tabs::SavedTabs> {
        self.wake();
        let state = self.state.try_borrow().ok()?;
        state.tabs.saved_snapshot(&state.conversation_id)
    }

    /// Tabs this window was given and the user has said nothing about: on offer, or waiting for their restore. Closing
    /// the window is no answer to them. `None` when there are none, or while the panel is busy.
    pub fn unopened_tabs(&self) -> Option<eitri_core::saved_tabs::SavedTabs> {
        self.wake();
        unopened_tabs(&*self.state.try_borrow().ok()?)
    }

    /// What this window's part of the file should hold when it goes without the user closing it: its tabs and any it has
    /// not opened. `None` when that says nothing to window 1: the panel is busy, or the window has told nothing and has
    /// nothing, so what is kept for it stands.
    pub fn tabs_to_keep(&self) -> Option<eitri_core::saved_tabs::SavedTabs> {
        self.wake();
        let state = self.state.try_borrow().ok()?;
        tabs_to_keep(&state)
    }

    /// Tabs window 1 kept for this window: restored or offered as a launch's own saved tabs are, under `agent.restore`.
    pub fn give_tabs(&self, tabs: eitri_core::saved_tabs::SavedTabs) {
        self.wake();
        self.run_seen(|state, screen, out| take_given_tabs(state, screen, tabs, out));
    }

    /// Whether this window's tabs are remembered at all: `agent.restore` is not `"off"`.
    pub fn remembers_tabs(&self) -> bool {
        self.wake();
        self.state.borrow().restore_policy != RestorePolicy::Off
    }

    /// The group's tabs file's last write: this window's close holds for it as it holds for a window's own file.
    pub fn hold_close_for_tabs(&self, write: std::sync::mpsc::Receiver<()>) {
        self.wake();
        self.state.borrow_mut().group_tabs.last_write = Some(write);
    }

    /// `page` becomes `view`'s live page, starting with a fresh record (no document ready yet): a new
    /// page, or the idle page that showed `view` before, attached again. Whatever page `view` had is
    /// replaced; a page is never moved to another view. The page is given the typing cadence and the
    /// editor's keys as they stand; its document is loaded by [`load_document`](Self::load_document).
    pub fn attach_page(&self, view: ViewId, page: Rc<PanelPage>) {
        self.wake();
        if !self.with_pages.get() {
            return;
        }
        let (cadence, editor_has_keys) = {
            let mut state = self.state.borrow_mut();
            // Whether the view's pane has the keys is the pane's, not the document's: it carries over.
            let pane_focused = state.page(view).is_some_and(|record| record.pane_focused);
            state.pages.insert(
                view,
                PageState {
                    pane_focused,
                    ..PageState::default()
                },
            );
            (state.typing_cadence, state.editor_has_keys)
        };
        {
            let mut pacer = page.pacer().borrow_mut();
            pacer.set_cadence(cadence);
            pacer.set_editor_has_keys(editor_has_keys);
        }
        self.pages.0.borrow_mut().insert(view, PageSlot { page, live: true });
    }

    /// `view`'s page leaves routing: its record is dropped, what its pacer held is let go, the prompts
    /// it showed go, the answers it was still owed are never sent (its document that asked is gone,
    /// and the next one numbers its requests afresh), and it is given a blank document at the panel's
    /// base ([`IDLE_DOCUMENT`]), so it shows nothing of the tab it showed and holds no chat while no
    /// one sees it; reusing the page loads the panel again ([`Self::load_document`]). Envelopes sent
    /// to it meanwhile are dropped by the blank page, and its background still follows the theme. The
    /// base view's page never goes idle.
    pub fn idle_page(&self, view: ViewId) {
        self.wake();
        if view.is_base() {
            eprintln!("[agent_panel] the base view's page cannot go idle");
            return;
        }
        let Some(page) = self.pages.any(view) else { return };
        if let Some(slot) = self.pages.0.borrow_mut().get_mut(&view) {
            slot.live = false;
        }
        {
            let mut state = self.state.borrow_mut();
            state.pages.remove(&view);
            state.owed.page_closed(view);
            // A proposal still being read answers a page that is gone: finishing it would raise the editor
            // and take the keys for a view no one sees, and a reused view's next page numbers its requests
            // afresh. Dropping the receiver leaves the worker's send with nowhere to go.
            state.proposal_jobs.retain(|job| job.view != view);
            if !state.shutting_down {
                state.tabs.drop_bypass_prompt_in(view);
            }
        }
        let discarded = page.pacer().borrow_mut().discard_pending();
        trace_discarded_first_texts(&mut self.state.borrow_mut(), discarded);
        page.load_document(IDLE_DOCUMENT, PANEL_BASE_URI);
    }

    /// Paints `view`'s page's background and loads the themed document into it, live or idle; called
    /// once the host listens for the page's messages, and again when an idle page is reused.
    pub fn load_document(&self, view: ViewId) {
        self.wake();
        let Some(page) = self.pages.any(view) else { return };
        let (tokens, document) = {
            let state = self.state.borrow();
            (state.theme.clone(), state.document)
        };
        page.set_background([tokens.bg.r, tokens.bg.g, tokens.bg.b]);
        page.load_document(&document.themed(&tokens.css_vars()), PANEL_BASE_URI);
    }

    /// A message `view`'s page posted (the `eitriAgent` handler that page was built with). A page that
    /// is not live -- idle, or never attached -- has nothing to say: its message is dropped, save the
    /// one late mirror of the tab that just left it (`late_draft_from_idle_page`).
    pub fn page_message(&self, view: ViewId, raw: &str) {
        self.wake();
        let Some(page) = self.pages.live(view) else {
            if !self.run(|state, out| late_draft_from_idle_page(raw, state, view, out)) {
                eprintln!(
                    "[agent_panel] view {}: a message from a page that is not live was dropped",
                    view.number()
                );
            }
            return;
        };
        let Some(message) = parse_inbound_message(raw) else {
            // Already logged inside parse_inbound_message -- an unparseable/unrecognized message
            // from the WebView must never crash the shell process. No requestId is known for it, so
            // no command_result is possible (see parse_inbound_message's own doc).
            return;
        };
        // The window is closing and its tabs are gone; a document still posting has nothing to reach.
        if self.state.borrow().shutting_down {
            return;
        }
        self.poll_partner_chooser();
        if let InboundMessage::Resume {
            provider_session_id, ..
        } = &message
        {
            let state = self.partner.borrow();
            let pick = state
                .pick
                .clone()
                .filter(|_| state.partner.is_some() && state.sessions.contains(provider_session_id));
            drop(state);
            if let Some(pick) = pick {
                pick(view, provider_session_id.clone());
                self.dispatch(
                    view,
                    eitri_core::agent_bridge::serialize_command_result_for_js(message.request_id(), Ok(())),
                );
                return;
            }
        }
        // What the window says about the message is asked before its arm runs, each with the state
        // released: which pages are on screen, and, for an answer the window must agree to, its gate.
        let screen = Screen::read(&self.state, &self.pages);
        let Some(admitted) = self.run(|state, out| admit(state, &message, view, out)) else {
            return;
        };
        let refused = match &admitted.admission {
            Admission::Ungated => None,
            Admission::Refused(why) => Some(why.clone()),
            Admission::Gated(asked) => ask_gate(&self.state, *asked).err(),
        };
        self.run(|state, out| {
            let owed = handle_inbound_message(state, message, view, admitted, refused, &screen, page.pacer());
            out.extend(owed);
        });
    }

    /// `view`'s page came on screen, or a float stopped covering it: whatever finished in the tab it
    /// shows while the page was away has been seen (the tray's `agent •`). Nothing is seen while the
    /// page is unmapped or a float covers it (`set_coverage`).
    pub fn page_shown(&self, view: ViewId) {
        self.wake();
        if self.pages.live(view).is_none() || self.state.borrow().shutting_down {
            return;
        }
        self.run_seen(|state, screen, out| {
            // Mapped under a float is not seen: the window calls this again once the float is off it.
            if !screen.shows(view) {
                return;
            }
            let before = state.tabs.attention();
            if let Some(tab) = state.tab_of_page(view) {
                state.tabs.mark_seen(tab);
            }
            report_attention(state, before, out);
        });
    }

    /// A link in the page was clicked. A markdown link the model rendered must never navigate the
    /// panel itself (that origin would inherit the same message bridge the panel uses to relay
    /// permission decisions), so the host keeps the page on its document and calls this instead. A web
    /// page or a mail draft goes to the system's handler; nothing else is launched: `sms:`, `tel:`,
    /// `xmpp:`, `ftp:` and the like hand the link's data to another program on one click, and a
    /// relative link resolves to the panel's own address, which goes nowhere. A refusal is logged (the
    /// address through `{:?}`, which escapes anything a terminal would act on) and flashed, so the
    /// click is not silently lost.
    pub fn link_clicked(&self, view: ViewId, uri: Option<&str>) {
        self.wake();
        let Some(page) = self.pages.live(view) else { return };
        match uri.and_then(clicked_link_url) {
            // The host runs arbitrary code, so it is called with the state released.
            Some(url) => self.run(|_, out| out.effect(Effect::OpenExternal(url.to_string()))),
            None => {
                eprintln!("[agent_panel] link click refused: {uri:?}");
                page.send(&eitri_core::agent_bridge::serialize_notice_for_js(
                    "not opened: this link's address is not one Eitri opens",
                ));
            }
        }
    }
}

impl AgentPanelHandle {
    /// For a move of `tab` to the other window: the panel's own record of the tab, or why it does not
    /// move now.
    pub fn panel_tab_handoff(&self, tab: TabId) -> Result<PanelTabHandoff, String> {
        self.wake();
        panel_tab_handoff(&self.state.borrow(), tab)
    }

    /// Takes up the panel's record of a tab another window moved here as `tab`.
    pub fn adopt_panel_tab(&self, tab: TabId, handoff: PanelTabHandoff) {
        self.wake();
        adopt_panel_tab(&mut self.state.borrow_mut(), tab, handoff);
    }

    /// Where a move's answers go: a freeze made or refused, a presence held or not. `main.rs` installs this once
    /// in a window that has another window.
    pub fn on_chat_move(&self, hook: impl Fn(ChatMoveEvent) + 'static) {
        self.wake();
        self.state.borrow_mut().chat_moves.hook = Some(Rc::new(hook));
    }

    /// What a move of `view`'s tab would take (in the base view the tab holding `session` when one is named), or
    /// why the tab does not move now.
    pub fn chat_move_facts(
        &self,
        view: ViewId,
        session: Option<&str>,
    ) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        self.wake();
        super::chat_moves::chat_move_facts(&self.state.borrow(), view, session)
    }

    /// The view whose tabs hold `session`, when a tab here holds it.
    pub fn view_of_session(&self, session: &str) -> Option<ViewId> {
        self.wake();
        super::chat_moves::view_of_session(&self.state.borrow(), session)
    }

    /// Freezes `view`'s tab (or `tab`) for move `mv`, answered later through [`Self::on_chat_move`]: the page is
    /// asked for its view of the tab first, for at most a second.
    pub fn freeze_for_move(&self, view: ViewId, tab: Option<TabId>, mv: u64) {
        self.wake();
        self.run(|state, out| super::chat_moves::begin_freeze(state, view, tab, mv, out));
    }

    /// The move of `view`'s tab (or `tab`) did not happen: the tab takes input and answers cards again.
    pub fn resume_moved(&self, view: ViewId, tab: Option<TabId>) {
        self.wake();
        super::chat_moves::resume_moved(&mut self.state.borrow_mut(), view, tab);
    }

    /// The other window holds `view`'s frozen tab (or `tab`) now: it leaves this window, its session not ended.
    pub fn forget_moved(&self, view: ViewId, tab: Option<TabId>) -> Result<(), String> {
        self.wake();
        self.run(|state, out| super::chat_moves::forget_moved(state, view, tab, out))
    }

    /// The commit of a move into `view`: the tab another window froze is taken up from `record` and `fds`.
    pub fn adopt_moved(
        &self,
        view: ViewId,
        record: &serde_json::Value,
        fds: Vec<std::os::fd::OwnedFd>,
    ) -> Result<TabId, String> {
        self.wake();
        let tab = self.run(|state, out| super::chat_moves::adopt_moved(state, view, record, fds, out))?;
        if let Some(mv) = record["workspace_move_id"].as_u64() {
            let state = self.state.borrow();
            let mut arrived = self.arrived_moves.borrow_mut();
            arrived.retain(|_, id| state.tabs.get(*id).is_some());
            arrived.insert(mv, tab);
        }
        Ok(tab)
    }

    /// The local tab an arriving move created, after its adopt has run.
    pub fn arrived_tab(&self, mv: u64) -> Option<TabId> {
        self.wake();
        self.arrived_moves
            .borrow()
            .get(&mv)
            .copied()
            .filter(|id| self.state.borrow().tabs.get(*id).is_some())
    }

    pub fn base_tabs(&self) -> Vec<TabId> {
        self.wake();
        self.state.borrow().tabs.base_tabs()
    }

    pub fn active_base_tab(&self) -> Option<TabId> {
        self.wake();
        Some(self.state.borrow().tabs.active())
    }

    pub fn tab_move_facts(&self, tab: TabId) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        self.wake();
        super::chat_moves::facts_for_tab(&self.state.borrow(), tab)
    }

    /// What a hand-over of `tab` to another window would take, or why it cannot go.
    pub fn tab_hand_over_facts(&self, tab: TabId) -> Result<eitri_core::wm::TabHandOver, String> {
        self.wake();
        super::hand_over::facts(&self.state.borrow(), tab)
    }

    /// Lets the tab holding `agent`'s `session` go for a hand-over, answered later through [`Self::on_chat_move`]: once
    /// its backend has let the session go, or at once when the tab stays.
    pub fn release_tab(&self, agent: &AgentId, session: &str) {
        self.wake();
        self.run(|state, out| super::hand_over::release(state, agent, session, out));
    }

    /// Takes up a tab another window let go, as a launch brings a saved tab back; answered later through
    /// [`Self::on_chat_move`], once the tab has settled here.
    pub fn adopt_tab(&self, tab: eitri_core::saved_tabs::SavedTab) {
        self.wake();
        self.run(|state, out| super::hand_over::adopt(state, tab, out));
    }

    pub fn moved_tab_key(&self, tab: TabId) -> Option<eitri_core::window_link::base_tabs::MovedTabKey> {
        self.wake();
        let state = self.state.borrow();
        let tab = state.tabs.get(tab)?;
        Some(eitri_core::window_link::base_tabs::MovedTabKey {
            agent: tab.agent().clone(),
            session: tab.provider_session_id()?,
        })
    }

    /// The tabs of the base view, and the extra views, holding work a close would end: a running turn, a
    /// connect, a waiting question, a turn the CLI started, or live background work.
    pub fn running_chats(&self) -> Vec<(eitri_core::layout::ModuleId, Option<TabId>)> {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return Vec::new();
        }
        state
            .tabs
            .tabs()
            .iter()
            .filter(|t| {
                t.turn_running()
                    || matches!(t.backend(), TabBackend::Starting(_))
                    || t.question_waiting()
                    || t.cli_unseen_busy()
                    || t.background_calls() > 0
            })
            .map(|t| {
                let view = state.tabs.view_of(t.id);
                (
                    if view.is_base() {
                        eitri_core::layout::ModuleId::agent()
                    } else {
                        view.module()
                    },
                    view.is_base().then_some(t.id),
                )
            })
            .collect()
    }

    /// The controller supplies the complete order after every arrival in the workspace has decided.
    pub fn order_base_tabs(&self, order: &[TabId], active: Option<TabId>) -> Result<(), String> {
        self.wake();
        self.run(|state, out| {
            let existing: Vec<_> = state
                .tabs
                .base_tabs()
                .into_iter()
                .filter(|t| !order.contains(t))
                .collect();
            let tabs = eitri_core::window_link::base_tabs::order_after_arrival(&existing, None, order);
            // The tabs a workspace brought are numbered after the ones here, which keep their numbers; once ordered
            // they are this view's own, so a later ordering keeps their numbers too.
            let arrived: Vec<_> = {
                let mut moves = self.arrived_moves.borrow_mut();
                let arrived = moves.values().copied().filter(|t| tabs.contains(t)).collect();
                moves.retain(|_, t| !tabs.contains(t));
                arrived
            };
            if !state.tabs.order_base_tabs(&tabs, &arrived) {
                return Err("the arriving tabs have changed before they could be ordered".into());
            }
            if let Some(active) = active {
                state.tabs.select(active);
            }
            switch_payloads(state, out);
            Ok(())
        })
    }

    /// Before the commit of move `mv`: this window's presence holds `session`'s running turn, answered through
    /// [`Self::on_chat_move`].
    pub fn hold_presence(&self, session: &str, mv: u64) {
        self.wake();
        self.run(|state, out| super::chat_moves::hold_presence(state, session, mv, out));
    }

    /// The arrival of `session` with move `mv` was given up.
    pub fn drop_presence(&self, session: &str, mv: u64) {
        self.wake();
        super::chat_moves::drop_presence(&mut self.state.borrow_mut(), session, mv);
    }
}

impl AgentPanelHandle {
    /// The tick's steady cadence: fast enough that streaming text feels live (a ~30-60fps cadence). The
    /// host runs the tick when [`run_tick`](Self::run_tick) and the wake say so, never sooner than this after
    /// the last one, and on this cadence only while something it acts on cannot say when it is ready
    /// (`crate::panel_wake`).
    pub const TICK: std::time::Duration = std::time::Duration::from_millis(33);

    /// The descriptor the host watches for reading; when it is readable the host calls
    /// [`take_wake`](Self::take_wake). `None`: there is no wake, and the host runs the tick every
    /// [`TICK`](Self::TICK) as [`run_tick`](Self::run_tick) says.
    pub fn wake_fd(&self) -> Option<std::os::fd::RawFd> {
        if !self.with_pages.get() && !self.drives_review.get() {
            return None;
        }
        self.state.borrow().wake.as_ref().map(crate::panel_wake::PanelWake::fd)
    }

    /// Asks the host to run the tick soon: whatever a call into the panel changed is sent by that tick.
    /// Cheap and idempotent until the host takes it.
    pub fn wake(&self) {
        if let Some(raise) = &self.raise {
            raise();
        }
    }

    /// The wake's descriptor was readable: takes what was raised and says when the tick is due -- now, or
    /// one [`TICK`](Self::TICK) after the last tick if that is later, so a stream of folds is taken in the
    /// batches a steady tick made. `None` when nothing was raised.
    pub fn take_wake(&self, now: std::time::Instant) -> Option<std::time::Instant> {
        let state = self.state.borrow();
        if !state.wake.as_ref().is_some_and(crate::panel_wake::PanelWake::take) {
            return None;
        }
        Some(state.last_tick.map_or(now, |last| (last + Self::TICK).max(now)))
    }

    /// Runs the tick at `now` and says when the host runs it next: on the steady cadence while anything
    /// it acts on is pending (and once more after that, for what a finished worker sent just before it
    /// was seen to finish), at the typing cadence's next release, or only on the wake.
    pub fn run_tick(&self, now: std::time::Instant) -> crate::panel_wake::NextTick {
        use crate::panel_wake::NextTick;
        let polled_before = needs_polling(&self.state.borrow());
        self.state.borrow_mut().last_tick = Some(now);
        if !self.tick() {
            return NextTick::Stop;
        }
        let state = self.state.borrow();
        let mut next = (polled_before || needs_polling(&state)).then_some(now + Self::TICK);
        // Every live page's typing cadence, each against its own document.
        for (view, page) in self.pages.live_views() {
            let paced = page
                .pacer()
                .borrow()
                .next_deadline(std::time::Instant::now(), state.ready(view))
                // Never at the moment this tick started: a deadline that would not move cannot spin the loop.
                .map(|at| at.max(now + std::time::Duration::from_millis(1)));
            next = match (next, paced) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        next.map_or(NextTick::OnWake, NextTick::At)
    }

    /// The panel's single main-loop tick, over EVERY tab. Three jobs, in order:
    ///
    /// 1. collect every backend that finished constructing on a worker thread (and every handoff whose
    ///    close finished), and answer the command that has been waiting on it;
    /// 2. drain every tab's backend (`TabSet::pump_views`): `take_ui_delivery` is where the permission
    ///    policy answers what needs no human, so a tab that was not pumped would stall. Each shown tab's
    ///    batch -- the base view's active tab's, and each bound tab's -- goes to the page that shows it
    ///    as one `events{tab,fromRevision,throughRevision,events[]}` envelope; a background tab's feeds
    ///    its attention and marks it stale;
    /// 3. report the window's aggregated status to `eitri-supervisor`.
    ///
    /// Then each page's `tabs` envelope and `hello` go out if what they describe changed, and the
    /// extra views left holding no tab are told to the window.
    pub fn tick(&self) -> bool {
        self.poll_partner_chooser();
        let state = &self.state;
        let pages = &self.pages;
        // No page, no session ever starts and nothing could be shown: nothing to drive, unless the window's editor
        // serves the reviews of the windows that show the chats.
        if !self.with_pages.get() {
            return self.drives_review.get() && self.tick_review();
        }
        // The window has been closed and its tabs taken; everything below would be a no-op
        // against a destroyed window for as long as the close watch holds the application. Stop
        // instead of ticking 30 times a second at nothing. See `AgentPanelState::shutting_down`.
        if state.borrow().shutting_down {
            return false;
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            state.borrow_mut().ticks_run += 1;
        }
        // A move's freeze waits for its page's answer, and an arrival for its presence: both are read here.
        self.run(super::chat_moves::poll_chat_moves);
        // Each chat view's host is unmapped while it is hidden or zoomed away; mapped under a float,
        // it is off screen too. Asked before the state is borrowed: the window's probe may read it.
        let live = pages.live_views();
        let screen = Screen::read(state, pages);
        // Read before anything below changes it: a session installed this tick resets its count.
        let attention_before = state.borrow().tabs.attention();
        // The payloads are built under the borrow and dispatched OUTSIDE it. Calling into WebKit while
        // holding a `RefMut` on the panel's own state is a latent re-entrancy hazard: the script
        // message handler takes the same `RefCell`, and anything that let it run during the
        // dispatch would panic on an already-borrowed cell rather than fail gracefully.
        let pumped = self.run(|state, out| tick_collect(state, &screen, out));
        // Each stream goes through its own page's typing cadence (`crate::panel_pacer`): held for the
        // next slot while the user types in the editor, if it is plain streamed content; at once
        // otherwise, and always at once into a page that has not said `ready` (its snapshot
        // follows), as before. Whatever a slot or the end of the typing window releases goes out
        // in this same tick. A payload for a view with no live page has nowhere to go: that page's
        // `ready` sends the tab's whole state.
        for (view, page) in &live {
            let page_ready = state.borrow().ready(*view);
            {
                let sink = page.sink();
                let mut pacer = page.pacer().borrow_mut();
                let now = std::time::Instant::now();
                for (_, (payload, first_text, class)) in pumped.streams.iter().filter(|(to, _)| to == view) {
                    let class = if page_ready {
                        *class
                    } else {
                        eitri_core::panel_cadence::EnvelopeClass::Immediate
                    };
                    pacer.send_stream(&sink, payload.clone(), *first_text, class, now);
                }
                pacer.poll(&sink, now, page_ready);
            }
            let sent = page.pacer().borrow_mut().take_sent();
            let mut state_ref = state.borrow_mut();
            // Stamped at the dispatch call that really sent it -- where the WebView's own clock starts,
            // which under the cadence may be a later tick or a flush between ticks -- for the tab and
            // turn it was pumped for.
            stamp_first_text_dispatches(&mut state_ref, sent.first_text);
            if sent.any {
                let shows = state_ref.tab_of_page(*view);
                if let Some(trace) = shows
                    .and_then(|tab| state_ref.tabs.get_mut(tab))
                    .and_then(|t| t.session.turn_trace.as_mut())
                {
                    if trace.is_complete() {
                        trace.emit();
                    }
                }
            }
        }
        self.run(|state, out| tick_send(state, &screen, pumped, attention_before, out));
        self.run(pair_outbound);
        true
    }

    /// The one job of a tick with no page: the overlay in this window's editor moves on, and what it found goes out
    /// to the windows it was drawn for.
    fn tick_review(&self) -> bool {
        if self.state.borrow().shutting_down {
            return false;
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            self.state.borrow_mut().ticks_run += 1;
        }
        self.run_seen(poll_review_flow);
        self.run(pair_outbound);
        true
    }

    /// This window will show no page, but other windows' reviews draw in its editor: from now on its tick runs for
    /// them, and the host drives it as it drives a window with pages.
    pub fn drive_review_without_pages(&self) {
        self.wake();
        self.drives_review.set(true);
    }

    /// Shuts down every tab's backend, INCLUDING one still being constructed on a worker thread,
    /// all of them in parallel under one backstop.
    ///
    /// The in-flight case is not hypothetical: since construction moved off the GTK main loop, a
    /// window closed during a connect leaves a fully-built backend -- a live sidecar process and a
    /// Tokio runtime thread -- owned by nothing but a channel nobody will ever read.
    ///
    /// Taking every tab (`TabSet::take_all`) is load-bearing twice over: it lets a backend that
    /// already finished be shut down properly, and it leaves nothing for a later tick to install a
    /// session into -- the pump stops on `shutting_down` anyway.
    ///
    /// **Every live backend is torn down at once** (`tear_down_all`): one worker per tab, one
    /// collector, one backstop. Serial sidecar closes are bounded at 10 s each, so N tabs torn down
    /// in turn could outlive any backstop.
    ///
    /// **This function does not wait for anything, and that is its most important property.** It
    /// runs from `window.connect_close_request` on the GTK main thread, where a wait keeps the
    /// window mapped and frozen until it returns; two previous versions of this code waited there
    /// (3s, then 15s) while describing themselves as not doing so. Every branch below instead puts
    /// its receiver in the returned [`ShutdownWatch`], which the host hands to `hold_until_done`:
    /// **this function** returns immediately and the process stays alive -- with no window on
    /// screen -- until the teardowns report or their backstops expire.
    ///
    /// **Scoped to the panel deliberately:** the
    /// whole close handler does NOT return immediately: `main.rs`'s `connect_close_request` calls
    /// `pane.shutdown()` first, on the GTK thread, and that spins until nvim exits
    /// (`LiveHarness::shutdown` in the fork). So "the window disappears at once" is a claim about
    /// this half only, and a slow `:qa!` -- a hung plugin, a slow `BufWritePre` -- still delays the
    /// close with nothing here involved.
    ///
    /// **A handoff's close** is already running on its own worker, which owns that backend: there
    /// is nothing to start, only a reason to keep the process alive. It is held on `dropped_rx`,
    /// not `closed_rx` -- `closed_rx` reports when `shutdown()` returned, while on the sidecar path
    /// the child dies in the backend's drop afterwards. `HANDOFF_CLOSE_BACKSTOP` is generous for
    /// the legacy backend and knowingly short of the sidecar's worst case; see its own doc.
    ///
    /// The hold belongs to the host (`hold_until_done`): this handle is per-window, and the process
    /// the watch keeps alive belongs to the application that owns the loop this panel's timers run on.
    pub fn shutdown(&self) -> ShutdownWatch {
        self.wake();
        // Queued words outlive the window as history: handed to the history's writer with whatever it
        // still holds, and its last write watched like the saved tabs' below.
        let history_write = {
            let mut state = self.state.borrow_mut();
            let texts = state.tabs.take_every_queued_text();
            state.history_writer.submit(&texts);
            state.history_writer.finish()
        };
        // The tabs as they are now, whatever the last tick saw: the writer's last set, written before the
        // close lets the process go.
        remember_tabs(&mut self.state.borrow_mut());
        let last_write = {
            let mut state = self.state.borrow_mut();
            state.tab_memory.finish().or(state.group_tabs.last_write.take())
        };
        let tabs = {
            let mut state = self.state.borrow_mut();
            // Read by the tick, by `poll_activate` and by every path into the tab set, all of
            // which keep running now that the loop outlives the window. See the field's own doc.
            state.shutting_down = true;
            state.tabs.take_all()
        };
        let mut entries = Vec::new();
        let mut backends = Vec::new();
        for RemovedTab { mut tab, retired } in tabs {
            if let Some(handoff) = tab.pending_handoff.take() {
                entries.push(watched(
                    "a terminal handoff's close",
                    handoff.dropped_rx,
                    HANDOFF_CLOSE_BACKSTOP,
                    |()| Vec::new(),
                ));
            }
            backends.extend(retired);
            match tab.into_backend() {
                TabBackend::Starting(pending) => entries.push(watch_connect(pending.result_rx)),
                // A live backend left the tab retired, above; a start still waiting for the trust
                // question has spawned nothing.
                TabBackend::Live(_)
                | TabBackend::NotStarted
                | TabBackend::AwaitingTrust(_)
                | TabBackend::Failed { .. } => {}
            }
        }
        entries.extend(tear_down_all("every session tab's teardown", backends));
        if let Some(rx) = history_write {
            entries.push(watched_abandoning(
                "the prompt history's last write",
                rx,
                HISTORY_BACKSTOP,
                "the prompts it still held are not kept: the history file stays the one before them, whole",
                |()| Vec::new(),
            ));
        }
        if let Some(rx) = last_write {
            entries.push(watched_abandoning(
                "the saved tabs' last write",
                rx,
                SAVED_TABS_BACKSTOP,
                "the write is left unfinished: the saved-tabs file stays the one before it, whole, without this \
                 window's last change to its tabs",
                |()| Vec::new(),
            ));
        }
        // Tabs that left earlier, while the window stayed open: their teardowns and connects are
        // still running on workers nothing else would keep alive. Same holds and backstops as the tabs just taken.
        let retiring = std::mem::take(&mut self.state.borrow_mut().retiring);
        entries.extend(retiring.connects.into_iter().map(watch_connect));
        for (_, rx) in retiring.teardowns {
            entries.push(watched("a closed tab's teardown", rx, SESSION_CLOSE_BACKSTOP, |()| {
                Vec::new()
            }));
        }
        ShutdownWatch { entries }
    }

    /// Records whether `view`'s page's document said `ready` (nothing for a page with no record).
    pub(super) fn set_document_ready(&self, view: ViewId, ready: bool) {
        let mut state = self.state.borrow_mut();
        if let Some(record) = state.pages.get_mut(&view) {
            record.document_ready = ready;
        }
    }

    /// Reloads the panel's frontend from scratch, deliberately WITHOUT touching the session.
    ///
    /// The frontend is a pure view layer over `AgentSessionProjection`, which lives here in Rust --
    /// so a WebView that has wedged (a frontend panic leaving a blank pane, a bridge that stopped
    /// dispatching) can be thrown away and rebuilt while the real conversation keeps running. The
    /// fresh document sends `ready` on mount, and `Ready`'s handler answers with `hello` plus a
    /// snapshot of the still-live session, which rehydrates the replacement document.
    ///
    /// `PageSurface::load_document` (WebKit's `load_html`), NOT `WebViewExt::reload()`. The latter
    /// reproducibly leaves this WebView permanently blank with zero further page activity, because the panel's document
    /// is a substitute-data load rather than a fetchable URI -- so the recovery action would itself
    /// be the wedge it exists to undo.
    ///
    /// **The invariant this rests on, stated because it is not obvious and a later change could
    /// break it silently:** `take_ui_delivery()` CONSUMES events. Every envelope the tick
    /// dispatches between this document load and the new document's own `ready` is therefore gone --
    /// the old document is being torn down and the new one has no listener yet. Nothing is lost
    /// only because `Ready`'s handler answers from canonical state with a full snapshot rather than
    /// a replay, and `serialize_snapshot_for_js` already carries tool-call results. A pump that
    /// ever stopped being the sole consumer, or a snapshot that stopped being complete, would turn
    /// this into a quiet data-loss path with no test failing.
    pub(crate) fn reload_document(&self, view: ViewId) {
        let Some(page) = self.pages.live(view) else {
            eprintln!(
                "[agent_panel] no panel document to reload in view {}: no live page shows it",
                view.number()
            );
            return;
        };
        eprintln!(
            "[agent_panel] reloading the panel document; the session is left untouched (view {})",
            view.number()
        );
        // The page that said `ready` is going; the tick holds its envelopes until the new one does.
        self.set_document_ready(view, false);
        // What the typing cadence still holds is in the projection, hence in the new page's snapshot.
        let discarded = page.pacer().borrow_mut().discard_pending();
        trace_discarded_first_texts(&mut self.state.borrow_mut(), discarded);
        // And so is the mode it reported: until the new page's `ready`, nothing is BROWSE or INPUT.
        forget_the_pages_nav_mode(&self.state.borrow(), view);
        let (vars, document) = {
            let state = self.state.borrow();
            (state.theme.css_vars(), state.document)
        };
        page.load_document(&document.themed(&vars), PANEL_BASE_URI);
    }

    /// The user's OWN recovery action for a wedged or crashed panel -- `prefix r`
    /// (`Action::PanelReload`) or the top-bar `\u{21bb}` button (`install_reload_action`) -- as
    /// distinct from the AUTOMATIC reload `page_gone` performs while still within
    /// `crash_guard`'s own budget.
    ///
    /// Re-arms `crash_guard` (`WebViewCrashGuard::reset`) before doing exactly what
    /// [`reload_document`](Self::reload_document) does. **Why a separate method rather than putting
    /// the reset inside `reload_document` itself:** `page_gone`'s own `Reload` branch calls
    /// `reload_document` directly, on every crash still within budget -- if that shared path reset
    /// the guard too, every automatic reload would erase its own crash history and `GiveUp` could
    /// never be reached, defeating the loop guard entirely. Only a genuinely manual recovery may
    /// re-arm it; the automatic path must keep counting toward the very loop it exists to detect.
    pub fn reload_document_by_hand(&self, view: ViewId) {
        self.wake();
        {
            let mut state = self.state.borrow_mut();
            if let Some(record) = state.pages.get_mut(&view) {
                record.crash_guard.reset();
            }
        }
        self.reload_document(view);
    }

    /// The page's content process ended: a real WebKit crash, or the host
    /// genuinely out of memory. Until the signal was connected the chat stayed blank until the user
    /// found `\u{21bb}`/`prefix r` by hand.
    /// Recovery is exactly [`reload_document`](Self::reload_document): the session lives in Rust,
    /// so a fresh document rehydrates from its own `ready`/`hello` round trip with nothing lost.
    ///
    /// Guarded by `crash_guard` (`webview_crash_guard::WebViewCrashGuard`) so a page that cannot
    /// even finish loading does not turn into a reload/crash loop. The host leaves out an end it asked
    /// for itself (WebKit's `TerminatedByApi`): that is no crash and must not count toward the guard's
    /// budget. Nothing on this panel's view ends its process today, but the Lua-panel's call site does.
    pub fn page_gone(&self, view: ViewId, reason: &str) {
        self.wake();
        // The page that reported BROWSE or INPUT is dead, and the give-up document never sends
        // `ready`: left as it was, `main.rs` would keep claiming `Ctrl+j`/`Ctrl+k` for a page that
        // cannot answer.
        forget_the_pages_nav_mode(&self.state.borrow(), view);
        // An idle page's crash is reloaded by its next attach; nothing is routed to it meanwhile.
        let response = {
            let mut state = self.state.borrow_mut();
            state.pages.get_mut(&view).map(|record| record.crash_guard.on_crash())
        };
        let Some(response) = response else { return };
        match response {
            crate::webview_crash_guard::CrashResponse::Reload => {
                eprintln!(
                    "[agent_panel] the WebView's web process terminated ({reason}); reloading it (view {})",
                    view.number()
                );
                self.reload_document(view);
            }
            crate::webview_crash_guard::CrashResponse::GiveUp => {
                eprintln!(
                    "[agent_panel] the WebView's web process kept terminating; giving up on automatic reload (view {})",
                    view.number()
                );
                self.set_document_ready(view, false);
                let html = crate::webview_crash_guard::crash_message_html(
                    "Reload it by hand with \u{21bb} in the top bar, or prefix r.",
                );
                if let Some(page) = self.pages.live(view) {
                    let discarded = page.pacer().borrow_mut().discard_pending();
                    trace_discarded_first_texts(&mut self.state.borrow_mut(), discarded);
                    page.load_document(&html, PANEL_BASE_URI);
                }
            }
            crate::webview_crash_guard::CrashResponse::AlreadyGivenUp => {}
        }
    }

    /// Non-blocking: returns `true` if `eitri-supervisor` asked this window to come to the
    /// front since the last call. `main.rs` polls this on its own timer and calls
    /// `window.present()` in response -- this handle has no `Window` reference of its own (see
    /// `AgentPanelHandle`'s own top-level doc: window lifecycle stays owned by `main.rs`).
    pub fn poll_activate(&self) -> bool {
        let mut state = self.state.borrow_mut();
        // The window this would raise is gone; see `AgentPanelState::shutting_down`.
        if state.shutting_down {
            return false;
        }
        if let Some(supervisor) = state.supervisor.as_mut() {
            supervisor.poll_activate()
        } else {
            false
        }
    }

    /// Where the window's link to `eitri-supervisor` stands, for a host that waits on its socket instead
    /// of polling [`poll_activate`](Self::poll_activate).
    pub fn supervisor_link(&self) -> SupervisorLink {
        let state = self.state.borrow();
        if state.shutting_down {
            return SupervisorLink::Absent;
        }
        match (&state.supervisor, &state.supervisor_pending) {
            (Some(client), _) => client
                .activate_fd()
                .map_or(SupervisorLink::Absent, SupervisorLink::Socket),
            (None, Some(_)) => SupervisorLink::Connecting,
            (None, None) => SupervisorLink::Absent,
        }
    }

    /// Records `tokens` as the panel's current theme, repaints the WebView's own background, and
    /// pushes the variables to the live document.
    ///
    /// Safe to call before the page has loaded: the dispatch is guarded, and the `ready` handshake
    /// sends the recorded envelope anyway, so neither ordering loses it.
    pub fn set_theme(&self, tokens: &eitri_core::theme::ThemeTokens) {
        self.wake();
        let payload = eitri_core::agent_bridge::serialize_theme_for_js(tokens);
        self.state.borrow_mut().theme = tokens.clone();
        // Every page, idle ones too: a reused page must not show the old colours for a frame.
        for page in self.pages.every() {
            page.set_background([tokens.bg.r, tokens.bg.g, tokens.bg.b]);
            page.send_direct(&payload);
        }
    }

    /// Updates only the recorded theme's `font_size_px` and re-sends it -- the zoom-together
    /// design's path (`text_size::TextSizeController`) for pushing a new panel scale without
    /// re-deriving any other themed colour or token. Built on `set_theme`, so both a panel reload
    /// (`reload_document`, which reads straight from the recorded theme) and a fresh `ready`
    /// handshake (whose `hello` theme payload comes from the same recorded field) pick the new
    /// size up automatically -- neither has a separate path that could fall out of sync with it.
    pub fn set_panel_font_size_px(&self, font_size_px: f32) {
        self.wake();
        let tokens = {
            let mut tokens = self.state.borrow().theme.clone();
            tokens.font_size_px = font_size_px;
            tokens
        };
        self.set_theme(&tokens);
    }

    /// The twin of [`set_panel_font_size_px`](Self::set_panel_font_size_px), for
    /// `--nv-editor-row`: updates only the recorded theme's `editor_row_px` and
    /// re-sends it, so both a panel reload and a fresh `ready` handshake pick the editor's cell
    /// height up the same way `font_size_px` already does, with no new envelope. `main.rs` calls
    /// this from `NeovideEditorPane::connect_cell_size_changed` and on a `gtk-xft-dpi` change, in
    /// the panel's CSS px -- already converted from the editor's GTK logical px by
    /// `webkit_zoom::EditorRow`.
    pub fn set_editor_row_px(&self, editor_row_px: f32) {
        self.wake();
        let tokens = {
            let mut tokens = self.state.borrow().theme.clone();
            tokens.editor_row_px = Some(editor_row_px);
            tokens
        };
        self.set_theme(&tokens);
    }
}

impl AgentPanelHandle {
    /// Dispatches one already-serialized envelope to the live document, guarded so it is safe to
    /// call before the page has loaded (in which case `ready`'s own batch re-sends whatever state
    /// mattered, and this call is simply a no-op).
    pub(super) fn dispatch(&self, view: ViewId, payload: String) {
        // No live page for the view: nothing is listening, as before a page loads.
        let Some(page) = self.pages.live(view) else { return };
        Self::dispatch_to(&page, &payload);
    }

    /// [`dispatch`](Self::dispatch) to every page, idle ones included (the keymap, how thinking is
    /// shown, the editor link). An idle page's blank document runs nothing and drops it.
    pub(super) fn dispatch_every(&self, payload: String) {
        for page in self.pages.every() {
            Self::dispatch_to(&page, &payload);
        }
    }

    /// [`dispatch`](Self::dispatch) to every live page.
    pub(super) fn dispatch_live(&self, payload: String) {
        for (_, page) in self.pages.live_views() {
            Self::dispatch_to(&page, &payload);
        }
    }

    /// The one send past the pacer for a dispatch: behind whatever the typing cadence is holding, as
    /// `PanelPage::send` is.
    pub(super) fn dispatch_to(page: &PanelPage, payload: &str) {
        page.flush();
        page.send_direct(payload);
    }

    /// The user pressed a key in the editor and it reached nvim (`NeovideEditorPane::
    /// connect_key_activity`). Starts, or extends, the typing window that paces the stream
    /// (`crate::panel_pacer`). O(1): it runs in the press's path.
    ///
    /// Wakes the tick only when the key began a typing window: a key inside one moves only the window's
    /// end, which the tick already waits for, and a wake per keystroke would cost a tick per keystroke.
    pub fn note_editor_key(&self) {
        let now = std::time::Instant::now();
        let mut began = false;
        for page in self.pages.every() {
            began |= page.pacer().borrow_mut().note_editor_key(now);
        }
        if began {
            self.wake();
        }
    }

    /// Whether the editor holds the window's keys (`pane_focus`). Losing them ends the typing window
    /// at once: the next tick releases what is held.
    pub fn set_editor_has_keys(&self, has_keys: bool) {
        self.wake();
        self.state.borrow_mut().editor_has_keys = has_keys;
        for page in self.pages.every() {
            page.pacer().borrow_mut().set_editor_has_keys(has_keys);
        }
    }

    /// `agent.typing_cadence_hz` as `init.lua` left it (`None`: `"off"`), applied once after it ran, to
    /// every page and to each page attached later.
    pub fn set_typing_cadence(&self, cadence_hz: Option<u32>) {
        self.wake();
        self.state.borrow_mut().typing_cadence = cadence_hz;
        for page in self.pages.every() {
            page.pacer().borrow_mut().set_cadence(cadence_hz);
        }
    }

    /// `agent.restore` as `init.lua` left it, applied once after it ran. `Off` also stops the tabs
    /// being remembered at all and forgets what the last window left.
    pub fn set_restore_policy(&self, policy: RestorePolicy) {
        self.wake();
        let mut state = self.state.borrow_mut();
        state.restore_policy = policy;
        if policy == RestorePolicy::Off {
            state.tab_memory.disable();
            state.restore_source = None;
        }
    }

    /// Which saved tabs the window offers back, decided once `init.lua` has run (the pair's restore depends on
    /// `window.restore`): read and claimed here, before the base page's first `ready` can use them. Given again
    /// later with more files to offer (window 2 not coming back after all), only what was not offered yet is read,
    /// and offered after what still is. Nothing is read when `agent.restore` is `"off"`.
    pub fn set_tab_files(&self, files: eitri_core::saved_tabs::TabFiles) {
        self.wake();
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        if state.restore_policy == RestorePolicy::Off {
            state.tab_files = Some(files);
            return;
        }
        let tabs_dir = eitri_core::saved_tabs::state_dir(
            state.review_roots.state_home.as_deref(),
            state.review_roots.home.as_deref(),
        );
        let (offer, notes, takeovers) =
            files.startup_beyond(state.tab_files.as_ref(), tabs_dir.as_deref(), &state.project_dir);
        for note in notes {
            eprintln!("{note}");
        }
        state.tab_memory.take_over_when_written(takeovers);
        state.restore_source = match (state.restore_source.take(), offer) {
            (Some(earlier), Some(later)) => Some(earlier.followed_by(later)),
            (earlier, later) => earlier.or(later),
        };
        state.tab_files = Some(files);
    }

    /// Window 1 starting window 2 again at its launch: `agent.restore = "auto"` waits until window 2 is back or
    /// is not ([`AgentPanelHandle::release_launch_restore`]), so the saved tabs of a window 2 that does not come
    /// back return in the same restore as this window's own.
    pub fn hold_launch_restore(&self) {
        self.wake();
        self.state.borrow_mut().launch_restore_held = true;
    }

    /// Window 2 is back, or is not (its saved tabs given first, [`AgentPanelHandle::set_tab_files`]): the launch
    /// restore runs on the next tick if the base page was ready meanwhile.
    pub fn release_launch_restore(&self) {
        self.wake();
        self.state.borrow_mut().launch_restore_held = false;
    }

    /// `agent.default_mode` as `init.lua` left it (`None`: unset), applied once after it ran. A mode
    /// Shift+Tab left for this project keeps winning; otherwise the empty tab the window already has,
    /// and every new one, start in it (an agent that offers no manual mode starts in auto instead).
    /// Naming bypass here is the user's own answer to the question that is otherwise asked every time,
    /// so saved bypass tabs also come back in bypass unasked.
    pub fn set_default_mode(&self, mode: Option<SessionModeChoice>) {
        self.wake();
        let mut state = self.state.borrow_mut();
        state.bypass_by_config = mode == Some(SessionModeChoice::Bypass);
        let Some(mode) = mode else { return };
        let remembered = state.mode_remembered.then(|| state.tabs.default_mode());
        if remembered.is_none() {
            state.tabs.apply_configured_default(mode);
        }
        // Said here, where it is decided, so the log never claims a mode new tabs do not start in.
        eprintln!("{}", default_mode_said(mode, remembered));
    }

    /// `agent.default` as `init.lua` left it, applied once after it ran: every tab made without naming
    /// an agent, and the empty tab the window already has, run it.
    pub fn set_default_agent(&self, agent: AgentId) {
        self.wake();
        let mut state = self.state.borrow_mut();
        state.tabs.apply_configured_agent(agent.clone());
        state.default_agent = agent;
    }

    /// `agent.codex.command` as `init.lua` left it (`None`: unset), read by every Codex start.
    pub fn set_codex_command(&self, command: Option<String>) {
        self.wake();
        self.state.borrow_mut().codex_command = command;
    }

    /// The ACP agents `eitri.agent.register` accepted, applied once after `init.lua` ran.
    pub fn set_acp_agents(&self, agents: Vec<AgentRegistration>) {
        self.wake();
        self.state.borrow_mut().acp_agents = agents;
    }

    /// `review.enabled` and `review.hint` as `init.lua` left them, applied once after it ran. Enabled
    /// installs the turn review (the legacy backend has no turn boundaries to follow and says so);
    /// disabled leaves none, and a request for one says why. The hint only decides whether a
    /// finished turn writes to the status band.
    pub fn set_review(&self, review: ReviewConfig) {
        self.wake();
        let payload = {
            let mut state = self.state.borrow_mut();
            state.apply_review(review);
            review_settings_payload(&state)
        };
        self.dispatch_live(payload);
    }

    /// Where a restore's one-line result goes. `main.rs` installs this once.
    pub fn on_toast(&self, hook: impl Fn(&str) + 'static) {
        self.wake();
        self.state.borrow_mut().toast_hook = Some(Rc::new(hook));
    }

    /// Records whether `view`'s pane has keyboard focus and tells its live document, which
    /// dims its mode block when it does not. Safe before the page has loaded for the same reason
    /// `set_theme` is: the dispatch is guarded, and `ready` re-sends the recorded value.
    pub fn set_pane_focused(&self, view: ViewId, focused: bool) {
        self.wake();
        {
            let mut state = self.state.borrow_mut();
            if let Some(record) = state.pages.get_mut(&view) {
                record.pane_focused = focused;
            }
            if !focused && !state.shutting_down {
                // A pending bypass prompt is about the tab the user was just looking at in this
                // page. Losing the keys (focus moved to another module, e.g. the editor, the
                // terminal or another chat view) must drop it, the same way switching tabs already
                // does (`TabSet::select`), so a `y` that slips through afterwards lands on nothing.
                // Another page's prompts are still on screen there, and stay.
                state.tabs.drop_bypass_prompt_in(view);
            }
        }
        self.dispatch(view, eitri_core::agent_bridge::serialize_pane_focus_for_js(focused));
    }

    /// `agent.thinking` as `init.lua` left it, applied once after it ran. Recorded for every later
    /// `ready` and told to a document that is already listening (the dispatch is guarded, as
    /// `set_pane_focused`'s is).
    pub fn set_thinking_display(&self, display: eitri_core::agent_prefs::ThinkingDisplay) {
        self.wake();
        self.state.borrow_mut().thinking_display = display;
        self.dispatch_every(eitri_core::agent_bridge::serialize_display_for_js(display));
    }

    /// Asks the panel to open its composer with the caret in it. Sent only from the new-tab path
    /// now; see `serialize_enter_input_for_js`. Safe before
    /// the page loads (the dispatch is guarded), in which case there is no composer yet and nothing
    /// happens.
    pub fn enter_input(&self, view: ViewId) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(view, eitri_core::agent_bridge::serialize_enter_input_for_js());
    }

    /// Where every other keyboard arrival lands: BROWSE -- on the last row with following resumed if
    /// the reader was following it, else on the row and scroll they left (2026-09-29; the page decides, from what it parked when the keys left). Sent after `Ctrl+l`/a
    /// tray chip/`prefix a`'s miss have moved GTK focus into the panel (this reverses the 2026-09-19
    /// behaviour of `Ctrl+l` going straight into the composer). Mirrors `enter_input` exactly
    /// except for the envelope it dispatches; safe before the page loads for the same reason.
    pub fn arrive(&self, view: ViewId) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(view, eitri_core::agent_bridge::serialize_arrive_for_js());
    }

    /// Every keyboard route GTK claims before the WebView sees a key is a route away
    /// from a bypass prompt. The panel cancels its own copy on the matching envelope; this drops
    /// Rust's, so a `y` that slipped through is refused by the nonce. HINT (`prefix f`) and `prefix ,` left the prompt open, and the
    /// next ordinary key -- the `y` that copies a code block HINT landed on, a name starting with
    /// `y` -- entered bypass and approved the waiting cards. The same now holds for `arrive`,
    /// `enter_input`, `nav_key`, `literal_key`, `prefix ?`, `prefix i`, `prefix &`, `<leader>bo` and
    /// a card landing, besides `pane_focus false` and the tab switches that already dropped it. Only
    /// the prompts of the page the route is in (`view`): another page's are still on its screen.
    pub(super) fn drop_bypass_prompt_in(&self, view: ViewId) {
        let mut state = self.state.borrow_mut();
        if !state.shutting_down {
            state.tabs.drop_bypass_prompt_in(view);
        }
    }

    /// `view`'s page's last-reported effective mode, for `main.rs`'s
    /// `Ctrl+j`/`Ctrl+k` intercept to decide against (`claims`). `Other` before the page has ever
    /// posted one, and again after every `ready` -- see `PanelKeys::Other`'s own doc.
    pub fn panel_keys(&self, view: ViewId) -> PanelKeys {
        self.state
            .borrow()
            .page(view)
            .map_or(PanelKeys::Other, |record| record.nav_mode.get())
    }

    /// Whether `view`'s page keeps `direction`'s `Ctrl+h/j/k/l` for its own use (`Ctrl+j` in BROWSE, `Ctrl+k` in
    /// INPUT, `claims`), read from the `panel_keys` mirror alone: the window controller asks this while it decides a
    /// key, so it changes nothing and sends the page nothing.
    pub fn nav_claims(&self, view: ViewId, direction: Direction) -> bool {
        claims(self.panel_keys(view), direction)
    }

    /// Hands `view`'s page the navigation key it claimed: `Ctrl+j` in BROWSE, or `Ctrl+k` in INPUT. Safe before the
    /// page loads for the same reason `arrive` is: with no live mirror yet nothing is claimed, so nothing sends one.
    /// A sideways direction is never claimed, so it has no key to send.
    pub fn nav_key(&self, view: ViewId, direction: Direction) {
        self.wake();
        let direction = match direction {
            Direction::Down => NavKeyDirection::Down,
            Direction::Up => NavKeyDirection::Up,
            Direction::Left | Direction::Right => {
                eprintln!("[panel] BUG: a {direction:?} nav key for {view:?}, which no page claims");
                return;
            }
        };
        self.drop_bypass_prompt_in(view);
        self.dispatch(view, serialize_nav_key_for_js(direction));
    }

    /// The `?` overlay's rows (`serialize_keymap_for_js`): recorded for every later `ready`, and
    /// sent now to a document that is already up.
    pub fn set_keymap_help(&self, payload: String) {
        self.wake();
        self.state.borrow_mut().keymap_help = Some(payload.clone());
        self.dispatch_every(payload);
    }

    /// `send-prefix`/`send-keys` with `view`'s page holding the keys.
    pub fn literal_key(&self, view: ViewId, key: &eitri_core::keymap::KeySpec) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(
            view,
            eitri_core::agent_bridge::serialize_literal_key_for_js(&key.to_string()),
        );
    }

    /// `prefix ?`: open the `?` overlay in `view`'s page, with the section of the plugin page that held the
    /// keys, if one did. `opening` numbers this opening; the page quotes it back when the overlay closes,
    /// so a close that arrives after a newer opening can be told apart.
    pub fn open_keymap(&self, view: ViewId, plugin: Option<eitri_core::plugin::keys::PluginKeysHelp>, opening: u64) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(
            view,
            eitri_core::agent_bridge::serialize_open_keymap_for_js(plugin.as_ref(), opening),
        );
    }

    /// `prefix :`: open `view`'s page's `:` command line, which runs nothing.
    pub fn open_command_line(&self, view: ViewId) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(view, eitri_core::agent_bridge::serialize_open_command_line_for_js());
    }

    /// Global `f` HINT, in the base page only: an extra view is labelled as a module, its rows are
    /// not. Each is a one-line dispatch of the matching `serialize_hint_*_for_js` envelope;
    /// `shell::hint` drives the session, this handle only relays it to the WebView.
    pub fn hint_collect(&self, session_id: u64) {
        self.wake();
        self.drop_bypass_prompt_in(ViewId::BASE);
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_collect_for_js(session_id),
        );
    }
    pub fn hint_show(&self, session_id: u64, labels: &[String]) {
        self.wake();
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_show_for_js(session_id, labels),
        );
    }
    pub fn hint_prefix(&self, session_id: u64, typed: &str) {
        self.wake();
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_prefix_for_js(session_id, typed),
        );
    }
    pub fn hint_land(&self, session_id: u64, index: usize) {
        self.wake();
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_land_for_js(session_id, index),
        );
    }
    pub fn hint_end(&self, session_id: u64) {
        self.wake();
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_end_for_js(session_id),
        );
    }

    /// Every card answer, bypass `y` and trust answer from a page passes this, with the page's view, after
    /// the page check and before anything is answered; `Err(why)` refuses it and leaves the card or question
    /// waiting, and `why` is what the page shows. The window sets it once.
    pub fn set_answer_gate(&self, gate: Rc<dyn Fn(ViewId) -> Result<(), String>>) {
        self.wake();
        self.state.borrow_mut().answer_gate = Some(gate);
    }

    /// Whether a float covers a chat view's module now, asked with the view whenever the panel decides
    /// if that view's page is on screen for attention (a turn finishing, a tab switch, the page coming
    /// on screen). The window sets it once, and calls [`AgentPanelHandle::page_shown`] when a float
    /// stops covering a view.
    pub fn set_coverage(&self, covered: Rc<dyn Fn(ViewId) -> bool>) {
        self.wake();
        self.state.borrow_mut().coverage = Some(covered);
    }

    /// Where the panel's two HINT messages go. `shell::hint::HintCoordinator` installs this once.
    pub fn on_hint(&self, hook: impl Fn(HintInbound) + 'static) {
        self.wake();
        self.state.borrow_mut().hint_hook = Some(Rc::new(hook));
    }

    /// Where the scratch round trips (`Ctrl+g`, `gf`) hand nvim their request. The hook shows and
    /// focuses the editor, then sends its keys; `Err` says why it could not. `main.rs` installs this once.
    pub fn on_editor_request(
        &self,
        hook: impl Fn(&eitri_core::scratch::ScratchRequest) -> Result<(), String> + 'static,
    ) {
        self.wake();
        self.state.borrow_mut().editor_request_hook = Some(Rc::new(hook));
    }

    /// Whether a tab's trust question is on screen. A read that raises no tick: the shell asks it after each tick.
    pub fn trust_question_open(&self) -> bool {
        let state = self.state.borrow();
        !state.shutting_down && state.trust.question_open()
    }

    /// Shows `text` under `title` in a scratch buffer of the editor, with no page to answer: what a `ViewInEditor`
    /// does after the request. The panel owns the window's one scratch directory, so the window's other parts go
    /// through here; the file written is one small file in a private runtime directory, written on the calling
    /// thread. `Err` says why the editor did not take it.
    pub fn view_in_editor(&self, title: &str, text: &str) -> Result<(), String> {
        self.wake();
        let answer: std::rc::Rc<RefCell<Option<Result<(), String>>>> = Rc::default();
        self.run(|state, out| {
            let request = state
                .scratch
                .as_mut()
                .ok_or_else(|| "the scratch directory could not be created at startup".to_string())?
                .prepare_view(title, text)
                .map_err(|e| format!("could not write the scratch file: {e}"))?;
            out.effect(Effect::AskEditor {
                request,
                then: EditorThen::Report(answer.clone()),
            });
            Ok::<(), String>(())
        })?;
        let answered = answer.borrow_mut().take();
        answered.unwrap_or_else(|| Err("the editor did not answer".to_string()))
    }

    /// The window's way to ask its editor something (the embedded pane, or the companion's link).
    /// `main.rs` and the companion window install this once.
    pub fn set_editor_rpc(&self, rpc: Rc<dyn eitri_core::editor_rpc::EditorRpc>) {
        self.wake();
        self.state.borrow_mut().editor_rpc = Some(rpc);
    }

    /// Who owns the review module in this window's editor, asked before every review request: the
    /// integrated window's own nvim, or the companion panel's channel. `None` from it: no editor.
    pub fn set_review_owner(&self, owner: impl Fn() -> Option<eitri_core::review_editor::Owner> + 'static) {
        self.wake();
        self.state.borrow_mut().review_editor.set_owner(Rc::new(owner));
    }

    /// Called after the editor opened a file for the review: the window brings the editor to the
    /// user (shows and focuses it, or raises the user's own window). `main.rs` and the companion
    /// window install this once.
    pub fn on_review_open(&self, hook: impl Fn() + 'static) {
        self.wake();
        self.state.borrow_mut().review_editor.set_opened(Rc::new(hook));
    }

    /// The editor went away, or its drafts were dropped: what the review overlay installed and
    /// drew there is forgotten, and an open still waiting on it is told it will not be answered.
    pub fn review_editor_lost(&self) {
        self.wake();
        self.run_seen(|state, screen, out| {
            let outs = state.review_flow.editor_lost();
            dispatch_review_outs(state, screen, outs, out);
        });
    }

    /// Where the panel stands with the editor beside it (companion mode), kept so a document that
    /// loads or reloads is told, and sent when it changes. Never called by the one-window mode.
    pub fn set_editor_link(&self, link: eitri_core::companion::attach::BandLink) {
        self.wake();
        let payload = eitri_core::agent_bridge::serialize_editor_link_for_js(link.state, &link.text);
        {
            let mut state = self.state.borrow_mut();
            if state.editor_link.as_ref() == Some(&payload) {
                return;
            }
            state.editor_link = Some(payload.clone());
        }
        self.dispatch_every(payload);
    }

    /// The editor went away or was swapped: every draft edit still out in it ends (a body saved in
    /// nvim becomes the draft). Each tab on screen is told now, in its own page; the others when they
    /// are switched to. Says so once if any edit was cut, and returns how many. It does not return
    /// the keys to the chat: the editor left, so nothing should take the focus.
    pub fn editor_detached(&self) -> usize {
        self.wake();
        let ended = {
            let mut state = self.state.borrow_mut();
            let ended = end_pending_edits(&mut state);
            ended
                .into_iter()
                .map(|(tab, draft)| (tab, draft, state.shown_in(tab)))
                .collect::<Vec<_>>()
        };
        for (tab, draft, shown) in &ended {
            let Some(view) = shown else { continue };
            if let Some(text) = draft {
                self.dispatch(*view, eitri_core::agent_bridge::serialize_draft_for_js(*tab, text));
            }
            self.dispatch(*view, eitri_core::agent_bridge::serialize_scratch_for_js(*tab, false));
        }
        if !ended.is_empty() {
            self.dispatch(
                ViewId::BASE,
                eitri_core::agent_bridge::serialize_notice_for_js("draft editing stopped: the editor went away"),
            );
        }
        ended.len()
    }

    /// One line in the base page's band, in the notice's own style. Used by the companion window for
    /// what it cannot say any other way (a desktop it cannot move focus on).
    pub fn show_notice(&self, text: &str) {
        self.wake();
        self.dispatch(ViewId::BASE, eitri_core::agent_bridge::serialize_notice_for_js(text));
    }

    /// Called when a draft edited in nvim came back (or was discarded), with the view of the edit's
    /// tab: the keys return to that chat view. `main.rs` installs this once.
    pub fn on_editor_done(&self, hook: impl Fn(ViewId) + 'static) {
        self.wake();
        self.state.borrow_mut().editor_done_hook = Some(Rc::new(hook));
    }

    /// Called with the value before and after whenever [`AgentPanelHandle::attention`] changes, or
    /// one tab's own attention does ([`AgentPanelHandle::attention_of`] for its view) -- the one place
    /// that knows both, so `main.rs` keeps no copy of its own to drift -- with the views handed a card
    /// since the last call, the view holding the newest of them last. `main.rs` installs this once.
    pub fn on_attention(
        &self,
        hook: impl Fn(eitri_core::attention::Attention, eitri_core::attention::Attention, &[ViewId]) + 'static,
    ) {
        self.wake();
        self.state.borrow_mut().attention_hook = Some(Rc::new(hook));
    }

    /// Called for views whose running state or queue changed, and with `false` for views that left.
    pub fn on_busy(&self, hook: impl Fn(&[(ViewId, bool)]) + 'static) {
        self.wake();
        self.state.borrow_mut().busy_hook = Some(Rc::new(hook));
    }

    /// Where a `tab_verb` message's mapped `TabAction` goes: the same code the prefix's own
    /// `Action::Tab` arm runs (`main.rs`'s `run_tab_action`). `None`
    /// until `main.rs` installs one, which is why `InboundMessage::TabVerb` can be refused.
    /// The hook is told the view of the page the verb came from. `main.rs` installs this once.
    pub fn on_tab_verb(&self, hook: impl Fn(ViewId, TabAction) + 'static) {
        self.wake();
        self.state.borrow_mut().tab_verb_hook = Some(Rc::new(hook));
    }

    /// Where a `nav_fallthrough` message's direction goes: `main.rs` wires this to
    /// `move_focus(&ModuleId::agent(), dir)`, i.e. exactly what the chord would have done had the
    /// panel never intercepted it. A `pane_nav` (`Ctrl+w h/j/k/l`) goes through the
    /// same hook, for it is the same move. The hook is told the view of the page the move came from, the
    /// module it moves from. `main.rs` installs this once.
    pub fn on_nav_fallthrough(&self, hook: impl Fn(ViewId, Direction) + 'static) {
        self.wake();
        self.state.borrow_mut().nav_fallthrough_hook = Some(Rc::new(hook));
    }

    /// Where a `keymap_closed` message goes: the opening it closes, and whether the overlay's own keys (or a
    /// click outside it) closed it. `main.rs` installs this once; without it the message does nothing.
    pub fn on_keymap_closed(&self, hook: impl Fn(u64, bool) + 'static) {
        self.wake();
        self.state.borrow_mut().keymap_closed_hook = Some(Rc::new(hook));
    }
}

/// The session-tab verbs `main.rs` binds. Each borrows, changes
/// the set, drops the borrow, then dispatches. Every one of them does nothing once the window is
/// closing (`AgentPanelState::shutting_down`: the set is empty then). A verb about "the tab on
/// screen" names the view whose page has the keys and acts on that page's tab.
impl AgentPanelHandle {
    /// The window is closing (`AgentPanelState::shutting_down`), or there are no pages at all
    /// (`with_pages`'s own doc): every verb below then does nothing, and says so to its caller the way
    /// it does for a closing window.
    pub(super) fn closing(&self) -> bool {
        !self.with_pages.get() || self.state.borrow().shutting_down
    }

    /// `prefix c`: opens a tab running the window's default agent (`agent.default`) in the base view,
    /// selects it, and sends `tabs` -- nothing else: an empty tab has no state yet.
    pub fn new_tab(&self) {
        self.wake();
        let agent = self.state.borrow().default_agent.clone();
        self.new_tab_for(agent);
    }

    /// [`new_tab`](Self::new_tab) for a named agent (`:tabnew <agent>`).
    pub fn new_tab_for(&self, agent: AgentId) {
        self.wake();
        if self.closing() {
            return;
        }
        self.run(|state, out| {
            state.tabs.open(agent);
            send_tabs(state, out);
        });
    }

    /// `prefix <digit>`: a base-view tab is selected there; a tab bound to an extra view asks the window
    /// to show that view (`on_show_view`). Where the tab is, or `None` when no tab has that number (the
    /// caller flashes).
    pub fn select_number(&self, n: u16) -> Option<TabPlace> {
        self.wake();
        if self.closing() {
            return None;
        }
        self.run_seen(|state, screen, out| {
            let place = state.tabs.select_number(n)?;
            match place {
                TabPlace::Base(_) => send_switch(state, screen, out),
                TabPlace::View(view, _) => out.effect(Effect::ShowView(view)),
            }
            Some(place)
        })
    }

    /// Whether stepping the base tabs can select a tab, without sending anything to a page.
    pub fn can_step(&self, delta: i32) -> bool {
        if !self.with_pages.get() {
            return false;
        }
        let Ok(state) = self.state.try_borrow() else {
            return false;
        };
        if state.shutting_down {
            return false;
        }
        let tabs = &state.tabs;
        let numbers: Vec<u16> = tabs
            .tabs()
            .iter()
            .filter(|tab| tab.view_id().is_base())
            .map(|tab| tab.number)
            .collect();
        eitri_core::tabs::step(&numbers, tabs.active_tab().number, delta).is_some()
    }

    /// Whether the previous base tab can be selected, without sending anything to a page.
    pub fn has_last(&self) -> bool {
        if !self.with_pages.get() {
            return false;
        }
        let Ok(state) = self.state.try_borrow() else {
            return false;
        };
        !state.shutting_down && state.tabs.has_last()
    }

    /// Where a numbered tab would be selected, without sending anything to a page.
    pub fn tab_place(&self, n: u16) -> Option<TabPlace> {
        if !self.with_pages.get() {
            return None;
        }
        let state = self.state.try_borrow().ok()?;
        if state.shutting_down {
            return None;
        }
        state
            .tabs
            .tabs()
            .iter()
            .find(|tab| tab.number == n)
            .and_then(|tab| state.tabs.place_of(tab.id))
    }

    /// `prefix n` (+1) / `p` (-1), wrapping, over the base view's tabs.
    pub fn step(&self, delta: i32) -> bool {
        self.wake();
        self.switch_with(|tabs| tabs.step(delta))
    }

    /// `prefix l`: the base view's previously active tab.
    pub fn select_last(&self) -> bool {
        self.wake();
        self.switch_with(eitri_core::tab_set::TabSet::select_last)
    }

    pub(super) fn switch_with(&self, select: impl FnOnce(&mut eitri_core::tab_set::TabSet) -> Option<TabId>) -> bool {
        if self.closing() {
            return false;
        }
        self.run_seen(|state, screen, out| {
            if select(&mut state.tabs).is_none() {
                return false;
            }
            send_switch(state, screen, out);
            true
        })
    }

    /// `prefix ,`: the inline rename field on the label of the tab `view`'s page shows.
    pub fn begin_rename(&self, view: ViewId) {
        self.wake();
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt_in(view);
        let payload = {
            let state = self.state.borrow();
            let Some(tab) = state.tab_of_page(view).and_then(|tab| state.tabs.get(tab)) else {
                return;
            };
            eitri_core::agent_bridge::serialize_begin_rename_for_js(tab.id, tab.name.as_deref())
        };
        self.dispatch(view, payload);
    }

    /// `prefix &`: the footer's y/n prompt for the tab `view`'s page shows.
    pub fn confirm_close(&self, view: ViewId) {
        self.wake();
        if self.closing() {
            return;
        }
        // The panel's close prompt replaces a bypass prompt on screen; Rust's copy goes with it.
        self.drop_bypass_prompt_in(view);
        let payload = {
            let state = self.state.borrow();
            let Some((tab, facts)) = state
                .tab_of_page(view)
                .and_then(|tab| state.tabs.close_facts(tab).map(|facts| (tab, facts)))
            else {
                return;
            };
            eitri_core::agent_bridge::serialize_confirm_close_for_js(tab, &eitri_core::tabs::close_prompt(&facts))
        };
        self.dispatch(view, payload);
    }

    /// `<leader>bo` in the base view: the footer's y/n prompt over every base-view tab but the active
    /// one. Nothing to dispatch, and no flash, when the active tab is the only one -- `<leader>bo`
    /// with one tab is simply nothing to do, the same as `prefix n`/`p` with one tab (unlike a verb
    /// naming a tab that does not exist, which flashes).
    pub fn confirm_close_others(&self) {
        self.wake();
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt_in(ViewId::BASE);
        let Some((tabs, prompt)) = self.state.borrow().tabs.close_others_plan() else {
            return;
        };
        let payload = eitri_core::agent_bridge::serialize_confirm_close_others_for_js(&tabs, &[prompt]);
        self.dispatch(ViewId::BASE, payload);
    }

    pub fn set_partner(&self, partner: Option<Partner>) {
        self.wake();
        let mut state = self.partner.borrow_mut();
        state.partner = partner;
        state.generation += 1;
        state.sessions.clear();
        state.pending = None;
        state.pending_tab = None;
    }

    pub fn on_chooser_partner_pick(&self, pick: impl Fn(ViewId, String) + 'static) {
        self.wake();
        self.partner.borrow_mut().pick = Some(Rc::new(pick));
    }

    pub fn chat_summary(&self) -> eitri_core::wm::ChatSummary {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return Default::default();
        }
        let attention = state.tabs.attention();
        eitri_core::wm::ChatSummary {
            waiting: attention.pending as u32,
            unseen: state
                .tabs
                .tabs()
                .iter()
                .filter(|t| t.attention.attention().unread)
                .count() as u32,
            running: state.tabs.running_count() as u32,
            queued: state.tabs.queued_count() as u32,
        }
    }

    fn poll_partner_chooser(&self) {
        let answer = self.partner.borrow().pending.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some((generation, view, payload, sessions)) = answer {
            if generation != self.partner.borrow().generation {
                return;
            }
            let tab = self.state.borrow().tab_of_page(view);
            let mut state = self.partner.borrow_mut();
            state.pending = None;
            if state.pending_tab.take() != tab {
                state.sessions.clear();
                return;
            }
            state.sessions = sessions;
            drop(state);
            self.dispatch(view, payload);
        }
    }

    /// `prefix w` in `view`'s page: every open tab, and the records of that page's tab's agent.
    pub fn open_chooser(&self, view: ViewId) {
        self.wake();
        if self.closing() {
            return;
        }
        // Opening the chooser is one of the routes that cancels an outstanding bypass
        // prompt in this page -- it was about the tab the user is leaving to see this list. A fresh
        // one raised from inside the chooser (its own Shift+Tab) makes its own plan afterwards.
        self.drop_bypass_prompt_in(view);
        let (payload, listed) = chooser_payload(&self.state.borrow(), view);
        {
            let mut state = self.state.borrow_mut();
            state.chooser_listed = listed;
            state.chooser_view = view;
        }
        self.partner.borrow_mut().sessions.clear();
        #[cfg(target_os = "linux")]
        let partner = self.partner.borrow().partner;
        #[cfg(target_os = "linux")]
        if let Some(partner) = partner {
            let generation = self.partner.borrow().generation + 1;
            let mut value: serde_json::Value = serde_json::from_str(&payload).expect("a serialized chooser");
            let state = self.state.borrow();
            let tab = state.tab_of_page(view);
            let agent = tab
                .and_then(|id| state.tabs.get(id))
                .map(|t| t.agent().record_name())
                .unwrap_or_else(|| "claude".into());
            let cwd = state.canonical_project_dir.clone();
            drop(state);
            let (tx, rx) = std::sync::mpsc::channel();
            let wake = self.raise.clone();
            std::thread::spawn(move || {
                let mut records: Vec<eitri_core::agent_bridge::ChooserRecord> =
                    serde_json::from_value(value["records"].take()).unwrap_or_default();
                mark_partner_records(&mut records, Some(partner.window), |session| {
                    agent::lease::SessionLease::is_held_by(&agent, &cwd, session, partner.pid, partner.start)
                });
                let sessions = records
                    .iter()
                    .filter(|r| r.held_by_partner)
                    .map(|r| r.provider_session_id.clone())
                    .collect();
                value["records"] = serde_json::to_value(records).expect("chooser records serialize");
                let _ = tx.send((generation, view, value.to_string(), sessions));
                if let Some(wake) = wake {
                    wake();
                }
            });
            let mut state = self.partner.borrow_mut();
            state.generation = generation;
            state.pending = Some(rx);
            state.pending_tab = tab;
            return;
        }
        self.dispatch(view, payload);
    }

    /// `prefix i`: the detail popover of the tab `view`'s page shows.
    pub fn open_detail(&self, view: ViewId) {
        self.wake();
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt_in(view);
        let payload = {
            let state = self.state.borrow();
            let Some(tab) = state.tab_of_page(view) else { return };
            detail_payload(&state, tab)
        };
        self.dispatch(view, payload);
    }

    /// The tray chip, `prefix a`: the oldest pending card among `view`'s tabs, with `view`'s page's
    /// cursor put on it. The base view switches to the tab holding it; an extra view shows its one
    /// tab already. `false` when none of `view`'s tabs holds a card.
    pub fn focus_oldest_card(&self, view: ViewId) -> bool {
        self.wake();
        if self.closing() {
            return false;
        }
        let target = self.run_seen(|state, screen, out| {
            let target = oldest_card_in(&state.tabs, view)?;
            if view.is_base() {
                state.tabs.select(target);
            }
            // A landing on a card is a route away even when it names the tab already on
            // screen, which `select` alone does not treat as a move.
            state.tabs.drop_bypass_prompt_in(view);
            if view.is_base() {
                send_switch(state, screen, out);
            }
            Some(target)
        });
        let Some(target) = target else {
            return false;
        };
        self.dispatch(
            view,
            eitri_core::agent_bridge::serialize_focus_permission_for_js(target),
        );
        true
    }

    /// The view whose tab holds the window's oldest pending card.
    pub fn oldest_card_view(&self) -> Option<ViewId> {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        state.tabs.oldest_card_place().map(|place| match place {
            TabPlace::Base(_) => ViewId::BASE,
            TabPlace::View(view, _) => view,
        })
    }

    /// What `view`'s tabs owe the user: the base view's summed, or an extra view's one tab's.
    pub fn attention_of(&self, view: ViewId) -> eitri_core::attention::Attention {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return Default::default();
        }
        state.tabs.attention_of(view)
    }

    /// What the chat owes the user now, summed over every tab: its pending permission
    /// cards, and whether a turn finished while it was off screen.
    #[cfg(test)]
    pub(super) fn attention(&self) -> eitri_core::attention::Attention {
        let state = self.state.borrow();
        if state.shutting_down {
            return Default::default();
        }
        state.tabs.attention()
    }

    /// The tab holding the newest pending card, as `(number, label name)`: the toast names it.
    #[cfg(test)]
    pub(super) fn newest_card_label(&self) -> Option<(u16, String)> {
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        let id = state.tabs.newest_card_tab()?;
        state.tabs.get(id).map(|t| (t.number, t.label_name()))
    }

    /// The tab holding the newest pending card among `view`'s tabs, as `(number, label name)`.
    pub fn newest_card_label_in(&self, view: ViewId) -> Option<(u16, String)> {
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        let id = state.tabs.newest_card_tab_in(view)?;
        state.tabs.get(id).map(|t| (t.number, t.label_name()))
    }

    /// `prefix x` on a chat view (2026-09-26): every tab of `view` is closed exactly as `y` to
    /// `prefix &` closes one (`close_tab`: a running turn interrupted, queued words kept as history,
    /// the backend shut down and dropped on a worker the window-close backstop still covers, the
    /// record never deleted, so each session stays resumable). The base view is left with one fresh
    /// empty tab, as closing its last tab always leaves it, and every extra view's tab keeps running;
    /// an extra view loses its one tab. How many tabs were closed.
    ///
    /// Refused, closing nothing, while one of them is being handed off to a terminal: `close_tab`
    /// refuses that tab, and a kill that closed the others and left it would not be a kill.
    pub fn close_view_tabs(&self, view: ViewId) -> Result<usize, &'static str> {
        self.wake();
        if self.closing() {
            return Ok(0);
        }
        let ids: Vec<TabId> = {
            let state = self.state.borrow();
            // Checked for every tab first, so a refusal closes none of them.
            if let Some(why) = tabs_cannot_close(&state, view) {
                return Err(why);
            }
            view_tabs(&state, view)
        };
        self.run(|state, out| {
            for id in &ids {
                close_tab(state, *id, out)?;
            }
            eprintln!(
                "[agent_panel] killed: {} session tab(s) closed, records kept (view {})",
                ids.len(),
                view.number()
            );
            tell_emptied_views(state, out);
            Ok(ids.len())
        })
    }

    /// Why `view`'s tabs cannot close now, in the words [`Self::close_view_tabs`] refuses with: one is being handed
    /// off to a terminal, or moving to the other window. `None` when they can, as a kill of the view asks first.
    pub fn tabs_cannot_close(&self, view: ViewId) -> Option<&'static str> {
        self.wake();
        tabs_cannot_close(&self.state.borrow(), view)
    }

    /// Whether `prefix !` from the base view can break a tab out now, asked before a new view is made for it: the
    /// base view keeps a tab of its own, so it needs two. The refusal's words when it cannot.
    pub fn can_break_out(&self) -> Result<(), String> {
        self.wake();
        if self.closing() {
            return Err("the window is closing".to_string());
        }
        if self.state.borrow().tabs.base_tabs().len() < 2 {
            return Err(eitri_core::tab_set::ViewError::OneTabLeft.to_string());
        }
        Ok(())
    }

    /// Whether [`Self::open_in`] can give the extra view `view` a new empty tab now, asked before a new view is
    /// placed for it. The refusal's words when it cannot.
    pub fn can_open_in(&self, view: ViewId) -> Result<(), String> {
        self.wake();
        if self.closing() {
            return Err("the window is closing".to_string());
        }
        if view.is_base() {
            return Err(eitri_core::tab_set::ViewError::NotAnExtraView.to_string());
        }
        if self.state.borrow().tabs.tab_in(view).is_some() {
            return Err(eitri_core::tab_set::ViewError::Taken(view).to_string());
        }
        Ok(())
    }

    /// `prefix !` (tmux's `break-pane`) with the keys in the base view: its active tab moves to the
    /// empty extra view `view`, and the base page shows the tab it falls back to. The moved tab.
    pub fn break_out(&self, view: ViewId) -> Result<TabId, String> {
        self.wake();
        if self.closing() {
            return Err("the window is closing".to_string());
        }
        self.run_seen(|state, screen, out| {
            let moved = state.tabs.break_out(view).map_err(|e| e.to_string())?;
            send_switch(state, screen, out);
            send_view(state, screen, view, out);
            send_moved_hint(state, moved, view, out);
            Ok(moved)
        })
    }

    /// The tab `view` shows comes back into the base view as its active tab; `view` is left empty,
    /// which the window is told (`on_view_empty`). The tab that came back.
    pub fn join_back(&self, view: ViewId) -> Result<TabId, String> {
        self.wake();
        if self.closing() {
            return Err("the window is closing".to_string());
        }
        self.run_seen(|state, screen, out| {
            let tab = state
                .tabs
                .join_back(view)
                .ok_or_else(|| format!("agent {} shows no tab", view.number()))?;
            send_switch(state, screen, out);
            send_moved_hint(state, tab, ViewId::BASE, out);
            state.emptied_views.push(view);
            tell_emptied_views(state, out);
            Ok(tab)
        })
    }

    /// A new chat view's tab: a new empty tab of the window's default agent, bound to the empty extra
    /// view `view`; the base view's active tab is unchanged. The new tab.
    pub fn open_in(&self, view: ViewId) -> Result<TabId, String> {
        self.wake();
        if self.closing() {
            return Err("the window is closing".to_string());
        }
        self.run_seen(|state, screen, out| {
            let agent = state.default_agent.clone();
            let tab = state.tabs.open_in(agent, view).map_err(|e| e.to_string())?;
            send_view(state, screen, view, out);
            Ok(tab)
        })
    }

    /// The extra views that hold a tab, ascending.
    pub fn views(&self) -> Vec<ViewId> {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return Vec::new();
        }
        state.tabs.views()
    }

    /// The label of the tab the extra view `view` shows, as its strip names it (`<n> <name>`): what
    /// `prefix x` on that view says it closes. `None` when `view` holds no tab, and for the base view,
    /// whose kill closes every base-view tab.
    pub fn view_tab_label(&self, view: ViewId) -> Option<String> {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        let tab = state.tabs.tab_in(view).and_then(|tab| state.tabs.get(tab))?;
        Some(eitri_core::tabs::label(tab.number, &tab.label_name()))
    }

    /// What closing the extra view `view`'s tab would end, as the window-close prompt counts it: whether
    /// its turn is running or its session still connecting (0 or 1), and its queued messages. `(0, 0)`
    /// when `view` holds no tab, and for the base view.
    pub fn view_running_queued(&self, view: ViewId) -> (usize, usize) {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return (0, 0);
        }
        let Some(tab) = state.tabs.tab_in(view).and_then(|tab| state.tabs.get(tab)) else {
            return (0, 0);
        };
        let running = matches!(tab.backend(), TabBackend::Starting(_)) || tab.turn_running() || tab.question_waiting();
        (usize::from(running), tab.session.queue.len())
    }

    /// Whether a tab of this window holds the agent session `session` (live, starting or waiting on trust): a
    /// session the other window of a linked pair may not name as one of its own.
    pub fn holds_session(&self, session: &str) -> bool {
        self.wake();
        let state = self.state.borrow();
        state
            .tabs
            .tabs()
            .iter()
            .any(|tab| tab.provider_session_id().as_deref() == Some(session))
    }

    /// A draft edit that the chat holding `session` brought from the other window of its pair ended in this
    /// window's editor: the tab takes it as it takes an edit of its own (the new draft, or the failure's reason),
    /// and the keys return to its chat. `false` when no tab here holds that session with an edit out.
    pub fn finish_moved_draft_edit(&self, session: &str, result: &eitri_core::scratch::EditDone) -> bool {
        self.wake();
        self.run(|state, out| super::review_glue::finish_moved_scratch_edit(state, session, result, out))
    }

    /// The tab the draft edit `edit` is out for, with the agent session that tab holds: what a window whose
    /// editor is in the other window of its pair names when it hands the draft over.
    pub fn draft_edit_tab(&self, edit: u64) -> Option<(TabId, Option<String>)> {
        self.wake();
        let state = self.state.borrow();
        let tab = state.tabs.tabs().iter().find(|tab| tab.editing_draft == Some(edit))?;
        Some((tab.id, tab.provider_session_id()))
    }

    /// The extra chat views the window's layout holds, in any workspace, told before any restore so a
    /// restored tab saved in one of them comes back into it.
    pub fn set_layout_views(&self, views: Vec<ViewId>) {
        self.wake();
        self.state.borrow_mut().tabs.set_layout_views(views);
    }

    /// Whether the last window's tabs are settled: none on offer, no restore question open and no
    /// restore run in flight. Until then an extra view holding no tab may still be given one.
    #[cfg(test)]
    pub(super) fn restore_settled(&self) -> bool {
        self.wake();
        restore_settled(&self.state.borrow())
    }

    /// Told once per extra view left holding no tab: its tab closed, joined back or a failed restore
    /// discarded it, and, once the restore has settled, each attached extra view that never got one.
    /// The window removes the view. `main.rs` installs this once.
    pub fn on_view_empty(&self, hook: impl Fn(ViewId) + 'static) {
        self.wake();
        self.state.borrow_mut().view_empty_hook = Some(Rc::new(hook));
    }

    /// Told when a pick asks the window to show a view and give it the keys: a chooser pick, a number
    /// key or a resume naming a tab bound to an extra view, or a resume from an extra page that opened
    /// in a new base-view tab. `main.rs` installs this once.
    pub fn on_show_view(&self, hook: impl Fn(ViewId) + 'static) {
        self.wake();
        self.state.borrow_mut().show_view_hook = Some(Rc::new(hook));
    }

    /// Tabs with a running turn, a session still connecting or a waiting question.
    pub fn running_count(&self) -> usize {
        let state = self.state.borrow();
        if state.shutting_down {
            return 0;
        }
        state.tabs.running_count()
    }

    /// Tabs with live non-ambient background work across the window.
    pub fn background_count(&self) -> usize {
        let state = self.state.borrow();
        if state.shutting_down {
            return 0;
        }
        state.tabs.background_count()
    }

    /// Background tabs a kill of this view closes, including every base-view tab for the base view.
    pub fn view_background_count(&self, view: ViewId) -> usize {
        let state = self.state.borrow();
        if state.shutting_down {
            return 0;
        }
        view_tabs(&state, view)
            .iter()
            .filter(|id| state.tabs.get(**id).is_some_and(|tab| tab.background_calls() > 0))
            .count()
    }

    /// Every tab's queued messages, summed (the window-close prompt).
    pub fn queued_count(&self) -> usize {
        let state = self.state.borrow();
        if state.shutting_down {
            return 0;
        }
        state.tabs.queued_count()
    }
}

/// The tabs a kill of `view` closes: every base-view tab for the base view, else the one tab it shows.
fn view_tabs(state: &AgentPanelState, view: ViewId) -> Vec<TabId> {
    if view.is_base() {
        state.tabs.base_tabs()
    } else {
        state.tabs.tab_in(view).into_iter().collect()
    }
}

/// Why `view`'s tabs cannot close now: one is being handed off to a terminal, or moving to the other window.
fn tabs_cannot_close(state: &AgentPanelState, view: ViewId) -> Option<&'static str> {
    let ids = view_tabs(state, view);
    if ids
        .iter()
        .any(|id| state.tabs.get(*id).is_some_and(|t| t.pending_handoff.is_some()))
    {
        return Some("a tab is still being handed off to a terminal; kill the chat once that finishes");
    }
    if ids.iter().any(|id| state.tabs.is_moving(*id)) {
        return Some("a tab is moving to the other window; kill the chat once that finishes");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    #[test]
    fn view_in_editor_refuses_without_a_scratch_directory() {
        let (handle, _) = settled_panel();
        handle.state.borrow_mut().scratch = None;
        let asked = Rc::new(RefCell::new(0));
        let seen = asked.clone();
        handle.on_editor_request(move |_| {
            *seen.borrow_mut() += 1;
            Ok(())
        });
        let why = handle.view_in_editor("init.lua lines", "x").unwrap_err();
        assert!(why.contains("scratch directory"), "{why}");
        assert_eq!(*asked.borrow(), 0, "the editor was not asked");
        handle.state.borrow_mut().scratch = Some(eitri_core::scratch::ScratchDir::new().unwrap());
        handle.view_in_editor("init.lua lines", "x").unwrap();
        assert_eq!(*asked.borrow(), 1);
    }

    #[test]
    fn a_chooser_partner_pick_raises_without_starting_a_session() {
        let (handle, _) = settled_panel();
        let picked = Rc::new(RefCell::new(Vec::new()));
        let seen = picked.clone();
        handle.on_chooser_partner_pick(move |view, session| seen.borrow_mut().push((view, session)));
        handle.set_partner(Some((7, 11, 2).into()));
        handle.partner.borrow_mut().sessions.push("peer-session".into());
        let tabs = handle.state.borrow().tabs.tabs().len();
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"resume","request_id":"pick","provider_session_id":"peer-session"}"#,
        );
        assert_eq!(*picked.borrow(), vec![(ViewId::BASE, "peer-session".into())]);
        assert_eq!(handle.state.borrow().tabs.tabs().len(), tabs);
        handle.set_partner(None);
        assert!(handle.partner.borrow().sessions.is_empty());
    }

    #[cfg(target_os = "linux")]
    fn wait_for_partner_chooser(handle: &AgentPanelHandle) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while handle.partner.borrow().pending.is_some() {
            handle.poll_partner_chooser();
            assert!(std::time::Instant::now() < deadline, "the chooser answer never came");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_partner_chooser_answer_after_a_tab_switch_is_dropped() {
        let (handle, page) = settled_panel();
        handle.set_partner(Some((7, 11, 2).into()));
        handle.new_tab_for(AgentId::Codex);
        assert!(handle.step(-1));
        let before = page.sent.borrow().len();
        handle.open_chooser(ViewId::BASE);
        assert!(handle.step(1));
        wait_for_partner_chooser(&handle);
        assert!(handle.partner.borrow().sessions.is_empty());
        assert!(!page.sent.borrow()[before..]
            .iter()
            .any(|payload| kind_of(payload) == "chooser"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_partner_chooser_answer_for_the_same_tab_is_dispatched_once() {
        let (handle, page) = settled_panel();
        handle.set_partner(Some((7, 11, 2).into()));
        let before = page.sent.borrow().len();
        handle.open_chooser(ViewId::BASE);
        wait_for_partner_chooser(&handle);
        handle.poll_partner_chooser();
        assert_eq!(
            page.sent.borrow()[before..]
                .iter()
                .filter(|payload| kind_of(payload) == "chooser")
                .count(),
            1
        );
    }

    /// A settled panel with three base tabs, the third active, and a fourth bound to view 2: `last` has a base
    /// tab to go back to, and one number names a tab in a view.
    fn mixed_tabs() -> (AgentPanelHandle, std::rc::Rc<RecordingPage>) {
        let (handle, page) = settled_panel();
        handle.new_tab();
        handle.new_tab();
        handle.open_in(ViewId::new(2).unwrap()).unwrap();
        (handle, page)
    }

    #[test]
    fn the_read_only_tab_queries_answer_what_the_calls_would_do() {
        // Each query is asked of one set and the acting call made on an identical one, so the call cannot have
        // seen what the query left behind.
        for twin in [settled_panel, mixed_tabs] {
            for delta in [1, -1, 2] {
                let (asked, page) = twin();
                let sent = page.sent.borrow().len();
                let answer = asked.can_step(delta);
                assert_eq!(
                    page.sent.borrow().len(),
                    sent,
                    "can_step({delta}) sends no page message"
                );
                assert_eq!(answer, twin().0.step(delta), "can_step({delta}) answers what step does");
            }
            let (asked, page) = twin();
            let sent = page.sent.borrow().len();
            let answer = asked.has_last();
            assert_eq!(page.sent.borrow().len(), sent, "has_last sends no page message");
            assert_eq!(answer, twin().0.select_last(), "has_last answers what select_last does");
            for number in 0..=5 {
                let (asked, page) = twin();
                let sent = page.sent.borrow().len();
                let answer = asked.tab_place(number);
                assert_eq!(
                    page.sent.borrow().len(),
                    sent,
                    "tab_place({number}) sends no page message"
                );
                assert_eq!(
                    answer,
                    twin().0.select_number(number),
                    "tab_place({number}) answers what select_number does"
                );
            }
        }
        let (handle, _) = mixed_tabs();
        assert!(handle.has_last(), "the set has a base tab to go back to");
        assert!(handle.can_step(1), "the set has another base tab to step to");
        assert!(
            matches!(handle.tab_place(4), Some(TabPlace::View(..))),
            "one number names the tab in view 2"
        );
    }

    /// The window controller asks a chat page whether it keeps a navigation key while it decides that key: the
    /// question reads the page's mode and changes nothing, sends the page nothing, and only the key itself, once
    /// decided, reaches the page, as one message.
    #[test]
    fn the_chat_intercept_query_mutates_nothing() {
        let (handle, page) = settled_panel();
        let directions = [Direction::Left, Direction::Down, Direction::Up, Direction::Right];
        for (mode, kept) in [
            (PanelKeys::Browse, vec![Direction::Down]),
            (PanelKeys::Input, vec![Direction::Up]),
            (PanelKeys::Other, vec![]),
        ] {
            handle.state.borrow().page(ViewId::BASE).unwrap().nav_mode.set(mode);
            let sent = page.sent.borrow().len();
            let claimed: Vec<Direction> = directions
                .into_iter()
                .filter(|direction| handle.nav_claims(ViewId::BASE, *direction))
                .collect();
            assert_eq!(claimed, kept, "{mode:?}");
            assert_eq!(
                page.sent.borrow().len(),
                sent,
                "asking sent the page nothing in {mode:?}"
            );
            assert_eq!(
                handle.panel_keys(ViewId::BASE),
                mode,
                "asking left the mirror as it was"
            );
        }
        let sent = page.sent.borrow().len();
        handle.nav_key(ViewId::BASE, Direction::Down);
        let after: Vec<String> = page.sent.borrow()[sent..].to_vec();
        assert_eq!(after.len(), 1, "the key is one message: {after:?}");
        assert_eq!(kind_of(&after[0]), "nav_key");
    }

    /// Every `y` in the panel (a row, a code block, the handoff command, the `prefix i` popover's
    /// line) calls `navigator.clipboard.writeText`, and none of them can reach the clipboard.
    /// `load_html(.., None)` gives the document an opaque origin, which is not
    /// a secure context, so WebKitGTK 2.52.6 has no `navigator.clipboard` and the optional chain
    /// did nothing. Measured in the sandbox with a bare WebKitGTK view: base `None`, nothing copied;
    /// base `https://eitri.invalid/`, `wl-paste` printed the text. Every `load_html` here must
    /// pass the secure base, and the base must stay one that never resolves (RFC 2606 `.invalid`):
    /// a relative link is still a `LinkClicked` the navigation guard hands to the browser. The host's one
    /// `load_html` passes on the base it is given; `shell` checks that half
    /// (`the_webkit_page_hands_load_html_the_base_it_is_given`).
    #[test]
    fn the_panel_document_is_loaded_in_a_secure_context_that_never_resolves() {
        assert!(PANEL_BASE_URI.starts_with("https://"), "{PANEL_BASE_URI}");
        let host = PANEL_BASE_URI.trim_start_matches("https://").trim_end_matches('/');
        assert!(host.ends_with(".invalid"), "{host}");
        every_load_names_the_secure_base(&panel_sources());
    }

    /// Every document load the panel asks for, in any of its files -- the first, a reload and the give-up
    /// page -- names the secure base. `PanelPage`, which hands its surface the base it was given, is the
    /// page layer and asks for no load of its own.
    fn every_load_names_the_secure_base(sources: &[eitri_core::source_scan::Source]) {
        use eitri_core::source_scan as scan;
        let code = production_code(sources);
        let code = scan::without(&code, &impl_blocks(&code, "implPanelPage"));
        let loads: Vec<&str> = code
            .match_indices(".load_document(")
            .map(|(i, _)| {
                let rest = &code[i..];
                &rest[..rest.find(';').unwrap_or(rest.len())]
            })
            .collect();
        for load in &loads {
            assert!(
                load.contains("PANEL_BASE_URI"),
                "a load_document without the secure base: {load}"
            );
        }
        assert!(loads.len() >= 3, "{loads:?}");
    }

    #[test]
    fn a_load_without_the_secure_base_in_a_second_file_fails_the_scan() {
        let read = fixture(&[
            (
                "panel/src/a.rs",
                "fn first(page: &P) { page.load_document(d, PANEL_BASE_URI); page.load_document(e, PANEL_BASE_URI); }\n\
                 impl PanelPage { fn load_document(&self, html: &str, base: &str) { self.surface.load_document(html, base); } }",
            ),
            ("panel/src/b.rs", "fn reload(page: &P) { page.load_document(d, OTHER_BASE); }"),
        ]);
        let why = scan_failure(|| every_load_names_the_secure_base(&read)).expect("the second file's load is read");
        assert!(why.contains("OTHER_BASE"), "{why}");
        let clean = fixture(&[
            (
                "panel/src/a.rs",
                "fn first(page: &P) { page.load_document(d, PANEL_BASE_URI); }",
            ),
            (
                "panel/src/b.rs",
                "fn reload(page: &P) { page.load_document(d, PANEL_BASE_URI); page.load_document(e, PANEL_BASE_URI); }",
            ),
        ]);
        assert_eq!(scan_failure(|| every_load_names_the_secure_base(&clean)), None);
    }

    /// A new tab of an agent that offers no manual mode, under a manual window default, starts in
    /// auto, and the page is told so once, in its transient slot, after the `tabs` that shows it.
    #[test]
    fn a_new_acp_tab_under_a_manual_default_is_announced_once() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual));
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let notices = || {
            page.sent
                .borrow()
                .iter()
                .filter(|e| kind_of(e) == "notice")
                .map(|e| {
                    serde_json::from_str::<serde_json::Value>(e).unwrap()["text"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect::<Vec<_>>()
        };
        assert!(notices().is_empty(), "{:?}", notices());
        handle.new_tab_for(AgentId::Codex);
        assert!(notices().is_empty(), "Codex offers manual: {:?}", notices());
        handle.new_tab_for(AgentId::Acp("gem".into()));
        let said = notices();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("gem tabs have no manual mode"), "{said:?}");
        let sent = page.sent.borrow().clone();
        let last_tabs = sent.iter().rposition(|e| kind_of(e) == "tabs").unwrap();
        let notice = sent.iter().rposition(|e| kind_of(e) == "notice").unwrap();
        assert!(last_tabs < notice, "the tab is on screen before it is spoken of");
        assert_eq!(state.borrow().tabs.active_tab().mode(), SessionModeChoice::Auto);
        handle.new_tab_for(AgentId::Claude);
        assert_eq!(notices().len(), 1, "once");
        assert_eq!(state.borrow().tabs.active_tab().mode(), SessionModeChoice::Manual);
    }

    #[test]
    fn arriving_base_tabs_replace_an_untouched_empty_tab() {
        let source = AgentPanelHandle::for_host_test(Rc::new(RecordingPage::default()));
        let dest = AgentPanelHandle::for_host_test(Rc::new(RecordingPage::default()));
        let records: Vec<_> = (0..2)
            .map(|_| {
                let tab = source.state.borrow().tabs.active();
                let panel = source.panel_tab_handoff(tab).unwrap();
                let frozen = source.state.borrow_mut().tabs.freeze_for_move(tab).unwrap();
                source.state.borrow_mut().tabs.unfreeze(tab);
                serde_json::json!({"version": 1, "tab": frozen.tab, "chat": null, "provider": null,
                "lease": false, "panel": panel.to_value(), "view": null})
            })
            .collect();
        let a = dest.adopt_moved(ViewId::BASE, &records[0], vec![]).unwrap();
        let b = dest.adopt_moved(ViewId::BASE, &records[1], vec![]).unwrap();
        dest.order_base_tabs(&[b, a], Some(a)).unwrap();
        assert_eq!(dest.base_tabs(), vec![b, a]);
        assert_eq!(dest.active_base_tab(), Some(a));
        let state = dest.state.borrow();
        assert_eq!(state.tabs.get(b).unwrap().number, 1);
        assert!(matches!(state.tabs.get(a).unwrap().backend(), TabBackend::NotStarted));
    }

    /// A note the tab set made before any page was ready (the configured ACP agent clamping the
    /// empty tab out of manual, as `init.lua` does at startup) is said once the page says `ready`,
    /// after the `tabs` that shows the tab, and not again on a later `tabs`.
    #[test]
    fn a_mode_note_made_before_the_page_was_ready_is_said_once_at_ready() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual));
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        handle.set_default_agent(AgentId::Acp("gem".into()));
        assert_eq!(state.borrow().tabs.active_tab().mode(), SessionModeChoice::Auto);
        let notices = || {
            page.sent
                .borrow()
                .iter()
                .filter(|e| kind_of(e) == "notice")
                .map(|e| {
                    serde_json::from_str::<serde_json::Value>(e).unwrap()["text"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .filter(|text| text.contains("have no manual mode"))
                .collect::<Vec<_>>()
        };
        assert!(notices().is_empty(), "nothing is said to a page that is not ready");
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let said = notices();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("gem tabs have no manual mode"), "{said:?}");
        let sent = page.sent.borrow().clone();
        let last_tabs = sent.iter().rposition(|e| kind_of(e) == "tabs").unwrap();
        let notice = sent.iter().rposition(|e| kind_of(e) == "notice").unwrap();
        assert!(last_tabs < notice, "the tab is on screen before it is spoken of");
        handle.new_tab_for(AgentId::Claude);
        assert_eq!(notices().len(), 1, "a later tabs change does not say it again");
    }

    #[test]
    fn view_kill_counts_background_tabs() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let handle = AgentPanelHandle::over(state.clone(), None);
        let dir = agent::state_dirs::test_workspace_dir("view-background-count");
        let (tab, provider) = live_tab(&handle, &dir);
        provider.queue(agent::AgentDomainEvent::BackgroundCalls { running: 2 });
        tick_handle_until(&handle, "background work", || handle.background_count() == 1);
        assert_eq!(handle.view_running_queued(ViewId::BASE), (0, 0));
        assert_eq!(handle.view_background_count(ViewId::BASE), 1);
        // A window close that moves its running chats away moves this tab too, so its work is not ended.
        assert_eq!(
            handle.running_chats(),
            vec![(eitri_core::layout::ModuleId::agent(), Some(tab))]
        );
        let extra = ViewId::new(2).unwrap();
        assert_eq!(handle.view_background_count(extra), 0);
        state.borrow_mut().tabs.break_out(extra).unwrap();
        assert_eq!(state.borrow().tabs.tab_in(extra), Some(tab));
        assert_eq!(handle.view_background_count(ViewId::BASE), 0);
        assert_eq!(handle.view_background_count(extra), 1);
        assert_eq!(handle.running_chats(), vec![(extra.module(), None)]);
        assert_eq!(handle.background_count(), 1);
        state.borrow_mut().shutting_down = true;
        assert_eq!(handle.background_count(), 0);
        assert_eq!(handle.view_background_count(extra), 0);
        assert!(handle.running_chats().is_empty());
        for taken in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = taken.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// Every call a host makes into the panel raises the wake, so what it changed is sent; only reads and
    /// the scheduling calls themselves do not. A new `pub fn` on the handle must choose.
    #[test]
    fn every_call_into_the_panel_wakes_the_tick() {
        let checked = every_handle_call_wakes_first(&panel_sources());
        assert!(checked > 50, "only {checked} methods found");
    }

    #[test]
    fn a_handle_method_in_a_second_file_that_does_not_wake_fails_the_scan() {
        let read = fixture(&[
            (
                "panel/src/a.rs",
                "impl AgentPanelHandle {\n    pub fn arrive(&self) {\n        self.wake();\n    }\n}\n",
            ),
            (
                "panel/src/b.rs",
                "impl AgentPanelHandle {\n    pub fn set_late(&self) {\n        self.state.borrow_mut().late = true;\n    }\n}\n",
            ),
        ]);
        let why = scan_failure(|| {
            every_handle_call_wakes_first(&read);
        })
        .expect("the second file's method is read");
        assert!(why.contains("AgentPanelHandle::set_late"), "{why}");
    }

    /// Checks every `pub fn` of every `impl AgentPanelHandle` block of `sources` (the test-support block
    /// aside) and returns how many it checked.
    fn every_handle_call_wakes_first(sources: &[eitri_core::source_scan::Source]) -> usize {
        const QUIET: &[&str] = &[
            "new",
            "tick",
            "run_tick",
            "wake",
            "wake_fd",
            "take_wake",
            "poll_activate",
            "supervisor_link",
            "panel_keys",
            "nav_claims",
            "can_step",
            "has_last",
            "tab_place",
            "newest_card_label_in",
            "running_count",
            "background_count",
            "view_background_count",
            "queued_count",
            "trust_question_open",
        ];
        // Calls that wake only when what they changed needs the tick: a key that starts a typing window.
        const WAKES_WHEN: &[&str] = &["note_editor_key"];
        let code = production_code(sources);
        let mut checked = 0;
        for (at, _) in code.match_indices("impl AgentPanelHandle {") {
            // The test-support block builds handles for tests; it is not a call a host makes.
            let line_before = code[..at].trim_end().rsplit('\n').next().unwrap_or("");
            if line_before.trim_start().starts_with("#[cfg(any(test, feature") {
                continue;
            }
            let body = &code[at..];
            let end = body.find("\n}").expect("the impl block ends");
            let body = &body[..end];
            for (fn_at, _) in body.match_indices("\n    pub fn ") {
                let rest = &body[fn_at + "\n    pub fn ".len()..];
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                let next = rest.find("\n    pub fn ").unwrap_or(rest.len());
                let fn_body = &rest[..next];
                checked += 1;
                if QUIET.contains(&name.as_str()) {
                    continue;
                }
                if WAKES_WHEN.contains(&name.as_str()) {
                    assert!(
                        fn_body.contains("self.wake()"),
                        "`AgentPanelHandle::{name}` never wakes the tick"
                    );
                    continue;
                }
                // The first statement, so no early return can skip it.
                let opened = fn_body.split_once('{').map_or("", |(_, body)| body);
                assert!(
                    opened.trim_start().starts_with("self.wake();"),
                    "`AgentPanelHandle::{name}` does not wake the tick before anything else"
                );
            }
        }
        checked
    }

    #[test]
    fn a_panel_with_no_page_never_ticks() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        assert!(!handle_for(&state).tick());
    }

    #[test]
    fn the_first_document_is_the_themed_bundle_at_the_panel_base() {
        let (handle, page, _host) = handle_with_recording_page();
        handle.load_document(ViewId::BASE);
        let documents = page.documents.borrow();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].1, PANEL_BASE_URI);
        assert!(documents[0].0.contains("<style id=\"nv-theme\">"));
        assert_eq!(page.backgrounds.borrow().len(), 1);
    }

    #[test]
    fn the_first_document_is_loaded_in_the_panels_start_theme() {
        use eitri_core::theme::payload::{HlAttrs, NvimThemePayload};
        let mut payload = NvimThemePayload::empty();
        payload.options.background = "light".into();
        payload.options.colors_name = "rose-pine-dawn".into();
        payload.options.guifont = "Mono:h14".into();
        payload.groups.insert(
            "Normal".into(),
            HlAttrs {
                fg: Some(0x575279),
                bg: Some(0xfaf4ed),
                reverse: false,
            },
        );
        let theme = eitri_core::theme::ThemeTokens::derive(&payload);
        let (handle, page, _host) = handle_with_recording_page_in_theme(theme);
        assert!(page.sent.borrow().is_empty());
        assert!(page.documents.borrow().is_empty());
        assert!(page.backgrounds.borrow().is_empty());
        handle.load_document(ViewId::BASE);
        assert_eq!(*page.backgrounds.borrow(), [[0xfa, 0xf4, 0xed]]);
        let documents = page.documents.borrow();
        assert_eq!(documents.len(), 1);
        assert!(documents[0].0.contains("#faf4ed"));
        assert!(page.sent.borrow().is_empty());
    }

    #[test]
    fn a_crash_reloads_the_bundle_and_repeated_crashes_give_up() {
        let (handle, page, _host) = handle_with_recording_page();
        handle.page_gone(ViewId::BASE, "Crashed");
        assert_eq!(page.documents.borrow().len(), 1, "one reload");
        for _ in 0..10 {
            handle.page_gone(ViewId::BASE, "Crashed");
        }
        let last = page.documents.borrow().last().cloned().unwrap();
        assert_eq!(last.1, PANEL_BASE_URI);
        assert!(
            !last.0.contains("<style id=\"nv-theme\">"),
            "the give-up page is not the bundle"
        );
    }

    /// With no page (WebKit's sandbox cannot
    /// start, so no `WebView` exists), every call `main.rs`'s focus, HINT, prefix, tab-verb, theme,
    /// zoom and close paths make still runs -- none panics, none needs a display -- and nothing that
    /// only a page could act on happens: no tab is opened or switched to (the verbs refuse, so the
    /// prefix flashes), `panel_keys` never leaves `Other` (so `Ctrl+j`/`Ctrl+k` are never claimed for
    /// a composer that does not exist and always move focus), and the chat owes no attention.
    #[test]
    fn a_panel_with_no_page_answers_every_window_call_and_does_nothing_a_page_would() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let host = Rc::new(RecordingHost::default());
        state.borrow_mut().host = host.clone() as Rc<dyn PanelHost>;
        let handle = AgentPanelHandle::over(state.clone(), None);
        let tabs_before: Vec<TabId> = state.borrow().tabs.tabs().iter().map(|t| t.id).collect();

        // Focus and arrival (`pane_focus`, `move_focus`, `arrive`), the prefix's panel keys.
        handle.set_pane_focused(ViewId::BASE, true);
        handle.arrive(ViewId::BASE);
        handle.enter_input(ViewId::BASE);
        handle.nav_key(ViewId::BASE, Direction::Down);
        handle.literal_key(ViewId::BASE, &eitri_core::keymap::KeySpec::parse("C-b").unwrap());
        handle.open_keymap(ViewId::BASE, None, 0);
        handle.open_command_line(ViewId::BASE);
        handle.set_keymap_help("{}".to_string());
        assert!(state.borrow().page(ViewId::BASE).unwrap().pane_focused);
        assert_eq!(handle.panel_keys(ViewId::BASE), PanelKeys::Other);
        handle.set_pane_focused(ViewId::BASE, false);

        // HINT (`hint::HintCoordinator`): every envelope is a no-op.
        handle.hint_collect(1);
        handle.hint_show(1, &["a".to_string()]);
        handle.hint_prefix(1, "a");
        handle.hint_land(1, 0);
        handle.hint_end(1);

        // Theme, zoom, reload and a crash report: recorded where there is state, nothing else.
        let mut tokens = eitri_core::theme::ThemeTokens::fallback();
        tokens.font_size_px = 17.0;
        handle.set_theme(&tokens);
        assert_eq!(state.borrow().theme.font_size_px, 17.0);
        handle.set_panel_font_size_px(19.0);
        handle.set_editor_row_px(21.0);
        assert_eq!(state.borrow().theme.font_size_px, 19.0);
        handle.reload_document(ViewId::BASE);
        handle.reload_document_by_hand(ViewId::BASE);
        handle.page_gone(ViewId::BASE, "Crashed");
        handle.load_document(ViewId::BASE);
        handle.page_shown(ViewId::BASE);
        handle.link_clicked(ViewId::BASE, Some("https://example.com/"));
        // A page message is the one way a command reaches the host, and there is no page to post one.
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"open_url","request_id":"a","url":"https://example.com/"}"#,
        );
        assert!(host.opened.borrow().is_empty());

        // The tab verbs refuse: nothing a user could not see is opened, closed or switched to.
        handle.new_tab();
        assert!(!handle.step(1));
        assert!(!handle.select_last());
        assert!(handle.select_number(1).is_none());
        handle.begin_rename(ViewId::BASE);
        handle.confirm_close(ViewId::BASE);
        handle.confirm_close_others();
        handle.open_chooser(ViewId::BASE);
        handle.open_detail(ViewId::BASE);
        assert!(!handle.focus_oldest_card(ViewId::BASE));
        assert_eq!(handle.close_view_tabs(ViewId::BASE), Ok(0));
        let tabs_after: Vec<TabId> = state.borrow().tabs.tabs().iter().map(|t| t.id).collect();
        assert_eq!(tabs_after, tabs_before);

        // What the tray, the toast and the window-close prompt ask.
        assert_eq!(handle.attention(), Default::default());
        assert_eq!(handle.newest_card_label(), None);
        assert_eq!(handle.running_count(), 0);
        assert_eq!(handle.queued_count(), 0);
        assert!(!handle.poll_activate());
    }

    /// The pane losing the keys entirely -- the WebView-free half of
    /// `set_pane_focused(false)`, `TabSet::drop_bypass_prompt` -- drops an outstanding bypass
    /// prompt the same way switching tabs already does, so a `y` that slips through afterwards is
    /// refused by the nonce rather than landing on a tab no longer on screen.
    #[test]
    fn a_bypass_prompt_dropped_by_a_focus_loss_refuses_a_later_confirm() {
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let state = state_for_hooks(set);

        let cycled = state.borrow_mut().tabs.cycle_mode(tab, ViewId::BASE);
        let plan = match cycled {
            Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => plan,
            other => panic!("an empty auto tab must ask first: {other:?}"),
        };

        // `AgentPanelHandle::set_pane_focused(false)` calls exactly this on the state it holds.
        state.borrow_mut().tabs.drop_bypass_prompt();

        assert_eq!(
            apply_confirm_bypass(&mut state.borrow_mut(), plan.scope, plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );
        assert_eq!(set_mode(&state, tab), SessionModeChoice::Auto, "nothing changed");
    }

    /// No source that builds the panel document or its policy may allow inline script by keyword:
    /// the Rust sources of this crate, and the web frontend's own `index.html` and sources. Matches
    /// on content only, so where a line ends does not matter; the needle is split so this test does
    /// not find itself.
    #[test]
    fn no_shell_source_allows_inline_script_by_keyword() {
        use eitri_core::source_scan as scan;
        const SCANNED: &[&str] = &["shell/src", "panel/src"];
        const WEB: &[&str] = &["agent-ui/web/src"];
        let needle = concat!("script-src ", "'unsafe-inline'");
        let rust = scan::rust_sources(SCANNED);
        scan::require_anchors(&rust, &["fn content_security_policy_for(", "impl PanelDocument"]);
        let web = scan::sources(WEB, &["ts", "tsx", "js", "jsx", "css", "html"]);
        let page = scan::read_required("agent-ui/web/index.html");
        // The raw text, not the code: the policy is a string literal.
        let offenders: Vec<&str> = rust
            .iter()
            .chain(&web)
            .chain(std::iter::once(&page))
            .filter(|source| source.text.contains(needle))
            .map(|source| source.path.as_str())
            .collect();
        assert!(
            offenders.is_empty(),
            "inline script allowed by keyword in: {offenders:?}"
        );
    }

    #[test]
    fn tab_files_given_after_init_are_the_ones_offered() {
        use eitri_core::saved_tabs::{self, SavedTab, SavedTabs, TabFiles};
        use eitri_core::window_link::pair::Role;
        let project = agent::state_dirs::test_workspace_dir("tab-files-late");
        let state_home = project.join("state");
        let tabs_dir = saved_tabs::state_dir(Some(state_home.as_os_str()), None).unwrap();
        let one = |session: &str| {
            SavedTabs::from_parts(
                vec![SavedTab {
                    agent: eitri_core::agent_backend::AgentId::Claude,
                    conversation_id: "0123456789abcdef0123456789abcdef".into(),
                    provider_session_id: session.into(),
                    name: None,
                    mode: Auto,
                    view: None,
                    window: None,
                    turn_running: false,
                }],
                0,
                Vec::new(),
                None,
            )
        };
        saved_tabs::save_as(&tabs_dir, &project, Role::Main, &one("s-main")).unwrap();
        saved_tabs::save_as(&tabs_dir, &project, Role::Second, &one("s-second")).unwrap();
        let panel = || {
            let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, Auto));
            {
                let mut s = state.borrow_mut();
                s.review_roots.state_home = Some(state_home.clone().into_os_string());
                s.project_dir = project.clone();
            }
            let handle = handle_for(&state);
            (state, handle)
        };
        let offered = |state: &Rc<RefCell<AgentPanelState>>| -> Vec<String> {
            state
                .borrow()
                .restore_source
                .as_ref()
                .map(|saved| saved.tabs.iter().map(|t| t.provider_session_id.clone()).collect())
                .unwrap_or_default()
        };
        let own = TabFiles {
            role: Role::Main,
            also_offer: Vec::new(),
            offers_own: true,
        };
        let with_second = TabFiles {
            also_offer: vec![Role::Second],
            ..own.clone()
        };

        // `agent.restore = "off"`, applied first, leaves every file unread and unclaimed.
        let (state, handle) = panel();
        handle.set_restore_policy(RestorePolicy::Off);
        handle.set_tab_files(with_second.clone());
        assert!(offered(&state).is_empty());
        assert!(
            tabs_dir.join(saved_tabs::file_name_as(&project, Role::Second)).exists(),
            "window 2's file is not claimed"
        );

        let (state, handle) = panel();
        assert!(
            offered(&state).is_empty(),
            "nothing is offered before the window says what"
        );
        handle.set_tab_files(own.clone());
        assert_eq!(offered(&state), ["s-main"]);
        // Window 2 does not come back after all: its saved tabs are offered after this window's own, once.
        handle.set_tab_files(with_second.clone());
        assert_eq!(offered(&state), ["s-main", "s-second"]);
        handle.set_tab_files(with_second);
        assert_eq!(offered(&state), ["s-main", "s-second"], "nothing is offered twice");

        // Given once its own offer was used up, only the files not offered yet are read.
        let (state, handle) = panel();
        saved_tabs::save_as(&tabs_dir, &project, Role::Second, &one("s-later")).unwrap();
        handle.set_tab_files(own.clone());
        state.borrow_mut().restore_source = None;
        handle.set_tab_files(TabFiles {
            also_offer: vec![Role::Second],
            ..own
        });
        assert!(
            offered(&state).contains(&"s-later".to_string()) && !offered(&state).contains(&"s-main".to_string()),
            "{:?}",
            offered(&state)
        );
        let _ = std::fs::remove_dir_all(&project);
    }
}
