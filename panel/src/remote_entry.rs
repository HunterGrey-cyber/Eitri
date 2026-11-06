//! A remote observer's messages (a phone's): their own entry point into the panel, apart from the pages'.
//!
//! A remote message names the tab it is about by the tab's row key and never by a view: the observer is not a page,
//! so nothing it sends is handled as a page's (`page_message`), meets the page check or the window's gate, or is
//! answered with a page's `command_result`. It parses only as a [`RemoteInbound`], a closed list of what may come
//! from afar, so every kind a page may send and the list does not name (a card's answer, a question's, a trust
//! answer, a mode change, a shell line) fails to parse. What it caused is told to the page that shows its tab,
//! naming the device.

use super::*;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// Who sent a remote message: the device it was enrolled as and the connection it came on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOrigin {
    /// The device's id in the device store, which its command ids are kept under.
    pub device_id: String,
    /// The device's name, as the page that shows the tab is told it.
    pub device: String,
    pub conn: u64,
}

/// The work a stop was asked about, so a stop that arrives after that work has ended stops nothing: a turn by its id,
/// or a stretch of work the panel cannot show by its number ([`eitri_core::tab_set::Tab::unseen_stretch`]).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExpectWork {
    Turn(String),
    Unseen(u64),
}

/// Every message a remote observer may send. Anything else, and any field not named here, fails to parse.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteInbound {
    /// Stops the work running in the tab, if it is still the work named.
    Interrupt {
        command_id: String,
        expect_work: ExpectWork,
    },
}

impl RemoteInbound {
    /// The id the sender gave this command; each is taken once per device.
    pub fn command_id(&self) -> &str {
        match self {
            RemoteInbound::Interrupt { command_id, .. } => command_id,
        }
    }
}

/// Why a remote message did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteRefusal {
    /// Not a remote message: too long, not JSON, a kind or a field the list does not have, or an unusable command id.
    Parse,
    /// No tab of this window has that key.
    NoSuchTab,
    /// The tab's agent is not Claude.
    NotClaude,
    /// The tab holds no session, or is being moved to another window.
    NotLive,
    /// The work the message was about is no longer the work running.
    WorkChanged,
    /// This device sent this command id before.
    Duplicate,
    /// The agent turned the command down or failed it, in its own words.
    Agent(String),
}

/// The longest remote message read; anything longer is refused unread.
pub(super) const REMOTE_MESSAGE_MAX_BYTES: usize = 4096;

/// The longest command id taken: a 128-bit id fits many times over, and the ids are kept for the process's life.
pub(super) const COMMAND_ID_MAX_BYTES: usize = 64;

/// Parses `raw` as a remote message, and nothing else.
pub(super) fn parse_remote_inbound(raw: &str) -> Result<RemoteInbound, RemoteRefusal> {
    if raw.len() > REMOTE_MESSAGE_MAX_BYTES {
        return Err(RemoteRefusal::Parse);
    }
    let message: RemoteInbound = serde_json::from_str(raw).map_err(|_| RemoteRefusal::Parse)?;
    let id = message.command_id();
    if id.is_empty() || id.len() > COMMAND_ID_MAX_BYTES {
        return Err(RemoteRefusal::Parse);
    }
    Ok(message)
}

/// The command ids each device has sent, kept for the life of the process: a command whose result never reached the
/// phone may be sent again by the person, never by a retry that repeats the id.
#[derive(Default)]
struct RemoteCommands {
    by_device: HashMap<String, HashSet<String>>,
}

impl RemoteCommands {
    /// Records `id` for `device`; false when that device sent it before.
    fn first_use(&mut self, device: &str, id: &str) -> bool {
        self.by_device
            .entry(device.to_owned())
            .or_default()
            .insert(id.to_owned())
    }
}

thread_local! {
    /// Every window's ledger: a panel handle never leaves the thread that made it, and the process runs its windows
    /// on its one UI thread, so this thread's ledger is the process's, and a window closed or opened again forgets
    /// nothing.
    static REMOTE_COMMANDS: std::cell::RefCell<RemoteCommands> = std::cell::RefCell::default();
}

/// Records command `id` from `device` in the process's ledger; false when that device sent it before.
pub(super) fn first_command_use(device: &str, id: &str) -> bool {
    REMOTE_COMMANDS.with_borrow_mut(|commands| commands.first_use(device, id))
}

