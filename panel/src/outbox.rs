//! What a step of the panel owes the pages and the window, delivered once the step has let the panel's
//! state go.
//!
//! A step is a function over `&mut AgentPanelState`: it changes the state and records, in order, what it
//! owes in an [`Outbox`] -- a payload for a page, a call into the window. No step calls a page or a window
//! hook itself. [`Outbox::deliver`] makes those calls afterwards with no borrow of the state held, because
//! a window hook calls back into the panel (it moves the layout's focus, opens a tab, idles a page) and
//! would find the state still borrowed: inside a WebKit signal handler that panic cannot unwind, and the
//! whole window aborts. What the window is asked before a step -- which pages are on screen ([`Screen`]),
//! whether an answer may be taken ([`ask_gate`]) -- is read here as well, with the state released, and
//! handed to the step as a value.
//!
//! This file is the only place a window hook is read out of the state.

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::attention::Attention;
use eitri_core::keymap::TabAction;
use eitri_core::layout::Direction;
use eitri_core::scratch::ScratchRequest;

use super::{AgentPanelState, EditorThen, HintInbound, Pages, TabId, ViewId};

/// Which page a payload goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dest {
    /// This view's page: an answer to a request it sent, or what that page shows.
    Page(ViewId),
    /// The page of this tab's view, decided when it is delivered: what is said about the tab.
    Tab(TabId),
}

/// A page's `tabs` envelope, numbered in the order the panel recorded it. The page shows the strip it
/// was sent last, so one recorded before a strip the page already has would put back tabs that are gone
/// or an active tab that is not: [`Outbox::deliver`] drops it. That happens when a window hook run during
/// delivery switches tabs, and the step that hook starts delivers its own, later strip ahead of the rest
/// of the outer outbox.
pub(super) struct Strip {
    pub(super) seq: u64,
    pub(super) payload: String,
}

/// What a page is sent: a payload as it is, or its `tabs` envelope, which a later one supersedes.
pub(super) enum Payload {
    Plain(String),
    Tabs(Strip),
}

impl Payload {
    /// The envelope's text.
    #[cfg(test)]
    pub(super) fn text(&self) -> &str {
        match self {
            Payload::Plain(text) => text,
            Payload::Tabs(strip) => &strip.payload,
        }
    }
}

impl From<String> for Payload {
    fn from(text: String) -> Self {
        Payload::Plain(text)
    }
}

impl From<Strip> for Payload {
    fn from(strip: Strip) -> Self {
        Payload::Tabs(strip)
    }
}

/// One thing a step owes, in the order it owes it.
pub(super) enum Effect {
    /// A payload for a page; sent only if that page is live when it is delivered, and a `tabs` envelope
    /// only if no later one has reached that page by then.
    Send(Dest, Payload),
    /// One line in the window's toast.
    Toast(String),
    /// The window shows this view and gives it the keys.
    ShowView(ViewId),
    /// This extra view holds no tab any more: the window removes it and idles its page.
    ViewEmpty(ViewId),
    /// The window's attention changed (or one tab's did), with the views handed a card since the last
    /// report.
    Attention {
        before: Attention,
        after: Attention,
        handed: Vec<ViewId>,
    },
    /// Running or queued work changed in these views.
    Busy { changed: Vec<(ViewId, bool)> },
    /// A draft edited in nvim came back: the keys return to this chat view.
    EditorDone(ViewId),
    /// The editor's "edit the selection" key was pressed: the window puts the keys in the chat.
    EditSelection,
    /// The editor opened a file for the review: the window brings the editor to the user.
    EditorOpened,
    /// One of the panel's HINT messages, for the window's HINT session.
    Hint(HintInbound),
    /// A tab verb from a page, with that page's view.
    TabVerb(ViewId, TabAction),
    /// A focus move out of a page, with that page's view.
    Nav(ViewId, Direction),
    /// The `?` overlay a page showed was closed.
    KeymapClosed { opening: u64, return_keys: bool },
    /// An address already checked, for the system's handler.
    OpenExternal(String),
    /// A request for nvim: the window shows the editor and hands it the request. What the window answers
    /// decides `then`, which is a step of its own.
    AskEditor { request: ScratchRequest, then: EditorThen },
    /// What the link owes the other window of a linked pair, for the window's link.
    PairOutbound(super::PairOutbound),
    /// A move's answer for the window: a tab frozen or not, a presence held or not.
    ChatMove(super::ChatMoveEvent),
}

