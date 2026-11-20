//! A remote tab's chooser: the sessions its server's transcripts hold, fetched on a worker when the chooser opens (at
//! most once per ten seconds per tab) and shown when they arrive. Nothing here waits on the network; the window's
//! tick takes in an answer and sends its rows again to the page that asked, which lays them over its chooser if one
//! is still open.
//!
//! The server's answers are checked against the tab's canonical root, the folder its trust scan named
//! (`Discovery.root`). A tab whose folder the scan has not named yet is not asked: its chooser says so, a check is
//! queued, and the list is fetched once the check came back.

use super::*;
use eitri_core::remote_project::resume::{list_on_worker, shown_refusal, ServerSessions, SessionListing};
use eitri_core::remote_project::transcripts::TabScope;
use std::time::{Duration, Instant};

/// How long a chooser waits for the tab's folder to be checked before it gives up and says so.
const SCAN_WAIT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct RemoteSessions {
    listings: BTreeMap<TabId, SessionListing>,
    /// Choosers opened before the tab's folder was named, with the page and when to give up: asked once it is.
    awaiting_scan: BTreeMap<TabId, (ViewId, Instant)>,
    /// Choosers to send again when the tab's list arrives, by tab: the page that opened it.
    awaiting_list: BTreeMap<TabId, ViewId>,
}

impl RemoteSessions {
    pub(super) fn forget_tab(&mut self, tab: TabId) {
        self.listings.remove(&tab);
        self.awaiting_scan.remove(&tab);
        self.awaiting_list.remove(&tab);
    }

    /// Whether the tick has something to read: an ask under way, or a chooser waiting for a check.
    pub(super) fn needs_polling(&self) -> bool {
        !self.awaiting_scan.is_empty() || self.listings.values().any(SessionListing::is_pending)
    }

    /// The server's sessions for `tab` as the last ask found them; empty before it did or when it could not.
    pub(super) fn sessions_of(&self, tab: Option<TabId>) -> Vec<agent::ResumableSession> {
        let found = tab
            .and_then(|tab| self.listings.get(&tab))
            .and_then(SessionListing::latest);
        match found {
            Some(Ok(ServerSessions { sessions, .. })) => sessions.clone(),
            _ => Vec::new(),
        }
    }

    /// The title the last list gave `session`, for a tab resumed from it.
    pub(super) fn title_of(&self, tab: TabId, session: &str) -> Option<String> {
        self.sessions_of(Some(tab))
            .into_iter()
            .find(|s| s.provider_session_id == session)
            .and_then(|s| s.title)
    }

    /// What a chooser shows besides the rows, for `tab`: that its list is on its way, why it could not be had, or that
    /// the server holds more than are listed. Safe to show: the server's words are escaped.
    pub(super) fn line_for(&self, tab: Option<TabId>) -> Option<String> {
        let tab = tab?;
        if self.awaiting_scan.contains_key(&tab) {
            return Some("checking this project on the server before its sessions can be listed".to_string());
        }
        let listing = self.listings.get(&tab)?;
        match listing.latest() {
            Some(Err(why)) => Some(why.clone()),
            Some(Ok(found)) if found.truncated => Some(format!(
                "the server holds more sessions than these {}; the newest are listed",
                found.sessions.len()
            )),
            Some(Ok(_)) => None,
            None => listing
                .is_pending()
                .then(|| "listing the sessions on the server...".to_string()),
        }
    }
}

/// Whether the window's connection to the server offers transcripts, from the Hello it already holds.
pub(super) fn offers_transcripts(state: &AgentPanelState) -> bool {
    state.server.as_ref().is_some_and(|server| server.offers_transcripts())
}

/// Whether the window's connection offers transcripts, or `None` while it has no connection or no Hello yet.
pub(super) fn transcripts_offered(state: &AgentPanelState) -> Option<bool> {
    state.server.as_ref().and_then(|server| server.transcripts_offered())
}