/// A parsed remote message from `origin` about the tab named `key`: admitted by [`admit_remote`], then its arm. No
/// view is taken and no request is answered on a page; the page that shows the tab gets a notice of what was done.
pub(super) fn handle_remote_message(
    state: &mut AgentPanelState,
    screen: &Screen,
    origin: &RemoteOrigin,
    key: &eitri_core::tab_set::RowKey,
    message: RemoteInbound,
    out: &mut Outbox,
) -> Result<(), RemoteRefusal> {
    let tab = admit_remote(state, &message, key)?;
    match message {
        RemoteInbound::Interrupt { .. } => {
            // The agent's work only: a `!` line the person started at the desk is theirs to stop.
            let outcome = interrupt_agent(state, tab);
            let result = match &outcome {
                Ok(()) => Ok(()),
                Err(error) => Err(RemoteRefusal::Agent(error.message.clone())),
            };
            apply_outcome(state, screen, tab, None, outcome, out);
            if result.is_ok() {
                if let Some(view) = state.shown_in(tab) {
                    let told = format!("stopped from {}", origin.device);
                    out.send(view, eitri_core::agent_bridge::serialize_notice_for_js(&told));
                }
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;
    use eitri_core::tab_set::RowKey;

    fn phone() -> RemoteOrigin {
        RemoteOrigin {
            device_id: "d1".into(),
            device: "Pixel 8".into(),
            conn: 1,
        }
    }

    fn interrupt(command_id: &str, expect_work: serde_json::Value) -> String {
        serde_json::json!({ "type": "interrupt", "command_id": command_id, "expect_work": expect_work }).to_string()
    }

    /// A new Claude tab whose provider can interrupt, opened (so selected) in the base view, and its row key.
    fn interruptible_tab(handle: &AgentPanelHandle, dir: &Path) -> (TabId, std::sync::Arc<RecordingProvider>, RowKey) {
        let provider = std::sync::Arc::new(RecordingProvider::interruptible());
        let conversation = agent::AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let mut state = handle.state.borrow_mut();
        let tab = state.tabs.open(AgentId::Claude);
        let held = state.tabs.get_mut(tab).unwrap();
        held.set_backend(TabBackend::Live(backend));
        let key = held.row_key().clone();
        (tab, provider, key)
    }

    fn active_turn(handle: &AgentPanelHandle, tab: TabId) -> Option<String> {
        let state = handle.state.borrow();
        let backend = state.tabs.get(tab).unwrap().live()?;
        let turn = backend.projection().active_turn_id.clone();
        turn
    }

    fn start_turn(handle: &AgentPanelHandle, tab: TabId, provider: &RecordingProvider, turn: &str) {
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        tick_handle_until(handle, "the turn", || active_turn(handle, tab).as_deref() == Some(turn));
    }

    fn completed(turn: &str) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    fn unseen(handle: &AgentPanelHandle, tab: TabId) -> (bool, u64) {
        let state = handle.state.borrow();
        let held = state.tabs.get(tab).unwrap();
        (held.cli_unseen_busy(), held.unseen_stretch())
    }

    #[test]
    fn remote_inbound_refuses_desk_only_kinds() {
        let interrupt = r#"{"type":"interrupt","command_id":"c1","expect_work":{"turn":"t1"}}"#;
        assert!(
            serde_json::from_str::<RemoteInbound>(interrupt).is_ok(),
            "the one remote kind parses"
        );
        for kind in [
            "permission_response",
            "question_answer",
            "run_shell",
            "cycle_mode",
            "confirm_bypass",
            "trust_answer",
            "send_message",
            "open_path",
            "ready",
            "turn_rendered",
        ] {
            let raw = format!(r#"{{"type":"{kind}","command_id":"c1","expect_work":{{"turn":"t1"}}}}"#);
            assert!(
                serde_json::from_str::<RemoteInbound>(&raw).is_err(),
                "{kind} is the desk's alone"
            );
            assert_eq!(parse_remote_inbound(&raw), Err(RemoteRefusal::Parse), "{kind}");
        }
    }

    #[test]
    fn remote_inbound_refuses_unknown_fields() {
        let named_tab = r#"{"type":"interrupt","command_id":"c1","expect_work":{"turn":"t"},"tab":3}"#;
        assert!(serde_json::from_str::<RemoteInbound>(named_tab).is_err());
        assert_eq!(parse_remote_inbound(named_tab), Err(RemoteRefusal::Parse));
        let both = r#"{"type":"interrupt","command_id":"c1","expect_work":{"turn":"t","unseen":1}}"#;
        assert!(serde_json::from_str::<RemoteInbound>(both).is_err(), "one kind of work");
        let without = r#"{"type":"interrupt","command_id":"c1","expect_work":{"turn":"t"}}"#;
        assert_eq!(
            serde_json::from_str::<RemoteInbound>(without).unwrap(),
            RemoteInbound::Interrupt {
                command_id: "c1".into(),
                expect_work: ExpectWork::Turn("t".into()),
            }
        );
        let stretch = r#"{"type":"interrupt","command_id":"c1","expect_work":{"unseen":2}}"#;
        assert!(matches!(
            serde_json::from_str::<RemoteInbound>(stretch).unwrap(),
            RemoteInbound::Interrupt {
                expect_work: ExpectWork::Unseen(2),
                ..
            }
        ));
    }

    #[test]
    fn remote_interrupt_resolves_by_key_not_tab_number() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-by-key");
        let (handle, _base, _second) = handle_with_two_pages();
        let (first, first_provider, first_key) = interruptible_tab(&handle, &dir);
        let (second, second_provider, _) = interruptible_tab(&handle, &dir);
        start_turn(&handle, first, &first_provider, "t1");
        start_turn(&handle, second, &second_provider, "t1");
        assert_eq!(
            handle.state.borrow().tabs.active(),
            second,
            "the other tab is the one on screen"
        );

        assert_eq!(
            handle.remote_message(
                &phone(),
                &first_key,
                &interrupt("c1", serde_json::json!({ "turn": "t1" }))
            ),
            Ok(())
        );
        assert_eq!(first_provider.interrupts(), 1, "the tab with that key stopped");
        assert_eq!(second_provider.interrupts(), 0, "the other running tab is untouched");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_unknown_key_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-unknown-key");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, _) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(
            handle.remote_message(
                &phone(),
                &RowKey::new(),
                &interrupt("c1", serde_json::json!({ "turn": "t1" }))
            ),
            Err(RemoteRefusal::NoSuchTab)
        );
        assert_eq!(provider.interrupts(), 0, "nothing stopped");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_for_an_ended_turn_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-ended-turn");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        provider.queue(completed("t1"));
        start_turn(&handle, tab, &provider, "t2");

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "turn": "t1" }))),
            Err(RemoteRefusal::WorkChanged)
        );
        assert_eq!(provider.interrupts(), 0, "nothing stopped");
        assert_eq!(active_turn(&handle, tab).as_deref(), Some("t2"), "t2 still runs");
        // A stretch of unseen work is not a turn: while one runs, a stop about a stretch stops nothing.
        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c2", serde_json::json!({ "unseen": 0 }))),
            Err(RemoteRefusal::WorkChanged)
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_for_an_ended_unseen_stretch_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-ended-stretch");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        assert_eq!(unseen(&handle, tab), (false, 0), "no stretch yet");
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        tick_handle_until(&handle, "the first stretch", || unseen(&handle, tab) == (true, 1));
        // More of the same stretch is not a new one.
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        provider.queue(completed(agent::NO_OPEN_TURN_ID));
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        tick_handle_until(&handle, "the second stretch", || unseen(&handle, tab) == (true, 2));

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "unseen": 1 }))),
            Err(RemoteRefusal::WorkChanged)
        );
        assert_eq!(provider.interrupts(), 0, "nothing stopped");
        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c2", serde_json::json!({ "unseen": 2 }))),
            Ok(()),
            "the stretch running now is stopped"
        );
        assert_eq!(provider.interrupts(), 1);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_works_in_bypass() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-bypass");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        for round in 0..3 {
            if mode_of(&handle, tab) == SessionModeChoice::Bypass {
                break;
            }
            clear(&[&base]);
            cycle_mode_from(&handle, ViewId::BASE, &format!("m{round}"), tab);
            if let Some((_, nonce)) = last_bypass_prompt(&base) {
                confirm_bypass_from(&handle, ViewId::BASE, &format!("y{round}"), tab, nonce);
            }
        }
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Bypass);
        start_turn(&handle, tab, &provider, "t1");

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "turn": "t1" }))),
            Ok(())
        );
        assert_eq!(provider.interrupts(), 1, "the running turn was stopped");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_leaves_a_desk_shell_line_running() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-shell-line");
        let (handle, base, _second) = handle_with_two_pages();
        handle.state.borrow_mut().project_dir = dir.clone();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        let line = serde_json::json!({ "type": "run_shell", "request_id": "r1", "tab": tab.0, "text": "!sleep 20" });
        handle.page_message(ViewId::BASE, &line.to_string());
        assert_eq!(result_for(&base.sent.borrow(), "r1"), Some(Ok(())));
        assert!(handle.state.borrow().tabs.shell_line_runs(tab), "the desk's line runs");
        start_turn(&handle, tab, &provider, "t1");

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "turn": "t1" }))),
            Ok(())
        );
        assert_eq!(provider.interrupts(), 1, "the agent's turn was stopped");
        handle.tick();
        assert!(
            handle.state.borrow().tabs.shell_line_runs(tab),
            "the line typed at the desk runs on"
        );
        handle.state.borrow_mut().tabs.interrupt_shell_line(tab);
        tick_handle_until(&handle, "the line's end", || {
            !handle.state.borrow().tabs.shell_line_runs(tab)
        });
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_never_allows_a_pending_card() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-card");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || waiting(&handle, tab, "perm-1"));

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "turn": "t1" }))),
            Ok(())
        );
        for _ in 0..5 {
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let decisions = provider.decisions();
        assert!(
            decisions.iter().all(|(_, decision)| !decision.allows()),
            "the backend never received an allow: {decisions:?}"
        );
        let denied = decisions.iter().any(|(id, _)| id == "perm-1");
        assert!(
            waiting(&handle, tab, "perm-1") || denied,
            "the card is still waiting or was denied"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_sends_the_desk_no_command_result() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-no-result");
        let (handle, base, second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        clear(&[&base, &second]);

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "turn": "t1" }))),
            Ok(())
        );
        handle.tick();
        for page in [&base, &second] {
            assert!(
                page.sent.borrow().iter().all(|p| kind_of(p) != "command_result"),
                "no page is answered for a message it did not send"
            );
        }
        let told = notices(&base.sent.borrow());
        assert_eq!(told.len(), 1, "one notice: {told:?}");
        assert!(told[0].contains("Pixel 8"), "it names the device: {told:?}");
        assert!(
            notices(&second.sent.borrow()).is_empty(),
            "a page that does not show the tab is told nothing"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn repeated_command_id_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-repeated-id");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        let stop = interrupt("c1", serde_json::json!({ "turn": "t1" }));

        assert_eq!(handle.remote_message(&phone(), &key, &stop), Ok(()));
        assert_eq!(
            handle.remote_message(&phone(), &key, &stop),
            Err(RemoteRefusal::Duplicate)
        );
        let on_another_connection = RemoteOrigin { conn: 2, ..phone() };
        assert_eq!(
            handle.remote_message(&on_another_connection, &key, &stop),
            Err(RemoteRefusal::Duplicate),
            "the same device on a new connection"
        );
        assert_eq!(provider.interrupts(), 1, "stopped once");
        let other = RemoteOrigin {
            device_id: "d2".into(),
            device: "Tablet".into(),
            conn: 3,
        };
        assert_eq!(
            handle.remote_message(&other, &key, &stop),
            Ok(()),
            "another device's ids are its own"
        );
        shut_down_every_tab(&handle);
    }

    /// The projection's fold revision, read without a tick.
    fn folded_through(handle: &AgentPanelHandle, tab: TabId) -> u64 {
        let state = handle.state.borrow();
        let revision = state.tabs.get(tab).unwrap().live().unwrap().projection().last_revision;
        revision
    }

    #[test]
    fn a_stop_for_unseen_work_folded_since_the_last_tick_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-stretch-unpumped");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        tick_handle_until(&handle, "the first stretch", || unseen(&handle, tab) == (true, 1));
        let counted = folded_through(&handle, tab);
        // The first stretch ends and a second begins, both folded and neither seen by a tick yet: the projection
        // shows work running, as it did for the first.
        provider.queue_all(vec![
            completed(agent::NO_OPEN_TURN_ID),
            agent::AgentDomainEvent::CliWorkUnseen,
        ]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while folded_through(&handle, tab) < counted + 2 {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for the fold");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            unseen(&handle, tab),
            (true, 1),
            "no tick has counted the second stretch"
        );

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "unseen": 1 }))),
            Err(RemoteRefusal::WorkChanged)
        );
        assert_eq!(provider.interrupts(), 0, "the second stretch was not stopped");
        tick_handle_until(&handle, "the second stretch", || unseen(&handle, tab) == (true, 2));
        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c2", serde_json::json!({ "unseen": 2 }))),
            Ok(()),
            "once counted, the stretch running now is stopped"
        );
        assert_eq!(provider.interrupts(), 1);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn command_ids_outlive_the_handle_that_took_them() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-ids-across-handles");
        let stop = interrupt("c1", serde_json::json!({ "turn": "t1" }));
        let (first, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&first, &dir);
        start_turn(&first, tab, &provider, "t1");
        assert_eq!(first.remote_message(&phone(), &key, &stop), Ok(()));

        let (other, _other_base, _other_second) = handle_with_two_pages();
        let (other_tab, other_provider, other_key) = interruptible_tab(&other, &dir);
        start_turn(&other, other_tab, &other_provider, "t1");
        assert_eq!(
            other.remote_message(&phone(), &other_key, &stop),
            Err(RemoteRefusal::Duplicate),
            "another window of the process knows the id"
        );
        shut_down_every_tab(&first);
        drop(first);
        assert_eq!(
            other.remote_message(&phone(), &other_key, &stop),
            Err(RemoteRefusal::Duplicate),
            "the window that took it is gone, the id is still spent"
        );
        assert_eq!(other_provider.interrupts(), 0, "nothing stopped by a repeated id");
        shut_down_every_tab(&other);
    }
}
