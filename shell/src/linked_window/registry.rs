//! Window 1's module windows: for each chat or page window it started, the link, the process and what that link was
//! told. The group is a star, so this is all a group knows of itself: module windows never link to each other.
//!
//! A window is here from its start until its link ends, is refused or misses its handshake; its name is then free to
//! start again. Peer ids only grow, so nothing that outlives a link, a late timer above all, can name its successor.
//!
//! The hand-overs of tabs between chat windows pass through here too: window 1 is the only window that links to them
//! all ([`super::hand_over`] decides, this sends). So do the tabs each one reports and is given
//! ([`super::group_tabs`]).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use eitri_core::agent_backend::AgentId;
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::Msg;
use eitri_core::wm::PeerId;

use super::driver::LinkDriver;
use super::group_tabs::GroupTabs;
use super::hand_over::{HandOvers, Outgoing};
use super::spawn::SecondProcess;
use super::HeldSaid;

/// What opening a chat or page window says in any window but window 1.
pub const MODULE_WINDOWS_ARE_WINDOW_1S: &str = "only the editor window opens chat and page windows";

/// The first peer id of a module window. Below it are window 1 and window 2.
const FIRST_MODULE_PEER: u32 = 16;

/// What opening a module window came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenModule {
    /// It was not open: it is started, and links once it says hello.
    Spawned(PeerId),
    /// It was open: it is asked to come forward.
    Raised(PeerId),
}

/// One module window, as window 1 holds it.
pub struct ModuleLink {
    pub peer: PeerId,
    pub link: LinkDriver,
    pub process: Rc<SecondProcess>,
    /// What this link was last told this window holds.
    pub said: HeldSaid,
}

/// The module windows of one window 1, by name.
pub struct ModuleWindows {
    windows: BTreeMap<WindowName, ModuleLink>,
    next_peer: u32,
    hand_overs: HandOvers,
    /// The group's saved tabs; `None` where they are not remembered, or where this is no hub.
    tabs: Option<GroupTabs>,
    /// The closing group's chat windows that have not said their last tabs yet.
    last_tabs_from: BTreeSet<WindowName>,
}

impl Default for ModuleWindows {
    fn default() -> ModuleWindows {
        ModuleWindows {
            windows: BTreeMap::new(),
            next_peer: FIRST_MODULE_PEER,
            hand_overs: HandOvers::default(),
            tabs: None,
            last_tabs_from: BTreeSet::new(),
        }
    }
}

impl ModuleWindows {
    /// A peer id no window of this run has had.
    pub fn next_peer(&mut self) -> PeerId {
        let peer = PeerId(self.next_peer);
        self.next_peer += 1;
        peer
    }

    pub fn insert(&mut self, name: WindowName, module: ModuleLink) {
        self.windows.insert(name, module);
    }

    pub fn get(&self, name: &WindowName) -> Option<&ModuleLink> {
        self.windows.get(name)
    }

    pub fn by_peer(&self, peer: PeerId) -> Option<&ModuleLink> {
        self.windows.values().find(|module| module.peer == peer)
    }

    /// Takes the window `peer` out; a hand-over it was part of goes on without it.
    pub fn remove(&mut self, peer: PeerId) -> Option<ModuleLink> {
        let name = self.name_of(peer)?;
        let module = self.windows.remove(&name);
        let sends = self.hand_overs.window_ended(&name);
        self.send(sends);
        module
    }

    /// Says `bye{why}` on every window's link and empties the registry: the group is closing, so nothing is handed over
    /// and no name is started again. A window still in its handshake hears it too. The processes are left for the caller
    /// to see off. Takes the cell because a link's closing hooks run here, with the registry free for them to read.
    pub fn end_group(windows: &RefCell<ModuleWindows>, why: &str) -> Vec<Rc<SecondProcess>> {
        let taken = std::mem::take(&mut windows.borrow_mut().windows);
        taken
            .into_values()
            .map(|module| {
                module.link.close_detached(why);
                module.process
            })
            .collect()
    }

    /// The group starts to close: every linked chat window is asked for its tabs as they stand, while its link can carry
    /// them, and from here on its tabs stay however it goes. Whether anyone was asked.
    pub fn ask_last_tabs(&mut self) -> bool {
        let Some(group) = &mut self.tabs else { return false };
        group.keep_every_window();
        for (name, module) in &self.windows {
            if name.chat_number().is_none() || !module.link.linked() {
                continue;
            }
            match module.link.send(Msg::GroupClosing {}, Vec::new()) {
                Ok(()) => {
                    self.last_tabs_from.insert(name.clone());
                }
                Err(err) => eprintln!("[window] {} was not asked for its last tabs: {err}", name.text()),
            }
        }
        !self.last_tabs_from.is_empty()
    }

