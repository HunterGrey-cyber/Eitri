//! A chat tab handed from one chat window to another through window 1, the shell's half. Module windows never link to
//! each other, and a session is held by one process at a time through its lease, so no live agent crosses: the source
//! retires the tab, window 1 passes the saved tab on, and the target opens it by resuming its session.
//!
//! **In window 1**, [`HandOvers`] follows each hand-over and says what goes to whom:
//!
//! 1. the target is started or raised, and once it has linked the source is sent `tab_release`;
//! 2. the source answers `tab_kept`, which ends it there, or `tab_released` once the session's lease has gone;
//! 3. the target is sent `tabs_given{adopt}` and answers `tab_adopted` once the tab has settled, or `tab_kept`;
//! 4. the source hears `tab_adopted`, and only then may an emptied window close, or it could not take the tab back.
//!
//! The session's lease is what keeps two windows from driving it, so the target is sent nothing to resume before the
//! source said `tab_released`. A target that ends, or cannot take the tab up, gives it back to the source the same way.
//! One hand-over at a time per source: a second would race the first for the same window's turn and tabs.
//!
//! **In a chat window**, [`serve`] takes what window 1 asks to a [`TabHolder`], which is the panel in a real window.

use std::cell::RefCell;
use std::io;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, TryRecvError};

use eitri_core::agent_backend::AgentId;
use eitri_core::saved_tabs::{SavedTab, SavedTabs};
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::Msg;
use eitri_core::wm::{ChatEvent, Controller, Driver, HostEvent, WinId};
use serde_json::Value;

use super::driver::LinkDriver;
use crate::agent_panel::{AgentPanelHandle, TabAnswer};

/// A message for one module window.
pub type Outgoing = (WindowName, Msg);

enum Stage {
    /// The target has not linked yet.
    Target,
    /// The source was asked to let go. `target_gone`: the target ended meanwhile, so a tab released now goes back.
    Release { target_gone: bool },
    /// The target was given the saved tab, kept here until it has settled there.
    Adopt { tab: Value },
}

struct HandOver {
    from: WindowName,
    agent: AgentId,
    session: String,
    to: WindowName,
    stage: Stage,
}

impl HandOver {
    fn release(&self) -> Outgoing {
        let msg = Msg::TabRelease {
            agent: self.agent.clone(),
            session: self.session.clone(),
            to: self.to.text(),
        };
        (self.from.clone(), msg)
    }

    fn kept(&self, why: String) -> Outgoing {
        let session = self.session.clone();
        (self.from.clone(), Msg::TabKept { session, why })
    }
}

fn given(to: WindowName, tab: Value) -> Outgoing {
    let msg = Msg::TabsGiven {
        tabs: Value::Array(vec![tab]),
        adopt: true,
    };
    (to, msg)
}

/// The hand-overs under way in one window 1.
#[derive(Default)]
pub struct HandOvers {
    moving: Vec<HandOver>,
}

impl HandOvers {
    /// Whether a tab of `from` is on its way already.
    pub fn busy(&self, from: &WindowName) -> bool {
        self.moving.iter().any(|h| h.from == *from)
    }

    /// The tab holding `agent`'s `session` begins its way from `from` to `to`; `linked`: its target is up, so the source
    /// is asked at once.
    pub fn begin(
        &mut self,
        from: WindowName,
        (agent, session): (AgentId, String),
        to: WindowName,
        linked: bool,
    ) -> Vec<Outgoing> {
        let mut hand_over = HandOver {
            from,
            agent,
            session,
            to,
            stage: Stage::Target,
        };
        let mut sends = Vec::new();
        if linked {
            hand_over.stage = Stage::Release { target_gone: false };
            sends.push(hand_over.release());
        }
        self.moving.push(hand_over);
        sends
    }

    /// The window `name` has linked: the sources waiting for it are asked to let go.
    pub fn target_linked(&mut self, name: &WindowName) -> Vec<Outgoing> {
        let mut sends = Vec::new();
        for hand_over in &mut self.moving {
            if hand_over.to == *name && matches!(hand_over.stage, Stage::Target) {
                hand_over.stage = Stage::Release { target_gone: false };
                sends.push(hand_over.release());
            }
        }
        sends
    }

