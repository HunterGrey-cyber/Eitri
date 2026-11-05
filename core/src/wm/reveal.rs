//! Reveals one local chat tab and lands in its own page without answering anything there.

use super::focus;
use super::host::{Batch, ChatOp, HostOp};
use super::verbs::modules::{self, Cx};
use super::window::{WinId, Window};
use crate::agents::Intent;
use crate::tabs::TabId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevealRequest {
    /// With no tab, return to the module that last held this window's keys.
    pub tab: Option<TabId>,
    pub card_first: bool,
    pub intent: Intent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealRefusal {
    Gone,
    Moving,
}

/// Shows the tab's module, selects the tab, then gives its page the keys. Arrival runs only after a
/// successful grab. The mutable window keeps mapping, focus, lifecycle and save bookkeeping together.
pub fn tab_reveal(w: &mut Window, cx: &Cx<'_>, win: WinId, req: &RevealRequest) -> Result<Vec<HostOp>, RevealRefusal> {
    let batch = reveal_batch(w, cx, win, req)?;
    Ok(batch.ops().cloned().collect())
}

pub(super) fn reveal_batch(
    w: &mut Window,
    cx: &Cx<'_>,
    win: WinId,
    req: &RevealRequest,
) -> Result<Batch, RevealRefusal> {
    let view = if let Some(tab) = req.tab {
        let view = cx.reads.view_of_tab(tab).ok_or(RevealRefusal::Gone)?;
        if cx.reads.tab_moving(tab) {
            return Err(RevealRefusal::Moving);
        }
        Some(view)
    } else {
        None
    };
    let to = if let Some(view) = view {
        view.module()
    } else {
        // The layout's focus follows native module reports and stays parked when the top bar takes the keys
        // or the window becomes inactive; the previous history entry is the module before that one.
        w.layout
            .try_borrow()
            .map_err(|_| RevealRefusal::Moving)?
            .focus()
            .clone()
    };
    if !w.layout.try_borrow().map_err(|_| RevealRefusal::Moving)?.contains(&to) {
        return Err(RevealRefusal::Gone);
    }
    let mut batch = Batch::default();
    modules::show_on_screen(w, cx, win, &to, &mut batch).map_err(|_| RevealRefusal::Gone)?;
    let tail = if let Some(view) = view {
        vec![HostOp::Chat(ChatOp::Arrive {
            win,
            view,
            card_first: req.card_first,
            tab: req.tab,
            intent: req.intent,
        })]
    } else {
        let mut tail = focus::arrive_tail(win, &to, req.card_first);
        for op in &mut tail {
            if let HostOp::Chat(ChatOp::Arrive { intent, .. }) = op {
                *intent = req.intent;
            }
        }
        tail
    };
    focus::focus_then(w, win, &to, tail, &mut batch).map_err(|_| RevealRefusal::Gone)?;
    if let Some(tab) = req.tab {
        batch.push_layout_before_keys(HostOp::Chat(ChatOp::SelectTab { win, tab }));
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Intent;
    use crate::layout::{Axis, Frame, Layout, ModuleId, Node, Size};
    use crate::tabs::{TabId, ViewId};
    use crate::wm::fake::{Logged, ScriptedDecide};
    use crate::wm::verbs::rig::{self, WIN};
    use crate::wm::*;
    use std::rc::Rc;

    fn request(tab: Option<TabId>) -> RevealRequest {
        RevealRequest {
            tab,
            card_first: true,
            intent: Intent::Show,
        }
    }

    fn two(hidden: bool) -> rig::Rig {
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        if hidden {
            crate::layout::hide(
                &mut layout,
                &ModuleId::agent(),
                &Frame::new(Size { w: 1200, h: 800 }, 1),
            )
            .unwrap();
        }
        let rig = rig::rig(layout);
        rig.host.set_view_of_tab(TabId(7), Some(ViewId::BASE));
        rig
    }

    #[test]
    fn reveal_shows_a_hidden_chat_and_selects_the_tab() {
        let mut r = two(true);
        let (answer, batch) = r.controller.tab_reveal(WIN, &request(Some(TabId(7))));
        assert_eq!(answer, Ok(()));
        let ops = rig::ops(&batch);
        let mapped = ops
            .iter()
            .position(|op| matches!(op, HostOp::Map { module, .. } if *module == ModuleId::agent()))
            .unwrap();
        let selected = ops
            .iter()
            .position(|op| matches!(op, HostOp::Chat(ChatOp::SelectTab { tab: TabId(7), .. })))
            .unwrap();
        let grabbed = ops
            .iter()
            .position(
                |op| matches!(op, HostOp::Focus { to: FocusTo::Module(module), .. } if *module == ModuleId::agent()),
            )
            .unwrap();
        assert!(mapped < selected && selected < grabbed);
        assert!(r.layout.borrow().is_visible(&ModuleId::agent()));
        assert!(
            matches!(&ops[grabbed], HostOp::Focus { on_grab, .. } if on_grab == &vec![HostOp::Chat(ChatOp::Arrive {
                win: WIN, view: ViewId::BASE, card_first: true, tab: Some(TabId(7)), intent: Intent::Show,
            })])
        );
    }

    #[test]
    fn a_gone_tab_is_refused() {
        let mut r = two(true);
        let before = r.layout.borrow().clone();
        let (answer, batch) = r.controller.tab_reveal(WIN, &request(Some(TabId(99))));
        assert_eq!(answer, Err(RevealRefusal::Gone));
        assert!(batch.is_empty());
        assert_eq!(*r.layout.borrow(), before);
    }

    #[test]
    fn a_moving_tab_is_refused() {
        let mut r = two(true);
        r.host.set_tab_moving(TabId(7), true);
        let before = r.layout.borrow().clone();
        let (answer, batch) = r.controller.tab_reveal(WIN, &request(Some(TabId(7))));
        assert_eq!(answer, Err(RevealRefusal::Moving));
        assert!(batch.is_empty());
        assert_eq!(*r.layout.borrow(), before);
    }

    #[test]
    fn reveal_answers_nothing() {
        let mut r = two(true);
        let (answer, batch) = r.controller.tab_reveal(WIN, &request(Some(TabId(7))));
        assert_eq!(answer, Ok(()));
        r.host.set_model(false);
        let mut decide = ScriptedDecide::default();
        decide.then_run(batch);
        let driver = Driver::new(decide, r.host.clone(), r.scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        r.host.attach(Rc::downgrade(&sink));
        driver.run(WIN, crate::keymap::Action::Module(ModuleId::agent()), Origin::Chip);
        let chat_ops: Vec<_> = r
            .host
            .log()
            .into_iter()
            .filter_map(|log| match log {
                Logged::Op(HostOp::Chat(op), _) => Some(op),
                _ => None,
            })
            .collect();
        assert_eq!(
            chat_ops,
            vec![
                ChatOp::SelectTab {
                    win: WIN,
                    tab: TabId(7)
                },
                ChatOp::Arrive {
                    win: WIN,
                    view: ViewId::BASE,
                    card_first: true,
                    tab: Some(TabId(7)),
                    intent: Intent::Show
                },
            ]
        );
    }

    #[test]
    fn reveal_without_a_tab_returns_to_the_last_module() {
        let mut r = two(false);
        for owner in [FocusOwner::Module(ModuleId::agent()), FocusOwner::TopBar] {
            r.controller.handle(HostEvent::NativeFocus {
                win: WIN,
                owner: Some(owner),
                chain: FocusChain(vec![]),
                active: true,
                op_index: None,
            });
        }
        let (answer, batch) = r.controller.tab_reveal(WIN, &request(None));
        assert_eq!(answer, Ok(()));
        assert!(
            rig::ops(&batch).iter().any(|op| matches!(op, HostOp::Focus {
            to: FocusTo::Module(module), on_grab, ..
        } if *module == ModuleId::agent() && matches!(&on_grab[..], [HostOp::Chat(ChatOp::Arrive { tab: None, .. })])))
        );
    }

    #[test]
    fn reveal_carries_stop_to_the_tabs_own_view() {
        let view = ViewId::new(2).unwrap();
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(view.module()),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        crate::layout::hide(&mut layout, &view.module(), &Frame::new(Size { w: 1200, h: 800 }, 1)).unwrap();
        let mut r = rig::rig(layout);
        r.host.set_view_of_tab(TabId(7), Some(view));
        let (answer, batch) = r.controller.tab_reveal(
            WIN,
            &RevealRequest {
                tab: Some(TabId(7)),
                card_first: false,
                intent: Intent::Stop,
            },
        );
        assert_eq!(answer, Ok(()));
        assert!(rig::ops(&batch).iter().any(|op| matches!(op, HostOp::Focus {
            to: FocusTo::Module(module), on_grab, ..
        } if *module == view.module() && on_grab == &vec![HostOp::Chat(ChatOp::Arrive {
            win: WIN, view, card_first: false, tab: Some(TabId(7)), intent: Intent::Stop,
        })])));
        assert!(r.layout.borrow().is_visible(&view.module()));
    }
}