/// What one step owes, in order. A trust step also keeps its toast and whether it started a restore apart
/// (`toast`, `restored`): those are said once, after everything else the step owes, however many trust
/// steps were folded into it with [`Outbox::extend`], and [`Outbox::settled`] turns them into effects.
#[derive(Default)]
pub(super) struct Outbox {
    effects: Vec<Effect>,
    /// The toast a trust step owes; the last one set wins.
    pub(super) toast: Option<String>,
    /// A trust step started a restore: its one-line result for the base page's band (`None` when it has
    /// nothing to say). Sent after the step's payloads and its toast, and then whether the restore is
    /// already over is checked.
    pub(super) restored: Option<Option<String>>,
}

impl Outbox {
    /// `payload` for the page `dest` names.
    pub(super) fn push(&mut self, dest: Dest, payload: impl Into<Payload>) {
        self.effects.push(Effect::Send(dest, payload.into()));
    }

    /// Each of `payloads`, in order, for the page `dest` names.
    pub(super) fn push_all<P: Into<Payload>>(&mut self, dest: Dest, payloads: impl IntoIterator<Item = P>) {
        for payload in payloads {
            self.push(dest, payload);
        }
    }

    /// `payload` for `view`'s page.
    pub(super) fn send(&mut self, view: ViewId, payload: impl Into<Payload>) {
        self.push(Dest::Page(view), payload);
    }

    /// Each of `payloads`, in order, for `view`'s page.
    pub(super) fn send_all<P: Into<Payload>>(&mut self, view: ViewId, payloads: impl IntoIterator<Item = P>) {
        self.push_all(Dest::Page(view), payloads);
    }