    /// The window `name` is gone. One that was to take a tab gives it back, by way of its source: the source is told
    /// when nothing has moved yet, and is sent the tab when it has let go. One that was giving a tab leaves nothing
    /// to do here: its tab went with it, or is on its way to the target.
    pub fn window_ended(&mut self, name: &WindowName) -> Vec<Outgoing> {
        let mut sends = Vec::new();
        let mut moving = std::mem::take(&mut self.moving);
        moving.retain_mut(|hand_over| {
            if hand_over.from == *name {
                return false;
            }
            if hand_over.to != *name {
                return true;
            }
            match &mut hand_over.stage {
                Stage::Target => {
                    sends.push(hand_over.kept(format!("window {} did not come up", name.text())));
                    false
                }
                Stage::Release { target_gone } => {
                    *target_gone = true;
                    true
                }
                Stage::Adopt { tab } => {
                    println!("[window] {} ended before it took the tab; it goes back", name.text());
                    sends.push(given(hand_over.from.clone(), tab.take()));
                    false
                }
            }
        });
        self.moving = moving;
        sends
    }

    fn at(&self, from: Option<&WindowName>, to: Option<&WindowName>, session: &str) -> Option<usize> {
        self.moving.iter().position(|h| {
            h.session == session && from.is_none_or(|from| h.from == *from) && to.is_none_or(|to| h.to == *to)
        })
    }

