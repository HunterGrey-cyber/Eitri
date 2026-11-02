//! The editor's "edit the selection" key, panel side. The editor stamps each press on the context it feeds
//! ([`EditorContext::edit_request`]); the tick that reads a stamp it has not seen asks the window to put the
//! keys in the chat ([`Effect::EditSelection`]), and the window, once they are there, calls
//! [`AgentPanelHandle::edit_selection`] for the view that took them: the shown tab opens its composer with the
//! prompt ready, an ordinary turn from then on.

use eitri_core::editor_context::EditorContext;

use super::*;

/// What the composer starts with when the key was pressed on an empty draft. The selection itself rides on the
/// turn as every selection does; this is only the start of the sentence the person finishes.
pub(super) const PROMPT: &str = "Edit the selection: ";

/// Which presses the panel has answered.
pub(super) struct EditAsk {
    /// A press stamped before this (milliseconds since the epoch, nvim's clock and this one the same machine's) is
    /// not news: the second window of a pair is handed the editor's cache, and a press made before it opened is in it.
    not_before: u64,
    seen: Option<u64>,
}

impl Default for EditAsk {
    fn default() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        Self::starting_at(now)
    }
}

impl EditAsk {
    fn starting_at(not_before: u64) -> Self {
        Self { not_before, seen: None }
    }

    /// Whether `context` carries a press this panel has not answered, and was made since it began.
    fn is_new(&mut self, context: &EditorContext) -> bool {
        let Some(ask) = context.edit_request else {
            return false;
        };
        if self.seen == Some(ask) {
            return false;
        }
        self.seen = Some(ask);
        ask >= self.not_before
    }
}

/// The tick's read of the editor context: a press not answered yet asks the window for the keys.
pub(super) fn poll(state: &mut AgentPanelState, context: Option<&EditorContext>, out: &mut Outbox) {
    if context.is_some_and(|context| state.edit_ask.is_new(context)) {
        out.effect(Effect::EditSelection);
    }
}

