//! The panel's record of each chat view's page (`PageState`), the host's page slots (`Pages`), which page
//! shows a tab, and the ledger of answers a page is still owed (`OwedAnswers`).

use super::*;

/// What the panel keeps for one live chat page. Everything here is about the document in that page:
/// a reload or a crash of one page leaves every other page's record as it was.
pub(super) struct PageState {
    /// Set by `ready`, cleared by `reload_document`: until then nothing that can wait is sent.
    pub(super) document_ready: bool,
    /// The page's last-reported `panel_keys` mode, mirrored here. Starts, and is reset to,
    /// `Other` on every `ready` (`PanelKeys::Other`'s own doc says why, `reset_nav_mode_for_ready`
    /// is where the reset happens) -- see `AgentPanelHandle::panel_keys`.
    pub(super) nav_mode: Cell<PanelKeys>,
    /// Whether this page's pane holds the window's keyboard focus, as the window's focus tracking last
    /// reported it. It is held here so that a freshly loaded document (first load or a
    /// `prefix r` reload) is told on `ready`. A document is otherwise told only when focus
    /// changes, and a reload does not change focus.
    pub(super) pane_focused: bool,
    /// The last `tabs` envelope sent to this page, so the tick sends one only when it changed.
    pub(super) last_tabs_payload: Option<String>,
    /// The number of the latest `tabs` envelope delivered to this page ([`outbox::Strip`]): one recorded
    /// before it would put back a strip the page has already replaced.
    pub(super) tabs_delivered: u64,
    /// The open provider sessions, each with its agent, this page's `hello` was last computed against.
    pub(super) last_open_ids: Vec<(AgentId, String)>,
    /// Whether this page's last hello could carry the restore offer (only the base page's can), so
    /// the tick sends a new one only when that changes.
    pub(super) last_restore_offered: bool,
    /// The agent of the tab this page's last hello was computed for: its sessions are the ones the
    /// greeting offers, so the tick sends a new one when that tab runs another agent.
    pub(super) last_hello_agent: AgentId,
    /// The last `editor_context` envelope sent to this page.
    pub(super) last_context_payload: Option<String>,
    /// How many times this page's own web process has crashed recently, and whether
    /// automatic recovery has given up on it -- see `AgentPanelHandle::page_gone`.
    /// Re-armed by a manual recovery, never by the automatic path itself --
    /// `AgentPanelHandle::reload_document_by_hand`'s own doc says why.
    pub(super) crash_guard: crate::webview_crash_guard::WebViewCrashGuard,
}

impl Default for PageState {
    fn default() -> Self {
        PageState {
            document_ready: false,
            nav_mode: Cell::new(PanelKeys::Other),
            pane_focused: false,
            last_tabs_payload: None,
            tabs_delivered: 0,
            last_open_ids: Vec::new(),
            last_restore_offered: false,
            last_hello_agent: AgentId::Claude,
            last_context_payload: None,
            crash_guard: crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
        }
    }
}

impl AgentPanelState {
    /// The record of `view`'s page, while it is live.
    pub(super) fn page(&self, view: ViewId) -> Option<&PageState> {
        self.pages.get(&view)
    }

    /// The record of `view`'s page, made if there is none.
    pub(super) fn page_mut(&mut self, view: ViewId) -> &mut PageState {
        self.pages.entry(view).or_default()
    }

    /// Whether `view`'s page is live and its document said `ready`.
    pub(super) fn ready(&self, view: ViewId) -> bool {
        self.page(view).is_some_and(|page| page.document_ready)
    }

    /// The tab `view`'s page shows: the base view's active tab, or an extra view's bound tab (none
    /// before one is opened in it).
    pub(super) fn tab_of_page(&self, view: ViewId) -> Option<TabId> {
        if view.is_base() {
            Some(self.tabs.active())
        } else {
            self.tabs.tab_in(view)
        }
    }