    /// `sender`, a module window, says how a hand-over goes.
    pub fn heard(&mut self, sender: &WindowName, msg: Msg) -> Vec<Outgoing> {
        match msg {
            Msg::TabReleased { session, tab } => self.released(sender, &session, tab),
            Msg::TabKept { session, why } => self.kept(sender, &session, &why),
            Msg::TabAdopted { session } => match self.at(None, Some(sender), &session) {
                Some(at) => {
                    let from = self.moving.remove(at).from;
                    vec![(from, Msg::TabAdopted { session })]
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// A tab nobody here is waiting for goes back to the window that let it go, so it is in no window for no reason.
    fn released(&mut self, sender: &WindowName, session: &str, tab: Value) -> Vec<Outgoing> {
        let Some(at) = self.at(Some(sender), None, session) else {
            println!(
                "[window] {} released a tab nothing waits for; it goes back",
                sender.text()
            );
            return vec![given(sender.clone(), tab)];
        };
        match self.moving[at].stage {
            Stage::Release { target_gone: false } => {
                let hand_over = &mut self.moving[at];
                let to = hand_over.to.clone();
                hand_over.stage = Stage::Adopt { tab: tab.clone() };
                vec![given(to, tab)]
            }
            Stage::Release { target_gone: true } => {
                let from = self.moving.remove(at).from;
                vec![given(from, tab)]
            }
            Stage::Target | Stage::Adopt { .. } => {
                println!("[window] {} released a tab out of turn; dropped", sender.text());
                Vec::new()
            }
        }
    }

    fn kept(&mut self, sender: &WindowName, session: &str, why: &str) -> Vec<Outgoing> {
        if let Some(at) = self.at(Some(sender), None, session) {
            if matches!(self.moving[at].stage, Stage::Release { .. }) {
                println!("[window] the tab stays in {}: {why}", sender.text());
                self.moving.remove(at);
            }
            return Vec::new();
        }
        let Some(at) = self.at(None, Some(sender), session) else {
            return Vec::new();
        };
        let Stage::Adopt { tab } = &mut self.moving[at].stage else {
            return Vec::new();
        };
        let tab = tab.take();
        println!("[window] {} did not take the tab ({why}); it goes back", sender.text());
        let from = self.moving.remove(at).from;
        vec![given(from, tab)]
    }
}

/// What a chat window does with its tabs for a hand-over.
pub trait TabHolder {
    /// Lets the tab holding `agent`'s `session` go, or keeps it; the answer comes later as a [`TabAnswer`].
    fn release(&self, agent: &AgentId, session: &str);
    /// Takes up `tab`, which another window let go; the answer comes later as a [`TabAnswer`].
    fn adopt(&self, tab: SavedTab);
    /// A tab this window offered stays here, for the reason `why`.
    fn kept(&self, why: String);
    /// A tab this window gave up is open in its new window.
    fn left(&self);
    /// Takes up `tabs`, which window 1 kept for this window, as a launch takes up its saved tabs.
    fn given(&self, tabs: SavedTabs);
    /// This window's tabs as they stand, when it can say.
    fn tabs_now(&self) -> Option<SavedTabs>;
    /// Tabs given to this window that the user has neither opened nor answered.
    fn unopened(&self) -> Option<SavedTabs> {
        None
    }
    /// What window 1 should hold for this window when it goes without the user closing it; `None` leaves that as it is.
    fn tabs_to_keep(&self) -> Option<SavedTabs> {
        self.tabs_now()
    }
}

/// Where a window's [`TabHolder`] is set, once it is built.
pub type TabHolderSlot = Rc<RefCell<Option<Rc<dyn TabHolder>>>>;

/// What a chat window takes from window 1: a hand-over's steps, and the tabs kept for it, given without `adopt`. Tabs
/// given are the other window's input: what is no tab is logged and dropped.
pub fn serve(link: &LinkDriver, holder: TabHolderSlot) {
    fn on_holder(holder: &TabHolderSlot, kind: &str, act: impl FnOnce(&dyn TabHolder)) {
        let held = holder.borrow().clone();
        match held {
            Some(held) => act(&*held),
            None => println!("[link] `{kind}`: this window holds no tabs"),
        }
    }
    let slot = holder.clone();
    link.on("tab_release", move |_, msg, _| {
        if let Msg::TabRelease { agent, session, .. } = msg {
            on_holder(&slot, "tab_release", |held| held.release(&agent, &session));
        }
    });
    let slot = holder.clone();
    link.on("tabs_given", move |_, msg, _| {
        let Msg::TabsGiven { tabs, adopt } = msg else {
            return;
        };
        if !adopt {
            match SavedTabs::from_wire(&tabs) {
                Ok(tabs) => on_holder(&slot, "tabs_given", |held| held.given(tabs)),
                Err(why) => eprintln!("[link] the tabs given here are dropped: {why}"),
            }
            return;
        }
        for tab in tabs.as_array().map(Vec::as_slice).unwrap_or_default() {
            match SavedTab::from_wire(tab) {
                Ok(tab) => on_holder(&slot, "tabs_given", |held| held.adopt(tab)),
                Err(why) => eprintln!("[link] a tab given here is dropped: {why}"),
            }
        }
    });
    let slot = holder.clone();
    link.on("tab_kept", move |_, msg, _| {
        if let Msg::TabKept { why, .. } = msg {
            on_holder(&slot, "tab_kept", |held| held.kept(why));
        }
    });
    link.on("tab_adopted", move |_, _, _| {
        on_holder(&holder, "tab_adopted", |held| held.left());
    });
}

/// The tabs a chat window let go whose `tab_released` is not on the socket yet: the window is their only holder until it
/// is, since window 1 gives nobody a tab it never heard of.
#[derive(Default)]
pub struct UnwrittenTabs(RefCell<Vec<(Receiver<io::Result<()>>, SavedTab)>>);

impl UnwrittenTabs {
    /// `tab`'s release was queued, and `written` answers once it is on the socket or never will be.
    pub fn hold(&self, written: Receiver<io::Result<()>>, tab: SavedTab) {
        self.0.borrow_mut().push((written, tab));
    }

    /// The tabs whose release can no longer reach window 1. One written is window 1's to pass on, and is forgotten; one
    /// still queued stays held. A link that stopped drops its queue, which answers as a sender gone.
    pub fn settle(&self) -> Vec<SavedTab> {
        let mut lost = Vec::new();
        self.0.borrow_mut().retain(|(written, tab)| match written.try_recv() {
            Ok(Ok(())) => false,
            Err(TryRecvError::Empty) => true,
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                lost.push(tab.clone());
                false
            }
        });
        lost
    }
}

/// What window 1 is told for `answer`.
pub fn answer_message(answer: TabAnswer) -> Msg {
    match answer {
        TabAnswer::Released { session, tab } => Msg::TabReleased {
            session,
            tab: tab.to_wire(),
        },
        TabAnswer::Kept { session, why } | TabAnswer::NotAdopted { session, why } => Msg::TabKept { session, why },
        TabAnswer::Adopted { session } => Msg::TabAdopted { session },
    }
}

/// A chat window's tabs: its panel, and its controller for what the user is told.
pub struct PanelTabs {
    panel: AgentPanelHandle,
    driver: Weak<Driver<Controller>>,
}

impl PanelTabs {
    pub fn new(panel: AgentPanelHandle, driver: Weak<Driver<Controller>>) -> PanelTabs {
        PanelTabs { panel, driver }
    }

    fn post(&self, event: ChatEvent) {
        if let Some(driver) = self.driver.upgrade() {
            driver.post(HostEvent::Chat(event));
        }
    }
}

impl TabHolder for PanelTabs {
    fn release(&self, agent: &AgentId, session: &str) {
        self.panel.release_tab(agent, session);
    }

    fn adopt(&self, tab: SavedTab) {
        self.panel.adopt_tab(tab);
    }

    fn kept(&self, why: String) {
        self.post(ChatEvent::TabKept { win: WinId::MAIN, why });
    }

    fn left(&self) {
        self.post(ChatEvent::TabLeft { win: WinId::MAIN });
    }

    fn given(&self, tabs: SavedTabs) {
        self.panel.give_tabs(tabs);
    }

    fn tabs_now(&self) -> Option<SavedTabs> {
        self.panel.tabs_now()
    }

    fn unopened(&self) -> Option<SavedTabs> {
        self.panel.unopened_tabs()
    }

    fn tabs_to_keep(&self) -> Option<SavedTabs> {
        self.panel.tabs_to_keep()
    }
}