    /// A call into the window.
    pub(super) fn effect(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    /// `other` after what this outbox holds. A toast or a restore `other` set replaces this outbox's own:
    /// a trust step says one of each, after the rest.
    pub(super) fn extend(&mut self, other: Outbox) {
        self.effects.extend(other.effects);
        if other.toast.is_some() {
            self.toast = other.toast;
        }
        if other.restored.is_some() {
            self.restored = other.restored;
        }
    }

    /// The toast and the restore this outbox kept apart, made effects after the rest, in the order a trust
    /// step owes them: the toast, then each extra page shown the tab a restore brought back into its view,
    /// the restore's own line in the base page's band, and its one message once it is over.
    pub(super) fn settled(mut self, state: &mut AgentPanelState, screen: &Screen) -> Outbox {
        if let Some(toast) = self.toast.take() {
            self.effects.push(Effect::Toast(toast));
        }
        if let Some(notice) = self.restored.take() {
            super::send_extra_views(state, screen, &mut self);
            if let Some(notice) = notice {
                self.send(ViewId::BASE, eitri_core::agent_bridge::serialize_notice_for_js(&notice));
            }
            super::finish_restore(state, &mut self);
        }
        self
    }

    /// Every payload, in order, whichever page it is for.
    #[cfg(test)]
    pub(super) fn payloads(&self) -> Vec<String> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send(_, payload) => Some(payload.text().to_owned()),
                _ => None,
            })
            .collect()
    }

    /// The page of each payload, in the order of [`Outbox::payloads`].
    #[cfg(test)]
    pub(super) fn dests(&self) -> Vec<Dest> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send(dest, _) => Some(*dest),
                _ => None,
            })
            .collect()
    }

    /// Makes every call this outbox owes, in order, each with the panel's state released: a payload goes
    /// to its page if that page is live, a window hook is read out of the state and the state let go
    /// before the hook runs, and a step that follows the window's answer runs under its own short borrow.
    pub(super) fn deliver(self, state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
        let Outbox {
            effects,
            toast,
            restored,
        } = self;
        debug_assert!(
            restored.is_none(),
            "a restore a trust step started is settled before delivery"
        );
        if restored.is_some() {
            eprintln!("[agent_panel] a restore's result reached delivery unsettled; its band line is dropped");
        }
        let mut run: Option<(ViewId, Vec<String>)> = None;
        let flush_run = |run: &mut Option<(ViewId, Vec<String>)>| {
            if let Some((view, payloads)) = run.take() {
                pages.send_many(view, &payloads.iter().map(String::as_str).collect::<Vec<_>>());
            }
        };
        for effect in effects.into_iter().chain(toast.map(Effect::Toast)) {
            // A hook may switch tabs and deliver a newer strip before this outbox continues.
            if !matches!(&effect, Effect::Send(..)) {
                flush_run(&mut run);
            }
            match effect {
                Effect::Send(dest, payload) => {
                    let view = super::dest_view(&state.borrow(), dest);
                    if run.as_ref().is_some_and(|(before, _)| *before != view) {
                        flush_run(&mut run);
                    }
                    let text = match payload {
                        Payload::Plain(text) => text,
                        Payload::Tabs(strip) => {
                            if !super::strip_still_current(&mut state.borrow_mut(), view, strip.seq) {
                                continue;
                            }
                            strip.payload
                        }
                    };
                    run.get_or_insert_with(|| (view, Vec::new())).1.push(text);
                }
                Effect::Toast(text) => {
                    let hook = state.borrow().toast_hook.clone();
                    if let Some(hook) = hook {
                        hook(&text);
                    }
                }
                Effect::ShowView(view) => {
                    let hook = state.borrow().show_view_hook.clone();
                    if let Some(hook) = hook {
                        hook(view);
                    }
                }
                Effect::ViewEmpty(view) => {
                    let hook = state.borrow().view_empty_hook.clone();
                    if let Some(hook) = hook {
                        hook(view);
                    }
                }
                Effect::Attention { before, after, handed } => {
                    let hook = state.borrow().attention_hook.clone();
                    if let Some(hook) = hook {
                        hook(before, after, &handed);
                    }
                }
                Effect::Busy { changed } => {
                    let hook = state.borrow().busy_hook.clone();
                    if let Some(hook) = hook {
                        hook(&changed);
                    }
                }
                Effect::EditorDone(view) => {
                    let hook = state.borrow().editor_done_hook.clone();
                    if let Some(hook) = hook {
                        hook(view);
                    }
                }
                Effect::EditSelection => {
                    let hook = state.borrow().edit_selection_hook.clone();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                Effect::PairOutbound(outbound) => {
                    let hook = state.borrow().pair_outbound_hook.clone();
                    if let Some(hook) = hook {
                        hook(outbound);
                    }
                }
                Effect::ChatMove(event) => {
                    let hook = state.borrow().chat_moves.hook.clone();
                    match hook {
                        Some(hook) => hook(event),
                        None => eprintln!("[chat_move] BUG: {event:?} for a window that moves no chat"),
                    }
                }
                Effect::EditorOpened => {
                    let hook = state.borrow().review_editor.opened_hook();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                Effect::Hint(hint) => {
                    let hook = state.borrow().hint_hook.clone();
                    if let Some(hook) = hook {
                        hook(hint);
                    }
                }
                Effect::TabVerb(view, action) => {
                    let hook = state.borrow().tab_verb_hook.clone();
                    if let Some(hook) = hook {
                        hook(view, action);
                    }
                }
                Effect::Nav(view, direction) => {
                    let hook = state.borrow().nav_fallthrough_hook.clone();
                    if let Some(hook) = hook {
                        hook(view, direction);
                    }
                }
                Effect::KeymapClosed { opening, return_keys } => {
                    let hook = state.borrow().keymap_closed_hook.clone();
                    if let Some(hook) = hook {
                        hook(opening, return_keys);
                    }
                }
                Effect::OpenExternal(url) => {
                    let host = state.borrow().host.clone();
                    host.open_external(&url);
                }
                Effect::AskEditor { request, then } => {
                    let hook = state.borrow().editor_request_hook.clone();
                    let answer = match hook {
                        Some(hook) => hook(&request),
                        None => Err("the editor is not connected to this panel".to_string()),
                    };
                    let next = super::editor_answered(&mut state.borrow_mut(), then, answer);
                    next.deliver(state, pages);
                }
            }
        }
        flush_run(&mut run);
    }
}

/// Which live chat pages are on screen for attention: mapped, and no float over their view
/// (`AgentPanelHandle::set_coverage`). What finishes in a page that is not is not seen.
pub(super) struct Screen {
    shown: Vec<ViewId>,
}

impl Screen {
    /// Asks every live page whether it is mapped and the window whether a float covers it, before a step
    /// and with the state released: the window's probe may read the panel.
    pub(super) fn read(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) -> Screen {
        let coverage = state.borrow().coverage.clone();
        let shown = pages
            .live_views()
            .into_iter()
            .filter(|(view, page)| page.is_visible() && !coverage.as_ref().is_some_and(|covered| covered(*view)))
            .map(|(view, _)| view)
            .collect();
        Screen { shown }
    }

    /// No page on screen.
    #[cfg(test)]
    pub(super) fn nothing_shown() -> Screen {
        Screen { shown: Vec::new() }
    }

    /// Whether `view`'s page is on screen.
    pub(super) fn shows(&self, view: ViewId) -> bool {
        self.shown.contains(&view)
    }
}

/// The window's check on an answer from `view`'s page (`AgentPanelHandle::set_answer_gate`), asked with the
/// state released: the gate reads the panel. `Err(why)` refuses the answer; with no gate set, every answer
/// is taken.
pub(super) fn ask_gate(state: &Rc<RefCell<AgentPanelState>>, view: ViewId) -> Result<(), String> {
    let gate = state.borrow().answer_gate.clone();
    gate.map_or(Ok(()), |gate| gate(view))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    #[test]
    fn consecutive_payloads_for_one_page_go_in_one_send() {
        let (handle, page, _) = handle_with_recording_page();
        let mut out = Outbox::default();
        out.send(
            ViewId::BASE,
            Strip {
                seq: 1,
                payload: "tabs".into(),
            },
        );
        out.send_all(ViewId::BASE, ["snapshot".to_owned(), "draft".to_owned()]);
        out.deliver(&handle.state, &handle.pages);
        assert_eq!(*page.batches.borrow(), [vec!["tabs", "snapshot", "draft"]]);
    }

    #[test]
    fn an_effect_between_payloads_splits_the_send() {
        let (handle, page, _) = handle_with_recording_page();
        let during_hook = page.clone();
        let state = handle.state.clone();
        let pages = handle.pages.clone();
        handle.state.borrow_mut().toast_hook = Some(Rc::new(move |_| {
            assert_eq!(*during_hook.batches.borrow(), [vec!["before"]]);
            let mut nested = Outbox::default();
            nested.send(
                ViewId::BASE,
                Strip {
                    seq: 2,
                    payload: "new tabs".into(),
                },
            );
            nested.deliver(&state, &pages);
        }));
        let mut out = Outbox::default();
        out.send(ViewId::BASE, "before".to_owned());
        out.effect(Effect::Toast("switch".into()));
        out.send(
            ViewId::BASE,
            Strip {
                seq: 1,
                payload: "stale tabs".into(),
            },
        );
        out.send(ViewId::BASE, "after".to_owned());
        out.deliver(&handle.state, &handle.pages);
        handle.state.borrow_mut().toast_hook = None;
        assert_eq!(
            *page.batches.borrow(),
            [vec!["before"], vec!["new tabs"], vec!["after"]]
        );
    }
}