/// The keys are in `view`: its shown tab opens the composer. The prompt goes in only when the draft is empty (a
/// half-typed message is the person's, and stays); the caret lands at its end.
pub(super) fn open_composer(state: &mut AgentPanelState, view: ViewId, out: &mut Outbox) {
    let tab = if view.is_base() {
        Some(state.tabs.active())
    } else {
        state.tabs.tab_in(view)
    };
    let Some(tab) = tab else { return };
    state.tabs.drop_bypass_prompt_in(view);
    if state.tabs.get(tab).is_some_and(|tab| tab.draft().trim().is_empty()) {
        state.tabs.set_draft(tab, PROMPT);
        out.send(view, eitri_core::agent_bridge::serialize_draft_for_js(tab, PROMPT));
    }
    out.send(view, eitri_core::agent_bridge::serialize_enter_input_for_js());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;
    use std::cell::{Cell, RefCell};

    fn context(edit_request: Option<u64>) -> EditorContext {
        EditorContext {
            file: "/p/a.rs".into(),
            selection: None,
            edit_request,
        }
    }

    #[test]
    fn a_press_from_before_this_panel_began_is_not_news() {
        let mut ask = EditAsk::starting_at(1000);
        assert!(
            !ask.is_new(&context(Some(999))),
            "a press the window it was handed from made before"
        );
        assert!(!ask.is_new(&context(Some(999))));
        assert!(
            ask.is_new(&context(Some(1000))),
            "one at the moment it began is its own"
        );
    }

    #[test]
    fn the_first_context_a_panel_reads_may_carry_a_press() {
        let mut ask = EditAsk::starting_at(1000);
        assert!(
            ask.is_new(&context(Some(1500))),
            "the press came before the first timer line"
        );
        assert!(!ask.is_new(&context(Some(1500))));
    }

    #[test]
    fn a_press_is_answered_once_however_many_contexts_repeat_it() {
        let mut ask = EditAsk::starting_at(0);
        assert!(!ask.is_new(&context(None)));
        assert!(ask.is_new(&context(Some(100))));
        for _ in 0..3 {
            assert!(!ask.is_new(&context(Some(100))));
        }
        assert!(ask.is_new(&context(Some(101))), "the next press is a new one");
    }

    #[test]
    fn a_context_with_no_press_answers_nothing() {
        let mut ask = EditAsk::starting_at(0);
        assert!(!ask.is_new(&context(None)));
        assert!(!ask.is_new(&context(None)));
    }

    /// A settled panel whose editor context is the one in `held`, and a count of the window's calls.
    fn panel_with_context() -> (
        AgentPanelHandle,
        Rc<RecordingPage>,
        Rc<RefCell<Option<EditorContext>>>,
        Rc<Cell<u32>>,
    ) {
        let (handle, page) = settled_panel();
        let held: Rc<RefCell<Option<EditorContext>>> = Rc::new(RefCell::new(Some(context(None))));
        let source = held.clone();
        handle.state.borrow_mut().editor_context = Rc::new(move || source.borrow().clone());
        handle.state.borrow_mut().edit_ask = EditAsk::starting_at(0);
        let calls = Rc::new(Cell::new(0));
        let counted = calls.clone();
        handle.on_edit_selection(move || counted.set(counted.get() + 1));
        handle.tick();
        (handle, page, held, calls)
    }

    #[test]
    fn each_press_asks_the_window_once_and_a_tick_with_no_news_asks_nothing() {
        let (handle, _page, held, calls) = panel_with_context();
        assert_eq!(calls.get(), 0, "no press yet");
        for press in [Some(500), Some(500), Some(500), Some(900), Some(900)] {
            *held.borrow_mut() = Some(context(press));
            handle.tick();
        }
        assert_eq!(calls.get(), 2, "the press at 500 and the one at 900, each once");
    }

    #[test]
    fn a_press_stamped_before_the_panel_began_asks_nothing() {
        let (handle, _page, held, calls) = panel_with_context();
        handle.state.borrow_mut().edit_ask = EditAsk::default();
        *held.borrow_mut() = Some(context(Some(1_000)));
        handle.tick();
        assert_eq!(calls.get(), 0, "the editor's cache held a press from long ago");
    }

    #[test]
    fn a_detached_editor_and_a_new_one_not_yet_pressed_ask_nothing() {
        let (handle, _page, held, calls) = panel_with_context();
        *held.borrow_mut() = None;
        handle.tick();
        *held.borrow_mut() = Some(context(None));
        handle.tick();
        assert_eq!(
            calls.get(),
            0,
            "a detached editor and a new one that has not been pressed"
        );
    }

    fn drafts_and_input(page: &RecordingPage, from: usize) -> (Vec<String>, usize) {
        let sent = page.sent.borrow();
        let drafts = sent[from..]
            .iter()
            .filter_map(|payload| {
                let value: serde_json::Value = serde_json::from_str(payload).ok()?;
                (value["kind"] == "draft").then(|| value["text"].as_str().unwrap().to_string())
            })
            .collect();
        let input = sent[from..]
            .iter()
            .filter(|p| p.contains(r#""kind":"enter_input""#))
            .count();
        (drafts, input)
    }

    #[test]
    fn the_composer_opens_with_the_prompt_ready_when_the_draft_was_empty() {
        let (handle, page) = settled_panel();
        let from = page.sent.borrow().len();
        handle.edit_selection(ViewId::BASE);
        let (drafts, input) = drafts_and_input(&page, from);
        assert_eq!(drafts, [PROMPT]);
        assert_eq!(input, 1, "and the caret goes to the composer");
        let state = handle.state.borrow();
        assert_eq!(
            state.tabs.active_tab().draft(),
            PROMPT,
            "Rust's copy agrees, so a reload keeps it"
        );
    }

    #[test]
    fn a_half_typed_draft_is_left_alone() {
        let (handle, page) = settled_panel();
        let tab = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.set_draft(tab, "make this fast");
        let from = page.sent.borrow().len();
        handle.edit_selection(ViewId::BASE);
        let (drafts, input) = drafts_and_input(&page, from);
        assert!(drafts.is_empty(), "nothing replaces what the person typed: {drafts:?}");
        assert_eq!(input, 1);
        assert_eq!(handle.state.borrow().tabs.active_tab().draft(), "make this fast");
    }

    #[test]
    fn a_view_with_no_tab_opens_nothing() {
        let (handle, page) = settled_panel();
        let from = page.sent.borrow().len();
        handle.edit_selection(ViewId::new(7).unwrap());
        assert_eq!(page.sent.borrow().len(), from);
    }
}
