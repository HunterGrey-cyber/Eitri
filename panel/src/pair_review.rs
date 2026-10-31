//! The panel's side of turn review across a linked pair of windows: the window reaches this panel's review flow
//! and tab set through [`AgentPanelHandle::with_turn_review`], and hears after every tick what the pair's link
//! owes the other windows ([`PairOutbound`], taken by [`take_pair_outbound`]): what window 1's overlay driver found
//! for each linked window, this window's turn starts and ends, and whether the partners must be told that a turn's
//! presence could not be confirmed. What the link carries back comes in through the same `with_turn_review`, so every
//! answer the flow makes and every prompt the presence lets go reaches the page that waits for it, as it does
//! from the panel's own requests.

use super::*;
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::review_editor::LinkedEvent;
use eitri_core::tab_set::{Sent, TabSet};
use eitri_core::turn_review::{Out, ReviewFlow};
use eitri_core::window_link::TurnEdge;
use eitri_core::wm::PeerId;
use std::collections::BTreeMap;

/// What a step on the turn review leaves for the pages: the flow's answers, and the prompts the tab set let go.
#[derive(Default)]
pub struct ReviewSettled {
    pub outs: Vec<Out>,
    pub sent: Vec<Sent>,
}

impl ReviewSettled {
    pub fn outs(outs: Vec<Out>) -> ReviewSettled {
        ReviewSettled { outs, sent: Vec::new() }
    }

    pub fn sent(sent: Vec<Sent>) -> ReviewSettled {
        ReviewSettled { outs: Vec::new(), sent }
    }
}

/// What the link owes the other window after a tick.
#[derive(Debug, Default, PartialEq)]
pub struct PairOutbound {
    /// What window 1's overlay driver found for each linked window, in order.
    pub linked: BTreeMap<PeerId, Vec<LinkedEvent>>,
    /// This window's turn starts and ends since the last tick.
    pub edges: Vec<TurnEdge>,
    /// A prompt's turn could not be confirmed as running: the partner is to count this window as another one.
    pub presence_false: bool,
    /// What jobs finished here for sessions whose chat moved to the other window, each with its session, oldest
    /// first: the holder records them.
    pub results: Vec<(String, serde_json::Value)>,
}

impl PairOutbound {
    pub fn is_empty(&self) -> bool {
        self.linked.is_empty() && self.edges.is_empty() && !self.presence_false && self.results.is_empty()
    }
}

/// Takes what the link owes the other window from `flow` and `tabs`.
pub fn take_pair_outbound(flow: &mut ReviewFlow, tabs: &mut TabSet) -> PairOutbound {
    let mut results = Vec::new();
    let away = tabs
        .turn_review()
        .map(|review| review.moved_away_sessions())
        .unwrap_or_default();
    for session in away {
        while let Some(result) = flow.job_result_for_holder(tabs, &session) {
            results.push((session.clone(), result));
        }
    }
    // A window with turn review switched off still runs turns the partner's overlap notes must count.
    let edges = match tabs.turn_review_mut() {
        Some(review) => review.take_turn_edges(),
        None => tabs.take_untracked_turn_edges(eitri_core::turn_review::monotonic_ns()),
    };
    PairOutbound {
        linked: flow.take_linked_events(),
        edges,
        presence_false: tabs.take_presence_false_owed(),
        results,
    }
}

/// A step on the window's review flow, its tab set and its editor, if it has one.
pub type ReviewStep<'a> = &'a mut dyn FnMut(&mut ReviewFlow, &mut TabSet, Option<&dyn EditorRpc>) -> ReviewSettled;

impl AgentPanelHandle {
    /// Runs `step` on this window's turn review, after the flow was told who owns the editor's review module, and
    /// delivers what it left: each answer to the page whose request it is, each prompt let go as the tick delivers
    /// one.
    pub fn with_turn_review(&self, step: ReviewStep<'_>) {
        self.wake();
        self.run_seen(|state, screen, out| {
            if state.shutting_down {
                return;
            }
            state.review_editor.sync(&mut state.review_flow);
            let settled = step(&mut state.review_flow, &mut state.tabs, state.editor_rpc.as_deref());
            dispatch_review_outs(state, screen, settled.outs, out);
            for sent in settled.sent {
                apply_sent(state, screen, sent, out);
            }
        });
    }

    /// Where what the link owes the other window goes, told after each tick that left any. `main.rs` installs this
    /// once, in a window of a linked pair.
    pub fn on_pair_outbound(&self, hook: impl Fn(PairOutbound) + 'static) {
        self.wake();
        self.state.borrow_mut().pair_outbound_hook = Some(Rc::new(hook));
    }
}

/// After a tick: what the link owes the other window, for the window's link once the state is released. Nothing is
/// taken in a window that installed no hook.
pub(super) fn pair_outbound(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.pair_outbound_hook.is_none() {
        return;
    }
    let outbound = take_pair_outbound(&mut state.review_flow, &mut state.tabs);
    if !outbound.is_empty() {
        out.effect(Effect::PairOutbound(outbound));
    }
}
