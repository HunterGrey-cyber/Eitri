//! A message that names the editor's own sources (`@buffers`, `@quickfix`, `@diagnostics`) waits here for the editor to
//! answer, and goes on through the arm it was bound for with the answers in its turn.
//!
//! The host never blocks on the editor: [`hold`] sends the reads and keeps the message, a tick tries them
//! ([`take_ready`]), and the window replays the message through `handle_inbound_message` with the sections set aside
//! for the one compose call that reads them ([`compose`]). A message of the same tab that arrives meanwhile queues
//! behind it, so the order the person sent them in holds.
use super::routing::remember_prompts;
use super::*;
use eitri_core::mentions::gather::{Gather, Gathered};
use eitri_core::mentions::{requested, Kind};
use std::time::Instant;

struct Waiting {
    gather: Gather,
    tab: TabId,
    message: InboundMessage,
    view: ViewId,
    admitted: Admitted,
}

/// What is waiting for the editor, oldest first.
#[derive(Default)]
pub(super) struct Desk {
    waiting: Vec<Waiting>,
    sequencer: Option<eitri_core::editor_tools::stamp::Sequencer>,
}

impl Desk {
    /// How many tabs have words of the person's waiting for the editor.
    pub(super) fn tabs_holding_words(&self) -> usize {
        let mut tabs: Vec<TabId> = self
            .waiting
            .iter()
            .filter(|waiting| words_of(&waiting.message).is_some())
            .map(|waiting| waiting.tab)
            .collect();
        tabs.sort();
        tabs.dedup();
        tabs.len()
    }

    /// Whether a message waits for the editor: the tick polls on its steady cadence until it goes.
    pub(super) fn is_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }
}

/// Forgets what `view`'s page was waiting for: a page that went idle or was reloaded is a new document whose request
/// numbers start again, and an answer to the old one could match a request of the new. What the person had typed is
/// not lost with it: it is back in the tab's draft and in the prompt history (`↑`).
pub(super) fn drop_view(state: &mut AgentPanelState, view: ViewId, out: &mut Outbox) {
    let (dropped, kept): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut state.mentions.waiting)
        .into_iter()
        .partition(|waiting| waiting.view == view);
    state.mentions.waiting = kept;
    for waiting in dropped {
        let Some(text) = words_of(&waiting.message).map(str::to_owned) else {
            continue;
        };
        remember_prompts(state, std::slice::from_ref(&text), out);
        state.tabs.set_draft(waiting.tab, &text);
    }
}

/// A message held, or one that goes on as it is.
pub(super) enum Held {
    Held,
    Goes(InboundMessage, Admitted),
}

/// The text a message-bearing arm sends: the person's words, then the sections, then the editor's context block.
pub(super) fn compose(state: &AgentPanelState, text: &str) -> String {
    let with_sections = match &state.mention_sections {
        Some(gathered) => gathered.appended_to(text),
        None => text.to_owned(),
    };
    eitri_core::editor_context::compose_turn_text(&with_sections, (state.editor_context)().as_ref())
}

/// The text of a message that sends words to a Claude tab, and may name a source.
fn words_of(message: &InboundMessage) -> Option<&str> {
    match message {
        InboundMessage::SendMessage { text, .. }
        | InboundMessage::QueueMessage { text, .. }
        | InboundMessage::SendNow { text, .. } => Some(text),
        _ => None,
    }
}

/// Whether a message of a tab must not overtake one held for it: what sends, queues, runs a line or takes the queue
/// back.
fn keeps_order(message: &InboundMessage) -> bool {
    words_of(message).is_some()
        || matches!(
            message,
            InboundMessage::RunShell { .. } | InboundMessage::TakeBackQueue { .. }
        )
}