    /// The view whose page shows `tab` now: the base view for its active tab, an extra view for its
    /// bound tab while that view's page is live. `None` for a background tab, and for a bound tab
    /// whose page is idle or not attached yet. The base page is always counted as there: with no page
    /// at all, what is sent to it goes nowhere.
    pub(super) fn shown_in(&self, tab: TabId) -> Option<ViewId> {
        if !self.tabs.is_shown(tab) {
            return None;
        }
        let view = self.tabs.view_of(tab);
        (view.is_base() || self.pages.contains_key(&view)).then_some(view)
    }

    /// Where a payload about `tab` that must wait for a ready document goes: [`Self::shown_in`] once
    /// that page's document is ready, else `None` (the payload waits for that page's `ready` or the
    /// tab's next switch, as a background tab's does).
    pub(super) fn page_of(&self, tab: TabId) -> Option<ViewId> {
        self.shown_in(tab).filter(|view| self.ready(*view))
    }
}

/// One chat page the panel can reach, and whether it is routed to. An idle page still takes the
/// theme and the font, and nothing else.
#[derive(Clone)]
pub(super) struct PageSlot {
    pub(super) page: Rc<PanelPage>,
    pub(super) live: bool,
}

/// Every chat page the window built, by the view it shows; shared by every clone of the handle. A
/// page is never moved to another view: a view's page is replaced only by attaching one for it.
#[derive(Clone, Default)]
pub(super) struct Pages(pub(super) Rc<RefCell<BTreeMap<ViewId, PageSlot>>>);

impl Pages {
    /// `view`'s page while it is live (routed to).
    pub(super) fn live(&self, view: ViewId) -> Option<Rc<PanelPage>> {
        self.0
            .borrow()
            .get(&view)
            .filter(|slot| slot.live)
            .map(|slot| slot.page.clone())
    }

    /// `view`'s page, live or idle.
    pub(super) fn any(&self, view: ViewId) -> Option<Rc<PanelPage>> {
        self.0.borrow().get(&view).map(|slot| slot.page.clone())
    }

    /// Every live page with its view, base first.
    pub(super) fn live_views(&self) -> Vec<(ViewId, Rc<PanelPage>)> {
        self.0
            .borrow()
            .iter()
            .filter(|(_, slot)| slot.live)
            .map(|(view, slot)| (*view, slot.page.clone()))
            .collect()
    }

    /// Every page, live and idle.
    pub(super) fn every(&self) -> Vec<Rc<PanelPage>> {
        self.0.borrow().values().map(|slot| slot.page.clone()).collect()
    }

    /// Sends `payload` to `view`'s page if it is live. The map is not borrowed while the page runs.
    pub(super) fn send(&self, view: ViewId, payload: &str) {
        if let Some(page) = self.live(view) {
            page.send(payload);
        }
    }
}

/// What a page is owed an answer for, beyond the request's own id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Owed {
    /// The start of this tab (a first message, a resume): answered when the connect ends, is put
    /// off, or the tab closes first. A tab has at most one start owed.
    StartPage(TabId),
    /// A review request the flow is working on. The flow knows it by the ledger's key, so two pages
    /// that number their requests alike are never mixed up.
    ReviewRequest,
    /// A `:trust` or `:untrust` the gate is working on, known to the gate by the ledger's key, about
    /// `tab`, typed in `typed_in`'s page. That page is the only one its prompt is shown in.
    TrustCommand { tab: TabId, typed_in: ViewId },
    /// The terminal handoff of this tab, answered once its session's close has returned.
    Handoff(TabId),
    /// A message of this tab typed in the page, held until the presence marks its turn as running:
    /// answered when it goes out or is not sent. A tab has at most one held.
    HeldPrompt(TabId),
}

/// One answer owed to a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OwedAnswer {
    /// The page the answer goes to; `None` once that page has closed, when the answer is dropped.
    pub(super) page: Option<ViewId>,
    /// The id the page that sent the request knows it by: the answer carries this one.
    pub(super) request_id: String,
    pub(super) what: Owed,
}