    /// The window `name` said its last tabs.
    pub fn last_tabs_said(&mut self, name: &WindowName) {
        self.last_tabs_from.remove(name);
    }

    /// Whether a window asked still has its last tabs to say; one whose link has ended never will.
    pub fn last_tabs_owed(&mut self) -> bool {
        let windows = &self.windows;
        self.last_tabs_from
            .retain(|name| windows.get(name).is_some_and(|module| module.link.linked()));
        !self.last_tabs_from.is_empty()
    }

    /// The window `peer`, while it is here.
    pub fn name_of(&self, peer: PeerId) -> Option<WindowName> {
        let (name, _) = self.windows.iter().find(|(_, module)| module.peer == peer)?;
        Some(name.clone())
    }

    pub fn names(&self) -> Vec<WindowName> {
        self.windows.keys().cloned().collect()
    }

    pub fn links(&self) -> impl Iterator<Item = &ModuleLink> {
        self.windows.values()
    }

    /// From here on the group's tabs are kept here.
    pub fn keep_tabs(&mut self, tabs: GroupTabs) {
        self.tabs = Some(tabs);
    }

    /// The window `name` reported its tabs.
    pub fn tabs_reported(&mut self, name: &WindowName, tabs: &serde_json::Value) {
        if let Some(group) = &mut self.tabs {
            group.reported(name, tabs);
        }
    }

    /// The window `name` closed cleanly.
    pub fn tabs_window_closed(&mut self, name: &WindowName) {
        if let Some(group) = &mut self.tabs {
            group.window_closed(name);
        }
    }

    /// The window `name` has linked: it is given what is kept for it.
    pub fn give_tabs(&self, name: &WindowName) {
        if let Some(msg) = self.tabs.as_ref().and_then(|group| group.given(name)) {
            self.send(vec![(name.clone(), msg)]);
        }
    }

    /// Window 1 is closing; the receiver reports once the tabs file's last write is done.
    pub fn finish_tabs(&mut self) -> Option<std::sync::mpsc::Receiver<()>> {
        self.tabs.as_mut().and_then(GroupTabs::finish)
    }

    /// Whether a tab of `from` is on its way already.
    pub fn hand_over_busy(&self, from: &WindowName) -> bool {
        self.hand_overs.busy(from)
    }

    /// The tab of `tab`'s agent and session leaves `from` for `to`, which is here and linked or still starting.
    pub fn begin_hand_over(&mut self, from: WindowName, tab: (AgentId, String), to: WindowName) {
        let linked = self.windows.get(&to).is_some_and(|module| module.link.linked());
        let sends = self.hand_overs.begin(from, tab, to, linked);
        self.send(sends);
    }

    /// The window `name` has linked: the hand-overs waiting for it go on.
    pub fn hand_over_target_linked(&mut self, name: &WindowName) {
        let sends = self.hand_overs.target_linked(name);
        self.send(sends);
    }

    /// `sender` says how a hand-over goes.
    pub fn hand_over_heard(&mut self, sender: &WindowName, msg: Msg) {
        let sends = self.hand_overs.heard(sender, msg);
        self.send(sends);
    }

    /// Sends each message over its window's link. A window that is gone was told by its link's end already, and with
    /// it goes only the tab's window: the session stays in its record, for the chooser to resume.
    pub fn send(&self, sends: Vec<Outgoing>) {
        for (name, msg) in sends {
            let kind = msg.kind();
            let Some(module) = self.windows.get(&name) else {
                eprintln!("[window] `{kind}` for {}, which is gone; dropped", name.text());
                continue;
            };
            if let Err(err) = module.link.send(msg, Vec::new()) {
                eprintln!("[window] `{kind}` for {} was not sent: {err}", name.text());
            }
        }
    }
}

/// The key action window 1 sent a module window to run, by its name and the options that rebuild it, read as a key's
/// binding is. `None`, logged, for a name or options that make no action here: nothing runs then.
pub fn peer_action(
    name: &str,
    opts: &[(String, serde_json::Value)],
    lua_panels: &[String],
) -> Option<eitri_core::keymap::Action> {
    use eitri_core::keymap::action::{parse, OptValue};
    let opts: Vec<(String, OptValue)> = opts
        .iter()
        .map(|(key, value)| (key.clone(), OptValue::from_wire(value)))
        .collect();
    match parse(name, &opts, lua_panels) {
        Ok(parsed) => Some(parsed.action),
        Err(_) => {
            eprintln!("[link] run_action {name}: not an action here");
            None
        }
    }
}

#[cfg(test)]
mod tests;