/// Holds `message` when it names a source of the editor, or follows one that is held for the same tab.
pub(super) fn hold(state: &mut AgentPanelState, message: InboundMessage, view: ViewId, admitted: Admitted) -> Held {
    let Some(tab) = admitted.target.filter(|_| keeps_order(&message)) else {
        return Held::Goes(message, admitted);
    };
    let claude = state.tabs.get(tab).is_some_and(|t| *t.agent() == AgentId::Claude);
    let behind = state.mentions.waiting.iter().any(|waiting| waiting.tab == tab);
    let kinds: Vec<Kind> = match words_of(&message) {
        Some(text) if claude => requested(text),
        _ => Vec::new(),
    };
    if kinds.is_empty() && !behind {
        return Held::Goes(message, admitted);
    }
    // The box was cleared at Enter: the host's mirror of it goes now, as it would have, and not when the message goes.
    if matches!(message, InboundMessage::SendMessage { .. }) {
        state.tabs.note_sent(tab);
    }
    let gather = Gather::start(
        state.editor_rpc.as_deref().filter(|_| !kinds.is_empty()),
        state.mentions.sequencer.get_or_insert_with(|| {
            eitri_core::editor_tools::stamp::Sequencer::new(format!(
                "mention-{}-{:x}",
                std::process::id(),
                eitri_core::editor_tools::stamp::wall_ms()
            ))
        }),
        &kinds,
        &state.project_dir,
        state.home.as_deref(),
        agent::transcript::claude_projects_dir()
            .ok()
            .and_then(|projects| projects.parent().map(Path::to_path_buf)),
        Instant::now(),
    );
    state.mentions.waiting.push(Waiting {
        gather,
        tab,
        message,
        view,
        admitted,
    });
    Held::Held
}

/// What has been answered, oldest first, each with its sections. A message waits until every one before it for its tab
/// has gone, so the order a person sent in holds.
pub(super) fn take_ready(
    state: &mut AgentPanelState,
    now: Instant,
) -> Vec<(InboundMessage, ViewId, Admitted, Gathered)> {
    let mut ready = Vec::new();
    let mut blocked: Vec<TabId> = Vec::new();
    let mut index = 0;
    while index < state.mentions.waiting.len() {
        let tab = state.mentions.waiting[index].tab;
        if blocked.contains(&tab) {
            index += 1;
            continue;
        }
        match state.mentions.waiting[index].gather.poll(now) {
            Some(gathered) => {
                let waiting = state.mentions.waiting.remove(index);
                ready.push((waiting.message, waiting.view, waiting.admitted, gathered));
            }
            None => {
                blocked.push(tab);
                index += 1;
            }
        }
    }
    ready
}