/// The answers the panel owes pages for requests that finish later, by a key unique in the window
/// (pages number their requests alike). A page that closes owes nothing: its entries stay until
/// their work ends, so each kind still knows the request was asked, but nothing is sent for them.
#[derive(Default)]
pub(super) struct OwedAnswers {
    pub(super) owed: BTreeMap<String, OwedAnswer>,
    /// The number in the next key.
    pub(super) next: u64,
}

impl OwedAnswers {
    /// Records that `page` is owed the answer to its request `request_id`; the key it is known by.
    pub(super) fn owe(&mut self, page: ViewId, request_id: &str, what: Owed) -> String {
        let key = format!("owed-{}", self.next);
        self.next += 1;
        self.owed.insert(
            key.clone(),
            OwedAnswer {
                page: Some(page),
                request_id: request_id.to_string(),
                what,
            },
        );
        key
    }

    pub(super) fn get(&self, key: &str) -> Option<&OwedAnswer> {
        self.owed.get(key)
    }

    /// The entry under `key`, spent.
    pub(super) fn take(&mut self, key: &str) -> Option<OwedAnswer> {
        self.owed.remove(key)
    }

    /// The key of the first entry `matches` picks.
    pub(super) fn key_where(&self, matches: impl Fn(&OwedAnswer) -> bool) -> Option<String> {
        self.owed
            .iter()
            .find(|(_, answer)| matches(answer))
            .map(|(key, _)| key.clone())
    }