/// The tab's canonical root as its trust scan named it; empty before one did.
pub(super) fn canonical_root_of(state: &AgentPanelState, tab: TabId) -> String {
    state
        .trust
        .bound_discovery(super::trust::trust_subject(state, tab))
        .map(|found| found.root.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Tells every remote tab its root on the server as the window's trust scan named it, before the gate judges its
/// requests: the server's path facts are used only when they are about that root (`Tab::gate_root`).
pub(super) fn note_server_roots(state: &mut AgentPanelState) {
    let remote: Vec<TabId> = state
        .tabs
        .tabs()
        .iter()
        .filter(|tab| tab.is_remote())
        .map(|tab| tab.id)
        .collect();
    for tab in remote {
        let root = canonical_root_of(state, tab);
        state
            .tabs
            .set_server_root(tab, (!root.is_empty()).then(|| std::path::PathBuf::from(root)));
    }
}

/// The Claude config directory an agent of this window reported for the tab's server and account.
pub(super) fn reported_config_dir_of(state: &AgentPanelState, tab: TabId) -> Option<String> {
    let eitri_core::remote_project::TabOrigin::Remote {
        host, host_key_sha256, ..
    } = state.tabs.get(tab)?.origin()
    else {
        return None;
    };
    let account = agent::account::configured().map(|account| account.name().to_string());
    super::trust::reported_config_dir(
        state
            .tabs
            .tabs()
            .iter()
            .map(|t| (t.origin(), t.live().and_then(|backend| backend.provider_info()))),
        host,
        host_key_sha256,
        account.as_deref(),
    )
    .map(|dir| dir.to_string_lossy().into_owned())
}

/// What the transcripts calls name `tab`'s project, or a line saying why they cannot be asked yet.
fn scope_of(state: &AgentPanelState, tab: TabId) -> Result<TabScope, String> {
    let Some(server) = &state.server else {
        return Err(super::remote_tabs::no_connection(&super::remote_tabs::host_of(
            state, tab,
        )));
    };
    let account = agent::account::configured().map(|account| account.name().to_string());
    server.tab_scope(
        &canonical_root_of(state, tab),
        account.as_deref(),
        reported_config_dir_of(state, tab).as_deref(),
    )
}

/// `view`'s chooser opened on a remote tab: starts the ask for its list unless one was made lately, and notes the
/// chooser to be sent again when the list arrives. A tab whose folder the scan has not named waits for one. Returns
/// the line to show now, if any (the chooser itself is built by `chooser_payload` from what is known).
pub(super) fn on_open(state: &mut AgentPanelState, view: ViewId) -> Option<String> {
    let Some(tab) = state.tab_of_page(view) else {
        return Some("this page shows no tab to list sessions for".to_string());
    };
    if canonical_root_of(state, tab).is_empty() && state.server.is_some() {
        // A check of the project queues once however often the chooser opens; the list is asked for when it is back.
        state.trust.refresh(super::trust::trust_subject(state, tab));
        state
            .remote_sessions
            .awaiting_scan
            .insert(tab, (view, Instant::now() + SCAN_WAIT));
        return state.remote_sessions.line_for(Some(tab));
    }
    match ask(state, tab) {
        Ok(()) => {
            note_list_awaited(state, tab, view);
            state.remote_sessions.line_for(Some(tab))
        }
        Err(why) => Some(shown_refusal(&why)),
    }
}

/// Whether an ask for `tab`'s list is under way.
fn is_listing(state: &AgentPanelState, tab: TabId) -> bool {
    state
        .remote_sessions
        .listings
        .get(&tab)
        .is_some_and(SessionListing::is_pending)
}

/// Notes that `view`'s chooser is to be sent the rows when the list arrives: only while an ask is under way (one that
/// was throttled leaves the chooser the last list, and nothing to wait for).
fn note_list_awaited(state: &mut AgentPanelState, tab: TabId, view: ViewId) {
    if is_listing(state, tab) {
        state.remote_sessions.awaiting_list.insert(tab, view);
    }
}

/// Starts `tab`'s ask on a worker if the last was made more than ten seconds ago and none is under way.
fn ask(state: &mut AgentPanelState, tab: TabId) -> Result<(), String> {
    let scope = scope_of(state, tab)?;
    let connection = state.server.clone().expect("a scope came from the connection");
    state
        .remote_sessions
        .listings
        .entry(tab)
        .or_default()
        .ask(Instant::now(), move || list_on_worker(connection, scope));
    Ok(())
}

/// The rows of `tab`'s list, as known now, to `view`'s chooser if it is still open there, and the line that goes with
/// them.
fn send_list(state: &mut AgentPanelState, tab: TabId, view: ViewId, out: &mut Outbox) {
    // The person may have closed the chooser while the list was on its way: the page lays these rows over a
    // chooser still open and opens none, so the keys it gave back stay where they are.
    let (payload, listed) = super::detail::chooser_update_payload(state, view);
    state.chooser_listed = listed;
    state.chooser_view = view;
    out.send(view, payload);
    if let Some(line) = state.remote_sessions.line_for(Some(tab)) {
        out.send(view, eitri_core::agent_bridge::serialize_notice_for_js(&line));
    }
}

/// Every tick: a chooser that waited for a check asks now that the tab's folder is named, and an answer that came is
/// sent as the chooser's rows again to the page that opened it, if its chooser is still open there.
pub(super) fn tick(state: &mut AgentPanelState, out: &mut Outbox) {
    let now = Instant::now();
    let waiting: Vec<(TabId, ViewId, Instant)> = state
        .remote_sessions
        .awaiting_scan
        .iter()
        .map(|(tab, (view, until))| (*tab, *view, *until))
        .collect();
    for (tab, view, until) in waiting {
        if state.tabs.get(tab).is_none() {
            state.remote_sessions.awaiting_scan.remove(&tab);
        } else if !canonical_root_of(state, tab).is_empty() {
            state.remote_sessions.awaiting_scan.remove(&tab);
            match ask(state, tab) {
                Ok(()) if is_listing(state, tab) => {
                    state.remote_sessions.awaiting_list.insert(tab, view);
                }
                // Asked lately: the last list is what there is, and the chooser waiting for the check gets it now.
                Ok(()) => send_list(state, tab, view, out),
                Err(why) => out.send(
                    view,
                    eitri_core::agent_bridge::serialize_notice_for_js(&shown_refusal(&why)),
                ),
            }
        } else if now >= until {
            state.remote_sessions.awaiting_scan.remove(&tab);
            out.send(
                view,
                eitri_core::agent_bridge::serialize_notice_for_js(
                    "this project on the server was not checked in time; open the chooser again",
                ),
            );
        }
    }
    let arrived: Vec<TabId> = state
        .remote_sessions
        .listings
        .iter_mut()
        .filter_map(|(tab, listing)| listing.collect().then_some(*tab))
        .collect();
    for tab in arrived {
        let Some(view) = state.remote_sessions.awaiting_list.remove(&tab) else {
            continue;
        };
        if state.tabs.get(tab).is_none() {
            continue;
        }
        send_list(state, tab, view, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;
    use eitri_core::agent_backend::BackendKind;
    use eitri_core::agent_bridge::SessionModeChoice;
    use eitri_core::remote_project::resume::spawn_listing;
    use eitri_core::tab_set::TabSet;

    fn remote_window() -> (Rc<RefCell<AgentPanelState>>, TabId) {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let tab = {
            let mut s = state.borrow_mut();
            s.tabs
                .set_project_origin(eitri_core::remote_project::TabOrigin::Remote {
                    host: "example-host".into(),
                    host_key_sha256: "SHA256:example".into(),
                    root: "/srv/project".into(),
                })
                .unwrap();
            s.server = Some(super::super::remote_tabs::tests::connection_offering_transcripts());
            s.tabs.active()
        };
        (state, tab)
    }

    fn listing_arriving(state: &Rc<RefCell<AgentPanelState>>, tab: TabId) {
        let asked = state
            .borrow_mut()
            .remote_sessions
            .listings
            .entry(tab)
            .or_default()
            .ask(Instant::now(), || {
                spawn_listing(|| {
                    Ok(ServerSessions {
                        sessions: Vec::new(),
                        truncated: false,
                    })
                })
            });
        assert!(asked);
    }

    /// A list that arrives after the chooser was sent goes to the page as the rows of a chooser still open
    /// (`chooser_update`), never as a `chooser`: a page whose chooser the person closed meanwhile would take that for
    /// a request to open one and give it the keys again.
    #[test]
    fn a_list_that_arrives_is_sent_as_an_update_never_as_a_chooser() {
        let (state, tab) = remote_window();
        listing_arriving(&state, tab);
        state
            .borrow_mut()
            .remote_sessions
            .awaiting_list
            .insert(tab, ViewId::BASE);
        let mut out = Outbox::default();
        wait_until_arrived(&state, &mut out);
        let kinds: Vec<String> = out.payloads().iter().map(|p| kind_of(p)).collect();
        assert!(kinds.contains(&"chooser_update".to_string()), "{kinds:?}");
        assert!(!kinds.contains(&"chooser".to_string()), "{kinds:?}");
    }

    fn wait_until_arrived(state: &Rc<RefCell<AgentPanelState>>, out: &mut Outbox) {
        let started = Instant::now();
        while out.payloads().is_empty() {
            tick(&mut state.borrow_mut(), out);
            assert!(started.elapsed() < Duration::from_secs(30), "the list never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// An ask that was not made (the last one is recent) leaves no chooser waiting for a list that is not coming.
    #[test]
    fn a_throttled_ask_leaves_nothing_awaited() {
        let (state, tab) = remote_window();
        listing_arriving(&state, tab);
        wait_for_list(&state, tab);
        assert!(!state
            .borrow_mut()
            .remote_sessions
            .listings
            .entry(tab)
            .or_default()
            .ask(Instant::now(), || unreachable!("asked within ten seconds of the last")),);
        note_list_awaited(&mut state.borrow_mut(), tab, ViewId::BASE);
        assert!(state.borrow().remote_sessions.awaiting_list.is_empty());
        // While an ask is under way the chooser is sent the list when it comes.
        let (_hold, release) = mpsc::channel::<()>();
        state
            .borrow_mut()
            .remote_sessions
            .listings
            .insert(tab, SessionListing::default());
        state
            .borrow_mut()
            .remote_sessions
            .listings
            .get_mut(&tab)
            .unwrap()
            .ask(Instant::now(), || {
                spawn_listing(move || {
                    let _ = release.recv();
                    Err("held".into())
                })
            });
        note_list_awaited(&mut state.borrow_mut(), tab, ViewId::BASE);
        assert_eq!(
            state.borrow().remote_sessions.awaiting_list.get(&tab),
            Some(&ViewId::BASE)
        );
    }

    fn wait_for_list(state: &Rc<RefCell<AgentPanelState>>, tab: TabId) {
        let started = Instant::now();
        while !state
            .borrow_mut()
            .remote_sessions
            .listings
            .get_mut(&tab)
            .unwrap()
            .collect()
        {
            assert!(started.elapsed() < Duration::from_secs(30), "the list never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