/// The tab went (closed, moved away) while its message waited: nothing is replayed into it.
pub(super) fn tab_is_still_here(state: &AgentPanelState, admitted: &Admitted) -> bool {
    admitted.target.is_some_and(|tab| state.tabs.get(tab).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;
    use eitri_core::editor_tools::testing::FakeEditor;
    use serde_json::json;

    fn send(kind: &str, request_id: &str, tab: TabId, text: &str) -> String {
        json!({ "type": kind, "request_id": request_id, "tab": tab.0, "text": text }).to_string()
    }

    fn buffer_reply(path: &Path) -> serde_json::Value {
        json!({
            "buffers": [{
                "path": path.to_str().unwrap(), "read_from": path.to_str().unwrap(), "modified": true,
                "loaded": true, "current": true, "shown": true, "lines": 1, "filetype": "rust",
                "text": ["@/home/me/.ssh/id_rsa"],
            }],
            "other": 0, "left_out": 0,
        })
    }

    fn results(page: &RecordingPage, request_id: &str) -> Vec<serde_json::Value> {
        page.sent
            .borrow()
            .iter()
            .map(|envelope| serde_json::from_str::<serde_json::Value>(envelope).unwrap())
            .filter(|envelope| envelope["kind"] == "command_result" && envelope["requestId"] == request_id)
            .collect()
    }

    struct Ready {
        handle: AgentPanelHandle,
        page: Rc<RecordingPage>,
        tab: TabId,
        provider: std::sync::Arc<eitri_core::test_providers::RecordingProvider>,
        editor: Rc<FakeEditor>,
        dir: PathBuf,
    }

    fn ready(label: &str) -> Ready {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let (handle, page, _second) = handle_with_two_pages();
        let editor = Rc::new(FakeEditor::default());
        {
            let mut state = handle.state.borrow_mut();
            state.project_dir = dir.clone();
            state.editor_rpc = Some(editor.clone());
        }
        let (tab, provider) = live_tab(&handle, &dir);
        std::fs::write(dir.join("a.rs"), "a\n").unwrap();
        handle.tick();
        Ready {
            handle,
            page,
            tab,
            provider,
            editor,
            dir,
        }
    }

    #[test]
    fn a_message_that_names_a_source_waits_for_the_editor_and_goes_with_its_sections() {
        let r = ready("panel-mentions-wait");
        r.handle.page_message(
            ViewId::BASE,
            &send("send_message", "s1", r.tab, "why is @buffers slow?"),
        );
        assert_eq!(r.editor.asked().len(), 1, "the editor was asked for the buffers");
        r.handle.tick();
        assert!(r.provider.turns().is_empty(), "nothing goes before the editor answers");
        assert!(results(&r.page, "s1").is_empty(), "and the page is not told yet");
        assert!(r.handle.state.borrow().mentions.is_waiting());
        assert!(needs_polling_now(&r.handle), "the tick keeps running while it waits");

        r.editor.answer_oldest(&buffer_reply(&r.dir.join("a.rs")));
        tick_handle_until(&r.handle, "the turn", || r.provider.turns().len() == 1);
        let turn = &r.provider.turns()[0];
        assert!(
            turn.starts_with("why is @buffers slow?\n\n[@buffers: the editor's open files"),
            "{turn}"
        );
        assert!(
            turn.contains("--- a.rs (unsaved changes, current, 1 line) ---\n1\t\u{2060}@/home/me/.ssh/id_rsa\n"),
            "{turn}"
        );
        assert!(turn.ends_with("[@buffers end]"), "{turn}");
        assert_eq!(results(&r.page, "s1").len(), 1, "answered once");
        assert!(!r.handle.state.borrow().mentions.is_waiting());
        assert!(
            r.handle.state.borrow().mention_sections.is_none(),
            "set aside for one message only"
        );
    }

    #[test]
    fn what_was_typed_without_a_name_of_a_source_is_never_held() {
        let r = ready("panel-mentions-plain");
        for text in ["no names here", "me@buffers", "`@buffers`", "/compact @buffers"] {
            r.handle
                .page_message(ViewId::BASE, &send("send_message", "s", r.tab, text));
            assert!(!r.handle.state.borrow().mentions.is_waiting(), "{text}");
            tick_handle_until(&r.handle, "the turn", || {
                r.provider.turns().last().is_some_and(|turn| turn == text)
            });
            provider_idle(&r);
        }
        assert!(r.editor.asked().is_empty(), "the editor was asked nothing");
    }

    fn provider_idle(r: &Ready) {
        r.provider
            .queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t".into() });
        r.provider.queue(agent::AgentDomainEvent::TurnCompleted {
            turn_id: "t".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        tick_handle_until(&r.handle, "the turn's end", || {
            !r.handle.state.borrow().tabs.get(r.tab).unwrap().turn_running()
        });
    }

    fn needs_polling_now(handle: &AgentPanelHandle) -> bool {
        super::super::tick::needs_polling(&handle.state.borrow())
    }

    fn begin_turn(r: &Ready, id: &str) {
        r.provider
            .queue(agent::AgentDomainEvent::TurnStarted { turn_id: id.into() });
        tick_handle_until(&r.handle, "the turn", || {
            r.handle.state.borrow().tabs.get(r.tab).unwrap().turn_running()
        });
    }

    fn end_turn(r: &Ready, id: &str) {
        r.provider.queue(agent::AgentDomainEvent::TurnCompleted {
            turn_id: id.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        tick_handle_until(&r.handle, "the turn's end", || {
            !r.handle.state.borrow().tabs.get(r.tab).unwrap().turn_running()
        });
    }

    fn quickfix_reply() -> serde_json::Value {
        json!({"title": "", "size": 0, "items": [], "left_out": 0})
    }

    fn diagnostics_reply() -> serde_json::Value {
        json!({"entries": [], "modified": [], "left_out": 0})
    }

    #[test]
    fn a_later_message_of_the_tab_waits_behind_the_held_one_and_goes_after_it() {
        let r = ready("panel-mentions-order");
        r.handle
            .page_message(ViewId::BASE, &send("send_message", "s1", r.tab, "first @diagnostics"));
        r.handle
            .page_message(ViewId::BASE, &send("queue_message", "q1", r.tab, "second"));
        r.handle.tick();
        assert!(r.provider.turns().is_empty());
        assert_eq!(
            r.handle.state.borrow().mentions.waiting.len(),
            2,
            "the plain one waits too"
        );
        r.editor.answer_oldest(&diagnostics_reply());
        tick_handle_until(&r.handle, "the first", || !r.provider.turns().is_empty());
        assert!(r.provider.turns()[0].starts_with("first @diagnostics\n\n[@diagnostics: "));
        assert!(r.provider.turns()[0].contains("(no diagnostics)"));
        assert_eq!(
            r.provider.turns().len(),
            1,
            "the second waits for the first turn: {:?}",
            r.provider.turns()
        );
        begin_turn(&r, "t1");
        end_turn(&r, "t1");
        tick_handle_until(&r.handle, "the second", || r.provider.turns().len() == 2);
        assert_eq!(r.provider.turns()[1], "second");
    }

    #[test]
    fn a_queued_message_and_a_send_now_go_with_their_sections() {
        let r = ready("panel-mentions-queue");
        r.handle
            .page_message(ViewId::BASE, &send("send_message", "s0", r.tab, "plain"));
        tick_handle_until(&r.handle, "the first turn", || r.provider.turns().len() == 1);
        begin_turn(&r, "t1");
        // A message queued during a turn goes at its end, with the section it asked for.
        r.handle
            .page_message(ViewId::BASE, &send("queue_message", "q1", r.tab, "later @diagnostics"));
        r.editor.answer_oldest(&diagnostics_reply());
        tick_handle_until(&r.handle, "the answer", || {
            !r.handle.state.borrow().mentions.is_waiting()
        });
        assert_eq!(r.provider.turns().len(), 1, "queued behind the running turn");
        end_turn(&r, "t1");
        tick_handle_until(&r.handle, "the queue", || r.provider.turns().len() == 2);
        let queued = &r.provider.turns()[1];
        assert!(
            queued.starts_with("later @diagnostics\n\n[@diagnostics: ") && queued.ends_with("[@diagnostics end]"),
            "{queued}"
        );
        begin_turn(&r, "t2");
        end_turn(&r, "t2");
        // And one that sends now.
        r.handle
            .page_message(ViewId::BASE, &send("send_now", "n1", r.tab, "now @quickfix"));
        r.editor.answer_oldest(&quickfix_reply());
        tick_handle_until(&r.handle, "the send", || r.provider.turns().len() == 3);
        let now = &r.provider.turns()[2];
        assert!(
            now.starts_with("now @quickfix\n\n[@quickfix: ") && now.contains("(the list is empty)"),
            "{now}"
        );
    }

    #[test]
    fn a_line_and_a_take_back_wait_behind_a_held_message() {
        let r = ready("panel-mentions-line");
        r.handle
            .page_message(ViewId::BASE, &send("send_message", "s1", r.tab, "see @quickfix"));
        r.handle
            .page_message(ViewId::BASE, &send("run_shell", "r1", r.tab, "!true"));
        r.handle.page_message(
            ViewId::BASE,
            &json!({"type": "take_back_queue", "request_id": "b1", "tab": r.tab.0}).to_string(),
        );
        r.handle.tick();
        assert_eq!(r.handle.state.borrow().mentions.waiting.len(), 3);
        assert!(
            !r.handle.state.borrow().tabs.shell_line_runs(r.tab),
            "the line did not overtake"
        );
        assert!(results(&r.page, "r1").is_empty() && results(&r.page, "b1").is_empty());
        r.editor.answer_oldest(&quickfix_reply());
        tick_handle_until(&r.handle, "all three", || {
            results(&r.page, "s1").len() + results(&r.page, "r1").len() + results(&r.page, "b1").len() == 3
        });
        assert!(r.provider.turns()[0].starts_with("see @quickfix\n\n[@quickfix: "));
    }

    #[test]
    fn the_box_is_cleared_at_enter_and_what_was_typed_comes_back_if_its_page_goes() {
        let r = ready("panel-mentions-draft");
        r.handle.state.borrow_mut().tabs.set_draft(r.tab, "typing on @buffers");
        r.handle
            .page_message(ViewId::BASE, &send("send_message", "s1", r.tab, "typing on @buffers"));
        assert_eq!(
            r.handle.state.borrow().tabs.get(r.tab).unwrap().draft(),
            "",
            "the host's mirror of the box goes at Enter, not when the message does"
        );
        assert_eq!(
            r.handle.unsaved_draft_count(),
            1,
            "words held for the editor go with the window if it closes"
        );
        r.handle.page_message(ViewId::BASE, READY);
        assert!(!r.handle.state.borrow().mentions.is_waiting());
        assert_eq!(
            r.handle.state.borrow().tabs.get(r.tab).unwrap().draft(),
            "typing on @buffers",
            "a reload does not lose the message"
        );
        assert!(r.provider.turns().is_empty());
    }

    #[test]
    fn a_tab_that_closed_while_its_message_waited_is_told() {
        let r = ready("panel-mentions-closed");
        r.handle
            .page_message(ViewId::BASE, &send("send_message", "s1", r.tab, "@quickfix"));
        r.handle.state.borrow_mut().tabs.remove(r.tab);
        r.editor
            .answer_oldest(&json!({"title": "", "size": 0, "items": [], "left_out": 0}));
        tick_handle_until(&r.handle, "the answer", || !results(&r.page, "s1").is_empty());
        let result = &results(&r.page, "s1")[0];
        assert_eq!(result["ok"], false, "{result}");
        assert!(
            result.to_string().contains("closed before the editor answered"),
            "{result}"
        );
        assert!(r.provider.turns().is_empty());
    }

    #[test]
    fn a_held_message_is_forgotten_when_its_page_reloads_or_goes_idle() {
        let r = ready("panel-mentions-reload");
        let extra = attach_recording_page(&r.handle, view(2));
        r.handle.page_message(view(2), READY);
        r.handle
            .page_message(ViewId::BASE, &send("send_message", "s1", r.tab, "@buffers"));
        assert!(r.handle.state.borrow().mentions.is_waiting());
        // A reloaded document numbers its requests again: nothing of the old one answers it.
        r.handle.page_message(ViewId::BASE, READY);
        assert!(!r.handle.state.borrow().mentions.is_waiting());
        r.editor.answer_oldest(&buffer_reply(&r.dir.join("a.rs")));
        for _ in 0..5 {
            r.handle.tick();
        }
        assert!(r.provider.turns().is_empty(), "the message was dropped with its page");
        assert!(results(&r.page, "s1").is_empty());
        // Only the view's own: another page's wait stays, and `idle_page` drops the same way.
        r.handle
            .page_message(ViewId::BASE, &send("send_message", "s2", r.tab, "@quickfix"));
        r.handle.run(|state, out| drop_view(state, view(2), out));
        assert!(r.handle.state.borrow().mentions.is_waiting());
        r.handle.run(|state, out| drop_view(state, ViewId::BASE, out));
        assert!(!r.handle.state.borrow().mentions.is_waiting());
        drop(extra);
    }

    #[test]
    fn with_no_editor_the_message_goes_and_says_the_source_was_not_attached() {
        let r = ready("panel-mentions-none");
        r.handle.state.borrow_mut().editor_rpc = None;
        r.handle
            .page_message(ViewId::BASE, &send("send_message", "s1", r.tab, "look @buffers"));
        tick_handle_until(&r.handle, "the turn", || r.provider.turns().len() == 1);
        assert_eq!(
            r.provider.turns()[0],
            "look @buffers\n\n[@buffers: not attached: the editor is not running]\n[@buffers end]"
        );
    }

    #[test]
    fn only_a_claude_tab_is_read_for_names() {
        let r = ready("panel-mentions-codex");
        let codex = r.handle.state.borrow_mut().tabs.open(AgentId::Codex);
        let admitted = Admitted {
            target: Some(codex),
            late_draft: false,
            admission: super::super::admission::Admission::Ungated,
        };
        let message =
            eitri_core::agent_bridge::parse_inbound_message(&send("send_message", "s1", codex, "@buffers")).unwrap();
        let held = hold(&mut r.handle.state.borrow_mut(), message, ViewId::BASE, admitted);
        assert!(matches!(held, Held::Goes(..)));
        assert!(r.editor.asked().is_empty());
    }

    /// A first send in a tab with no session, in a project that raises the trust question: held for the editor, then
    /// the question, and nothing starts before its answer.
    fn asked_first_send(label: &str) -> (crate::agent_panel::tests::TrustViews, Rc<FakeEditor>, Prompt) {
        let views = trust_views(label);
        let editor = Rc::new(FakeEditor::default());
        {
            let mut state = views.handle.state.borrow_mut();
            state.project_dir = views.fx.root.clone();
            state.editor_rpc = Some(editor.clone());
        }
        std::fs::write(views.fx.root.join("a.rs"), "a\n").unwrap();
        views
            .handle
            .state
            .borrow_mut()
            .tabs
            .set_draft(views.tab, "see @buffers");
        views
            .handle
            .page_message(view(2), &send("send_message", "s1", views.tab, "see @buffers"));
        assert!(views.handle.state.borrow().mentions.is_waiting(), "held for the editor");
        assert_eq!(views.handle.state.borrow().tabs.get(views.tab).unwrap().draft(), "");
        assert!(views.fx.spawned.borrow().is_empty());
        assert!(
            prompts(&views.second.sent.borrow()).is_empty(),
            "the question waits for the editor's answer"
        );
        editor.answer_oldest(&buffer_reply(&views.fx.root.join("a.rs")));
        poll_trust_until(&views.handle, "the question", || {
            views.handle.tick();
            !prompts(&views.second.sent.borrow()).is_empty()
        });
        let prompt = prompts(&views.second.sent.borrow()).pop().unwrap();
        (views, editor, prompt)
    }

    #[test]
    fn a_held_first_send_meets_the_trust_question_before_anything_starts_and_goes_with_its_sections() {
        let (views, _editor, prompt) = asked_first_send("panel-mentions-trust-y");
        assert!(awaiting_trust(&views.handle, views.tab));
        assert!(
            views.fx.spawned.borrow().is_empty(),
            "nothing started before the answer"
        );
        assert!(
            results(&views.second, "s1").is_empty(),
            "and the send is not answered yet"
        );
        trust_answer_from(&views.handle, view(2), "a1", &prompt, true);
        poll_trust_until(&views.handle, "the start", || !views.fx.spawned.borrow().is_empty());
        let state = views.handle.state.borrow();
        let TabBackend::Starting(pending) = state.tabs.get(views.tab).unwrap().backend() else {
            panic!("the tab connects once the record is written");
        };
        let first = pending.first_turn.as_ref().unwrap();
        assert_eq!(first.typed, "see @buffers", "the words as typed");
        assert!(
            first.wire.starts_with("see @buffers\n\n[@buffers: ") && first.wire.ends_with("[@buffers end]"),
            "{}",
            first.wire
        );
        assert!(first.typed_by_person);
    }

    #[test]
    fn a_put_off_trust_question_answers_the_held_send_so_that_its_words_return_to_the_box() {
        let (views, _editor, prompt) = asked_first_send("panel-mentions-trust-esc");
        views.handle.page_message(
            view(2),
            &json!({"type": "trust_cancel", "request_id": "c1", "tab": views.tab.0, "nonce": prompt.nonce}).to_string(),
        );
        views.handle.tick();
        assert!(views.fx.spawned.borrow().is_empty(), "nothing started");
        let answered = results(&views.second, "s1");
        assert_eq!(
            answered.len(),
            1,
            "the send is answered, once: {:?}",
            views.second.sent.borrow()
        );
        assert_eq!(
            answered[0]["ok"], false,
            "a refusal is what puts the words back in the box: {answered:?}"
        );
        assert!(!views.handle.state.borrow().mentions.is_waiting());
    }
}