    /// The first entry `matches` picks, spent.
    pub(super) fn take_where(&mut self, matches: impl Fn(&OwedAnswer) -> bool) -> Option<OwedAnswer> {
        let key = self.key_where(matches)?;
        self.take(&key)
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&str, &OwedAnswer) -> bool) {
        self.owed.retain(|key, answer| keep(key, answer));
    }

    /// `view`'s page has closed: nothing it asked for is sent anywhere any more.
    pub(super) fn page_closed(&mut self, view: ViewId) {
        for answer in self.owed.values_mut() {
            if answer.page == Some(view) {
                answer.page = None;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.owed.is_empty()
    }

    /// How many answers `view`'s page is still owed.
    #[cfg(test)]
    pub(super) fn owed_to(&self, view: ViewId) -> usize {
        self.owed.values().filter(|answer| answer.page == Some(view)).count()
    }
}

/// Resets the nav-mode mirror (`AgentPanelState::nav_mode`'s own doc): called from the `Ready` arm on
/// every `ready` -- the first one and every reload's alike -- so the mirror never survives a
/// reload holding whatever mode the torn-down document last reported.
///
/// Without this, a reload while the composer is open (`nav_mode == Input`) leaves the mirror at
/// `Input` even though the fresh document starts in BROWSE. Before that document posts its own
/// first `panel_keys` message, the host's `claims(Input, Direction::Down)` is `false`, so a
/// `Ctrl+j` the user presses to enter the composer is not claimed and falls through to
/// `move_focus` instead -- the opposite of what BROWSE + `Ctrl+j` must do.
///
/// A plain field write rather than a bigger function only because it is one: kept apart from the
/// rest of `Ready`'s payload-building so it has its own doc and its own test
/// (`ready_resets_the_nav_mode_mirror_to_other`), the same reason `ready_payloads` and
/// `changed_envelope` are pulled out of that arm.
pub(super) fn reset_nav_mode_for_ready(state: &AgentPanelState, view: ViewId) {
    if let Some(record) = state.page(view) {
        record.nav_mode.set(PanelKeys::Other);
    }
}

/// The other half of the same rule: when the page that reported a mode goes -- its web process
/// died (`page_gone`, both the reload and the give-up path) or it is being
/// replaced (`reload_document`) -- the mirror is `Other` until a new page says otherwise. The
/// give-up document (`webview_crash_guard::crash_message_html`) never sends `ready`, so the reset
/// on `ready` alone left `Ctrl+j` claimed for a page that could not take it.
pub(super) fn forget_the_pages_nav_mode(state: &AgentPanelState, view: ViewId) {
    if let Some(record) = state.page(view) {
        record.nav_mode.set(PanelKeys::Other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    /// The nav-mode mirror starts `Other` and returns to `Other` on every ready (a reload).
    /// A round trip through `handle_inbound_message`'s `Ready` arm needs a real `WebView`
    /// (`dispatch_all`/`ok` both take one), so this pins the one-line mutation the arm calls,
    /// `reset_nav_mode_for_ready`, directly -- the same shape as
    /// `nav_fallthrough_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_direction`
    /// pinning `run_nav_fallthrough` apart from the WebView it is also called from.
    ///
    /// Without resetting `nav_mode` on `ready`, a reload while `mode == Input` would leave
    /// the mirror at `Input` even though the fresh document starts in BROWSE, so `main.rs`'s
    /// `claims(PanelKeys::Input, Direction::Down)` was `false` and a `Ctrl+j` meant to open the
    /// composer fell through to `move_focus` instead.
    #[test]
    fn ready_resets_the_nav_mode_mirror_to_other() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        for stale in [PanelKeys::Input, PanelKeys::Browse] {
            state.borrow().page(ViewId::BASE).unwrap().nav_mode.set(stale);
            reset_nav_mode_for_ready(&state.borrow(), ViewId::BASE);
            assert_eq!(
                state.borrow().page(ViewId::BASE).unwrap().nav_mode.get(),
                PanelKeys::Other,
                "stale {stale:?} must clear"
            );
        }
    }

    /// A crashed or replaced page's BROWSE/INPUT is
    /// forgotten at once, not at a `ready` the give-up document never sends. `page_gone`
    /// and `reload_document` are exercised against a recording page elsewhere; this pins the call they both make.
    #[test]
    fn a_dead_pages_nav_mode_is_forgotten_before_any_ready() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        for stale in [PanelKeys::Input, PanelKeys::Browse] {
            state.borrow().page(ViewId::BASE).unwrap().nav_mode.set(stale);
            forget_the_pages_nav_mode(&state.borrow(), ViewId::BASE);
            assert_eq!(
                state.borrow().page(ViewId::BASE).unwrap().nav_mode.get(),
                PanelKeys::Other,
                "stale {stale:?}"
            );
        }
    }

    /// Every tab the envelopes `page` was sent name, for those that name one.
    fn tabs_named(page: &RecordingPage) -> Vec<u64> {
        page.sent
            .borrow()
            .iter()
            .filter_map(|e| json_of(e)["tab"].as_u64())
            .collect()
    }

    /// The ids and the active tab of the last `tabs` envelope `page` was sent.
    fn last_strip(page: &RecordingPage) -> (Vec<u64>, u64) {
        let sent = page.sent.borrow();
        let strip = sent
            .iter()
            .rev()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "tabs")
            .expect("a tabs envelope");
        let ids = strip["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_u64().unwrap())
            .collect();
        (ids, strip["active"].as_u64().unwrap())
    }

    #[test]
    fn break_out_and_join_route_every_payload_to_the_right_page() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-break-join");
        let (handle, base, second) = handle_with_two_pages();
        let first = handle.state.borrow().tabs.active();
        let (moved, provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(moved));
        assert_eq!(handle.state.borrow().tabs.active(), first, "the base view falls back");
        let (ids, active) = last_strip(&base);
        assert!(!ids.contains(&moved.0), "the base strip no longer lists it: {ids:?}");
        assert_eq!(active, first.0);
        assert_eq!(last_strip(&second), (vec![moved.0], moved.0));

        clear(&[&base, &second]);
        stream(&provider, "t1", "to the extra view");
        tick_handle_until(&handle, "the moved tab's events in page 2", || {
            sent_about(&second, "events", moved)
        });
        assert!(
            !tabs_named(&base).contains(&moved.0),
            "the base page is told nothing of it: {:?}",
            base.sent.borrow()
        );

        assert_eq!(handle.join_back(view(2)), Ok(moved));
        let (ids, active) = last_strip(&base);
        assert!(ids.contains(&moved.0), "{ids:?}");
        assert_eq!(active, moved.0, "it comes back as the base view's active tab");
        clear(&[&base, &second]);
        provider.queue(agent::AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "back in the base view".into(),
        });
        tick_handle_until(&handle, "the joined tab's events in the base page", || {
            sent_about(&base, "events", moved)
        });
        assert!(
            !tabs_named(&second).contains(&moved.0),
            "page 2 shows it no longer: {:?}",
            second.sent.borrow()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_receiving_page_gets_the_tabs_snapshot_with_its_pending_card() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-break-card");
        let (handle, _base, second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || {
            handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .attention
                .attention()
                .pending
                == 1
        });
        clear(&[&second]);
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        let sent = second.sent.borrow().clone();
        let strip = sent.iter().position(|e| kind_of(e) == "tabs").expect("its strip");
        let snapshot = sent
            .iter()
            .position(|e| kind_of(e) == "snapshot" && json_of(e)["tab"] == tab.0)
            .expect("its tab's snapshot");
        assert!(strip < snapshot, "the strip before the tab it shows");
        assert_eq!(last_strip(&second), (vec![tab.0], tab.0));
        assert!(
            sent[snapshot].contains("perm-1"),
            "with its pending card: {}",
            sent[snapshot]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_page_has_its_own_document_ready() {
        let (handle, _base, _host) = handle_with_recording_page();
        let second = attach_recording_page(&handle, view(2));
        handle.page_message(ViewId::BASE, READY);
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let moved = handle.break_out(view(2)).unwrap();
        handle.tick();
        assert!(
            second.sent.borrow().is_empty(),
            "nothing goes to a page before its ready: {:?}",
            second.sent.borrow()
        );
        handle.page_message(ViewId::BASE, READY);
        handle.state.borrow_mut().tabs.rename(moved, "renamed");
        handle.tick();
        assert!(
            second.sent.borrow().is_empty(),
            "the base page's ready releases nothing of page 2's: {:?}",
            second.sent.borrow()
        );
        handle.page_message(view(2), READY);
        assert_eq!(last_strip(&second), (vec![moved.0], moved.0));
        assert!(second.sent.borrow().iter().any(|e| kind_of(e) == "hello"));
        clear(&[&second]);
        handle.state.borrow_mut().tabs.rename(moved, "again");
        handle.tick();
        assert!(
            second.sent.borrow().iter().any(|e| kind_of(e) == "tabs"),
            "from its own ready on, its strip follows"
        );
    }

    #[test]
    fn page_shown_from_an_extra_view_marks_its_own_tab_seen_not_active() {
        let (handle, base, second) = handle_with_two_pages();
        let active = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let bound = handle.break_out(view(2)).unwrap();
        assert_eq!(handle.state.borrow().tabs.active(), active);
        base.visible.set(true);
        second.visible.set(true);
        for tab in [active, bound] {
            handle
                .state
                .borrow_mut()
                .tabs
                .get_mut(tab)
                .unwrap()
                .attention
                .session_ended(false);
        }
        let unread = |tab: TabId| {
            handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .attention
                .attention()
                .unread
        };
        assert!(unread(active) && unread(bound));
        handle.page_shown(view(2));
        assert!(!unread(bound), "the page's own tab is seen");
        assert!(unread(active), "the base view's active tab is not");
    }

    #[test]
    fn prefix_x_on_the_base_view_leaves_an_extra_views_session_running_and_opens_a_fresh_base_tab() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-kill-base");
        let (handle, _base, _second) = handle_with_two_pages();
        let (bound, _bound_provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(bound));
        let (closed, _closed_provider) = live_tab(&handle, &dir);
        let before = handle.state.borrow().tabs.base_tabs();
        assert_eq!(before.len(), 2);
        assert_eq!(handle.close_view_tabs(ViewId::BASE), Ok(2));
        let state = handle.state.borrow();
        assert!(state.tabs.get(closed).is_none());
        let now = state.tabs.base_tabs();
        assert_eq!(now.len(), 1, "the base view is left one tab");
        assert!(!before.contains(&now[0]), "a fresh one");
        assert_eq!(state.tabs.active(), now[0]);
        let kept = state.tabs.get(bound).expect("the extra view's tab is kept");
        assert!(matches!(kept.backend(), TabBackend::Live(_)), "its session runs on");
        assert_eq!(state.tabs.view_of(bound), view(2));
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_idle_page_is_told_nothing_and_holds_a_blank_document() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-idle");
        let (handle, _base, second) = handle_with_two_pages();
        let (_bound, provider) = live_tab(&handle, &dir);
        handle.break_out(view(2)).unwrap();
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "its card in page 2", || {
            second.sent.borrow().iter().any(|e| e.contains("perm-1"))
        });
        let documents = second.documents.borrow().len();
        handle.idle_page(view(2));
        assert_eq!(second.documents.borrow().len(), documents + 1, "a document is loaded");
        let panel = {
            let state = handle.state.borrow();
            state.document.themed(&state.theme.css_vars())
        };
        {
            let loaded = second.documents.borrow();
            let (html, base) = loaded.last().unwrap();
            assert_eq!(
                (html.as_str(), base.as_str()),
                (IDLE_DOCUMENT, PANEL_BASE_URI),
                "a blank document at the panel's secure base"
            );
            assert!(!html.contains("<script"), "nothing to run: {html}");
            assert_ne!(*html, panel, "not the panel");
        }
        assert!(handle.state.borrow().page(view(2)).is_none(), "its record is gone");
        clear(&[&second]);
        provider.queue(write_card("perm-2"));
        for _ in 0..20 {
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        handle.set_pane_focused(view(2), true);
        handle.arrive(view(2));
        handle.page_message(view(2), READY);
        assert!(
            second.sent.borrow().is_empty(),
            "an idle page is sent nothing, and its own message is dropped: {:?}",
            second.sent.borrow()
        );
        assert!(handle.state.borrow().page(view(2)).is_none());

        // Reused, as the window reuses a pooled page: attached again, then the panel is loaded.
        handle.attach_page(view(2), Rc::new(PanelPage::new(second.clone() as Rc<dyn PageSurface>)));
        handle.load_document(view(2));
        assert_eq!(
            second.documents.borrow().last().cloned(),
            Some((panel, PANEL_BASE_URI.to_string())),
            "the panel again once reused"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_theme_reaches_every_page_idle_ones_included() {
        let (handle, base, second) = handle_with_two_pages();
        let third = attach_recording_page(&handle, view(3));
        handle.idle_page(view(3));
        for page in [&base, &second, &third] {
            page.sent.borrow_mut().clear();
            page.backgrounds.borrow_mut().clear();
        }
        let mut tokens = eitri_core::theme::ThemeTokens::fallback();
        tokens.font_size_px = 23.0;
        handle.set_theme(&tokens);
        handle.set_panel_font_size_px(25.0);
        for page in [&base, &second, &third] {
            assert_eq!(page.backgrounds.borrow().len(), 2);
            let themes = page.sent.borrow().iter().filter(|e| kind_of(e) == "theme").count();
            assert_eq!(themes, 2, "{:?}", page.sent.borrow());
        }
    }

    #[test]
    fn a_reload_of_an_extra_page_gets_its_own_tabs_snapshot_and_no_base_card() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-reload");
        let (handle, base, second) = handle_with_two_pages();
        let (bound, _bound_provider) = live_tab(&handle, &dir);
        handle.break_out(view(2)).unwrap();
        let (base_tab, base_provider) = live_tab(&handle, &dir);
        base_provider.queue(write_card("perm-base"));
        tick_handle_until(&handle, "the base tab's card", || {
            handle
                .state
                .borrow()
                .tabs
                .get(base_tab)
                .unwrap()
                .attention
                .attention()
                .pending
                == 1
        });
        clear(&[&base, &second]);
        handle.page_message(view(2), READY);
        let sent = second.sent.borrow().clone();
        assert_eq!(last_strip(&second), (vec![bound.0], bound.0));
        assert!(sent_about(&second, "snapshot", bound), "{sent:?}");
        let hello = sent
            .iter()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "hello")
            .expect("a hello");
        assert!(hello["restore"].is_null(), "no restore offer outside the base page");
        for envelope in &sent {
            assert!(!envelope.contains("perm-base"), "{envelope}");
            assert_ne!(json_of(envelope)["tab"].as_u64(), Some(base_tab.0), "{envelope}");
        }
        assert!(
            base.sent.borrow().is_empty(),
            "the base page is told nothing of it: {:?}",
            base.sent.borrow()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_shown_tabs_streaming_in_one_tick_reach_their_own_pages() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-two-streams");
        let (handle, base, second) = handle_with_two_pages();
        let (bound, bound_provider) = live_tab(&handle, &dir);
        handle.break_out(view(2)).unwrap();
        let (shown, shown_provider) = live_tab(&handle, &dir);
        clear(&[&base, &second]);
        stream(&bound_provider, "tb", "the bound tab's text");
        stream(&shown_provider, "ts", "the base tab's text");
        tick_handle_until(&handle, "both pages' events", || {
            sent_about(&second, "events", bound) && sent_about(&base, "events", shown)
        });
        assert!(
            tabs_named(&base).iter().all(|tab| *tab == shown.0),
            "{:?}",
            base.sent.borrow()
        );
        assert!(
            tabs_named(&second).iter().all(|tab| *tab == bound.0),
            "{:?}",
            second.sent.borrow()
        );
        assert!(base.sent.borrow().iter().all(|e| !e.contains("the bound tab's text")));
        assert!(second.sent.borrow().iter().all(|e| !e.contains("the base tab's text")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_and_overlays_in_the_base_page_leave_an_extra_views_prompts() {
        let (handle, _base, _second) = handle_with_two_pages();
        let state = handle.state.clone();
        state.borrow_mut().tabs.open(AgentId::Claude);
        let bound = handle.break_out(view(2)).unwrap();
        // The mode key in page 2 asks first (a fresh press there takes page 2's own questions off,
        // so its tab's trust question is asked after it).
        let default_prompt = || {
            let cycled = cycle_default_mode(&mut state.borrow_mut(), view(2));
            match cycled {
                eitri_core::tab_set::ModeCycle::Confirm(plan) => plan,
                other => panic!("auto -> bypass asks first: {other:?}"),
            }
        };
        let plan = default_prompt();
        assert_eq!(plan.page, view(2));
        let nonce = {
            let mut s = state.borrow_mut();
            s.tabs.defer_start(bound, first_send("r1", "hello")).unwrap();
            s.tabs.ask_trust(bound).unwrap()
        };
        handle.arrive(ViewId::BASE);
        handle.open_keymap(ViewId::BASE, None, 0);
        handle.hint_collect(1);
        handle.set_pane_focused(ViewId::BASE, false);
        assert_eq!(state.borrow().tabs.check_trust_answer(bound, nonce, view(2)), Ok(()));
        assert!(matches!(
            state.borrow_mut().tabs.confirm_bypass(plan.scope, plan.nonce, view(2)),
            Ok(eitri_core::tab_set::ConfirmOutcome::Entered { .. })
        ));

        // Page 2 losing the keys drops both of its own.
        state
            .borrow_mut()
            .tabs
            .apply_configured_default(SessionModeChoice::Auto);
        let plan = default_prompt();
        let nonce = state.borrow_mut().tabs.ask_trust(bound).unwrap();
        assert_eq!(state.borrow().tabs.check_trust_answer(bound, nonce, view(2)), Ok(()));
        handle.set_pane_focused(view(2), false);
        assert!(state.borrow().tabs.check_trust_answer(bound, nonce, view(2)).is_err());
        assert!(state
            .borrow_mut()
            .tabs
            .confirm_bypass(plan.scope, plan.nonce, view(2))
            .is_err());
    }
}
