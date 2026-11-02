//! The window controller: the decisions about windows, modules, focus and keys, as a synchronous state machine.
//! Facts come in as [`HostEvent`]s and verb requests; ordered batches of [`HostOp`]s come out, and the driver applies
//! them.
//!
//! Every action from a key, a Lua call or a panel tab verb goes through one runner, [`Decide::run`]: the
//! container selection's gate and the float's first (a tray chip meets neither), then the action's family. Every
//! family is decided here (the tiling verbs, [`super::verbs::tiling`], the focus verbs, [`super::verbs::focus`], the
//! keys verbs, [`super::verbs::keys`], the module verbs, [`super::verbs::modules`], and the rest of
//! [`super::verbs`]) on the window's layout and answered with the batch that shows its change. Nothing else writes a
//! window's layout or its workspaces: the shell reads them through views, and the facts it reports (a link's end, a
//! window 2 that did not come back) are changed here.
//!
//! Who holds the keys is decided here too ([`super::focus`]): the requests that give them, and what follows the
//! host's report of where they are. So are the keys themselves ([`super::keys`]): each capture stage's answer, the
//! window's prefix and what its commands do, and the chrome the prefix shows. It also decides each window's module
//! lifecycle and layout persistence: a change derives the modules' service effects and arms the save's debounce on
//! the scheduler. The timer writes through the window's [`Saver`](super::persist::Saver), and the close's save is
//! the teardown's own ([`super::close::save_for_close`]), the first step of the batch that ends the window. Each
//! family that moves adds its state and its decisions here and deletes the shell code that made them.
//!
//! What each window's chrome shows is derived here too ([`super::chrome`]): every entry that returns a batch ends
//! by deriving the fields whose inputs moved, and announces the model once when a value changed.

use super::chrome::{self, ChromeModel, ChromeTitles, Derivations};
use super::host::*;
use super::module::{Lifecycle, SurfaceDecl};
use super::verbs::modules::{self as module_verbs, ModuleSetup};
use super::verbs::{self, FloatGate, SelectionGate};
use super::window::{
    EditorPresence, Monitor, PeerId, QueuedOpen, Timers, WinId, Window, WindowParts, WindowPlacement, WindowSpec,
};
use crate::keymap::action::Action;
use crate::keymap::prefix::{Prefix, PrefixCommand, Press};
use crate::keymap::Keymap;
use crate::layout::{Arrangement, Con, ModuleId, ModuleKeys, Size};
use crate::lua::layout::LayoutRequest;
use crate::tabs::ViewId;
use crate::window_link::names::WindowName;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

/// The window controller; see the module documentation.
pub struct Controller {
    scheduler: Rc<dyn Scheduler>,
    /// What the shell answers at once: the grid's size and measures, the editor's cell.
    reads: Rc<dyn HostReads>,
    windows: BTreeMap<WinId, Window>,
    /// Each window's keymap and module keys, from its config phase on; the refusals name keys from them.
    keymaps: BTreeMap<WinId, (Rc<Keymap>, Rc<ModuleKeys>)>,
    /// Each window's `keymap.which_key_delay`, from its config phase on; a window made after it is given gets it.
    hint_delays: BTreeMap<WinId, Duration>,
    /// Each window's open rules, module declarations, start-hidden plugins and project root, which its module
    /// verbs place and kill by.
    setups: BTreeMap<WinId, Rc<ModuleSetup>>,
    /// Where every window's timers come from; the driver's own token is the largest there is.
    timers: Timers,
    /// Specs are retained here only until the live layout is attached.
    declared: BTreeMap<WinId, WindowSpec>,
    pub(super) starts: BTreeMap<WinId, super::start::StartState>,
    /// The latest output list, also given to windows attached later.
    monitors: Vec<Monitor>,
    /// Each window's placement given before its layout phase attached it, kept until then.
    placements: BTreeMap<WinId, WindowPlacement>,
    /// The idles that judge an extra chat view the panel said holds no tab, each with its window and view.
    view_empty: Vec<(TimerToken, WinId, ViewId)>,
    #[cfg(test)]
    start_notes: Vec<String>,
}

impl Controller {
    pub fn new(scheduler: Rc<dyn Scheduler>) -> Controller {
        Controller {
            timers: Timers {
                scheduler: scheduler.clone(),
                next: Rc::new(Cell::new(1)),
            },
            scheduler,
            reads: Rc::new(NoReads),
            windows: BTreeMap::new(),
            keymaps: BTreeMap::new(),
            hint_delays: BTreeMap::new(),
            setups: BTreeMap::new(),
            declared: BTreeMap::new(),
            starts: BTreeMap::new(),
            monitors: Vec::new(),
            placements: BTreeMap::new(),
            view_empty: Vec::new(),
            #[cfg(test)]
            start_notes: vec![],
        }
    }

    /// Seeds the text-size model from the shell's current font and editor scale.
    pub fn set_text_size(&mut self, win: WinId, base: f32, reported_s: f32) {
        if let Some(window) = self.windows.get_mut(&win) {
            window.text_size = Some(super::text_size::TextSize::new(base, reported_s));
        }
    }

    /// The zoom for a page that has had a view this session, when this window has a text-size
    /// model. A killed page's module still answers: its factor is kept for a view rebuilt in its
    /// place.
    pub fn page_zoom(&self, win: WinId, module: &ModuleId) -> Option<f32> {
        let window = self.windows.get(&win)?;
        if !window.zoom_pages.contains(module) {
            return None;
        }
        Some(window.text_size.as_ref()?.page_scale(module))
    }

    #[cfg(test)]
    pub(crate) fn panel_scale_for_test(&self, win: WinId) -> Option<f32> {
        Some(self.windows.get(&win)?.text_size.as_ref()?.panel_scale())
    }

    /// What the controller reads of the shell from now on; until it is given, [`NoReads`]' answers.
    pub fn set_reads(&mut self, reads: Rc<dyn HostReads>) {
        self.reads = reads;
    }

    /// The keymap and the module keys `win`'s config built, which its refusals name keys from and its prefix
    /// reads. A window already held gets a new prefix over them, waiting for nothing.
    pub fn set_keymap(&mut self, win: WinId, keymap: Rc<Keymap>, module_keys: Rc<ModuleKeys>) {
        if let Some(window) = self.windows.get_mut(&win) {
            window.keys.prefix = Some(Prefix::new(keymap.clone(), module_keys.clone()));
            // The prefix strip lists the keys this keymap binds.
            window.chrome_state.dirty.prefix += 1;
        }
        self.keymaps.insert(win, (keymap, module_keys));
    }

    /// How long `win`'s prefix waits for a key before the hint panel shows (`keymap.which_key_delay`); `None`
    /// never shows it. Kept for a window not made yet, which gets it when it is, and applied at once to one that
    /// is.
    pub fn set_hint_delay(&mut self, win: WinId, delay: Option<Duration>) {
        match delay {
            Some(delay) => self.hint_delays.insert(win, delay),
            None => self.hint_delays.remove(&win),
        };
        if let Some(window) = self.windows.get_mut(&win) {
            super::keys::set_hint_delay(window, delay);
        }
    }

    /// Holds compositor mode's first-start toast until the first-start screen is ruled out or has closed, so it never
    /// shows over the screen; a `window.tiling` line the screen wrote drops it.
    pub fn hold_first_toast(&mut self, win: WinId, text: String) {
        if let Some(window) = self.windows.get_mut(&win) {
            window.first_toast = Some(text);
        }
    }

    /// What `win`'s module verbs place and kill by, from its config on; before it, no rules, no declarations and
    /// the current directory as the project root.
    pub fn set_module_setup(&mut self, win: WinId, setup: ModuleSetup) {
        self.setups.insert(win, Rc::new(setup));
    }

    /// Where `win` goes at its start ([`WindowPlacement`]): the output it is put back on, once that output exists
    /// and the window is on another one, and whether it starts window 2 again at its requests phase. Given before the
    /// window is attached it waits for the attach; given later it applies at once.
    pub fn set_placement(&mut self, win: WinId, placement: WindowPlacement) -> Batch {
        let mut batch = Batch::default();
        match self.windows.get_mut(&win) {
            Some(window) => {
                window.set_placement(placement);
                window.put_back_on_its_monitor(win, &mut batch);
            }
            None => {
                self.placements.insert(win, placement);
            }
        }
        batch
    }

    /// At the requests phase: a window whose placement starts window 2 again does so on an idle.
    pub(super) fn arm_second_restore(&mut self, win: WinId) {
        if let Some(window) = self.windows.get_mut(&win).filter(|window| window.restore_second) {
            window.restore_idle = Some(window.arm_idle());
        }
    }

    /// Arms the idle that opens the module windows `win`'s start owes, when it owes any.
    pub(super) fn arm_start_windows(&mut self, win: WinId) {
        let owes = self.starts.get(&win).is_some_and(|start| !start.windows.is_empty());
        if let Some(window) = self.windows.get_mut(&win).filter(|_| owes) {
            window.start_windows_idle = Some(window.arm_idle());
        }
    }

    /// A chooser pick, by the key `keycode` that made it, uses the same workspace decisions as the action runner.
    pub fn workspace_pick(&mut self, win: WinId, choice: crate::keymap::prefix::WorkspacePick, keycode: u32) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("workspace_pick", choice)) {
            return batch;
        }
        if choice == crate::keymap::prefix::WorkspacePick::New {
            return self.run(win, Action::WorkspaceNew, Origin::Key { keycode });
        }
        let (reads, _, setup) = self.module_parts(win);
        if let Some(window) = self.windows.get_mut(&win) {
            verbs::workspaces::pick(window, &*reads, win, choice, &setup, &mut batch);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// `win` is linked to the window `peer`, called `name`, reached through `port`; its moves start from here.
    /// Attaching an id that is attached replaces that peer alone, ending its moves as the link's end does; the
    /// window's other peers stay.
    pub fn attach_peer(&mut self, win: WinId, peer: PeerId, name: WindowName, port: Rc<dyn PeerPort>) -> Batch {
        let mut batch = Batch::default();
        if self.windows.get(&win).is_some_and(|w| w.peer(peer).is_some()) {
            batch = self.detach_peer(win, peer);
        }
        let Some(window) = self.windows.get_mut(&win) else {
            eprintln!("[wm] BUG: a peer attached to {win:?}, which is not held");
            return batch;
        };
        let side = super::moves::side_of(window.role());
        window
            .peers
            .insert(peer, super::moves::PeerMoves::new(peer, name.clone(), port, side));
        super::moves::say_monitor_to(window, peer);
        super::moves::summary_at_attach(window, win, peer, &*self.reads);
        window.say(format!("[window] linked to the other window ({peer:?})"));
        verbs::module_windows::linked(window, win, &name, peer, &mut batch);
        batch
    }

    /// `win`'s link to `peer` has gone: every move on it ends as the link's end ends it (a module not yet sent stays
    /// here, an arrival not yet committed is dropped), and the window verbs refuse until a peer is attached again.
    /// The window's other peers are untouched.
    pub fn detach_peer(&mut self, win: WinId, peer: PeerId) -> Batch {
        let mut batch = Batch::default();
        let (reads, keymap, setup) = self.module_parts(win);
        if let Some(window) = self.windows.get_mut(&win) {
            let cx = module_verbs::Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            super::moves::detach(window, &cx, win, peer, &mut batch);
            super::close::peer_left(window, &cx, win, &mut batch);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// The peers attached to `win`, by id, each with the name its window answers to.
    pub fn peer_names(&self, win: WinId) -> Vec<(PeerId, WindowName)> {
        self.windows
            .get(&win)
            .map(|window| window.peers().map(|(id, peer)| (*id, peer.name().clone())).collect())
            .unwrap_or_default()
    }

    /// The window a peer is attached to.
    fn window_of_peer(&self, peer: PeerId) -> Option<WinId> {
        self.windows
            .iter()
            .find(|(_, window)| window.peer(peer).is_some())
            .map(|(win, _)| *win)
    }

    /// A fact from the other window or the link, decided on the window it is attached to.
    fn peer_event(&mut self, event: PeerEvent, batch: &mut Batch) {
        let Some(win) = self.window_of_peer(event.peer()) else {
            eprintln!("[wm] {event:?}: no window is linked to that peer");
            return;
        };
        if let PeerEvent::HubAsk { .. } | PeerEvent::WindowFocused { .. } = event {
            self.hub_event(win, event, batch);
            return;
        }
        let (reads, keymap, setup) = self.module_parts(win);
        if let Some(window) = self.windows.get_mut(&win) {
            let cx = module_verbs::Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            super::moves::peer_event(window, &cx, win, event, batch);
        }
    }

    /// What a module window says to window 1 in compositor mode: a window of another tiling has no such peers.
    fn hub_event(&mut self, win: WinId, event: PeerEvent, batch: &mut Batch) {
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        if window.tiling != super::window::Tiling::Hub {
            window.warn(format!(
                "[window] {event:?}: this window is not window 1 of a compositor group"
            ));
            return;
        }
        match event {
            PeerEvent::WindowFocused { peer } => verbs::module_windows::window_focused(window, peer),
            PeerEvent::HubAsk { peer, ask, token } => {
                let reads = self.reads.clone();
                let asked = verbs::module_windows::hub_ask(window, &*reads, win, peer, ask, token.clone(), batch);
                let Some(action) = asked else {
                    return;
                };
                // A module of window 1's own: it runs here as its key does, and the window comes forward after.
                let ran = self.run_gated(win, action, Origin::Hub, None);
                batch.append(ran);
                batch.push_effect(HostOp::Present {
                    win,
                    activation: token.map(super::window::ActivationToken),
                });
            }
            _ => {}
        }
    }

    /// What this module window is, for the verbs that differ between `c1` and the others.
    pub fn set_module_name(&mut self, win: WinId, name: WindowName) {
        if let Some(window) = self.windows.get_mut(&win) {
            window.module_name = Some(name);
        }
    }

    /// The host could not start or raise the other window. The refusal names no window: a raise's goes to the
    /// window that asked for one, a start's to every window that can have a second one (one per process).
    fn window_op_refused(&mut self, what: OpTag, why: &str, batch: &mut Batch) {
        for (win, window) in &mut self.windows {
            let asked = match what {
                OpTag::RaisePeer | OpTag::ForwardRaise => super::moves::raise_refused(window),
                _ => !matches!(window.role(), super::window::WindowRole::Companion),
            };
            if asked {
                verbs::windows::refused(window, *win, &what, why, batch);
                if matches!(what, OpTag::RaisePeer | OpTag::ForwardRaise) {
                    return;
                }
            }
        }
    }

    /// What `win`'s module verbs read: the shell's answers, the keymap and the module setup.
    pub(super) fn module_parts(&self, win: WinId) -> (Rc<dyn HostReads>, Rc<Keymap>, Rc<ModuleSetup>) {
        (
            self.reads.clone(),
            self.keymap(win),
            self.setups.get(&win).cloned().unwrap_or_default(),
        )
    }

    /// Whether a card answer from chat view `view` may be taken, read and nothing else: `Ok` only when the
    /// controller can tell nothing covers the view. A layout being written right now, a window it does not hold, or
    /// a view the window's layout does not hold answer `Err` with the refusal's text, which the caller posts as
    /// [`HostEvent::AnswerRefused`] for the toast.
    pub fn answer_gate(&self, win: WinId, view: ViewId) -> Result<(), String> {
        let (reads, keymap, setup) = self.module_parts(win);
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        module_verbs::answer_gate(self.windows.get(&win), &cx, win, view)
    }

    /// The attention policy's choice for a view handed a card, from this window's current layout.
    pub fn attention(
        &self,
        win: WinId,
        view: ViewId,
        policy: crate::attention::ChatOnPermission,
        mine: crate::attention::Attention,
    ) -> verbs::chat::AttentionChoice {
        let Some(w) = self.windows.get(&win) else {
            return verbs::chat::AttentionChoice::Toast;
        };
        let (reads, keymap, setup) = self.module_parts(win);
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        verbs::chat::attention(w, &cx, win, view, policy, mine)
    }

    /// The choice after the host handled the reveal: coverage and the tab with the keys may still keep it away.
    pub fn attention_revealed(&self, event: &ChatEvent) -> verbs::chat::AttentionChoice {
        let win = match event {
            ChatEvent::Covered { win, .. } | ChatEvent::Reveal { win, .. } => *win,
            _ => return verbs::chat::AttentionChoice::None,
        };
        let Some(w) = self.windows.get(&win) else {
            return verbs::chat::AttentionChoice::Toast;
        };
        let (reads, keymap, setup) = self.module_parts(win);
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        verbs::chat::attention_revealed(w, &cx, event)
    }

    /// Whether a chat's request to `win`'s editor is refused, asked while the page waits for the answer: the
    /// refusal's words, or `None` when it may go through [`Self::editor_request`]. A window it does not
    /// hold refuses, since nothing there can be judged.
    pub fn editor_request_refusal(&self, win: WinId) -> Option<String> {
        let (reads, keymap, setup) = self.module_parts(win);
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        match self.windows.get(&win) {
            Some(window) => verbs::chat::editor_request_refusal(window, &cx, win),
            None => Some(super::feedback::LAYOUT_CHANGING_TEXT.into()),
        }
    }

    /// Checks and carries out a panel's editor request, returning any refusal to the requesting panel.
    pub fn editor_request(&mut self, win: WinId, keys: String) -> (Result<(), String>, Batch) {
        let mut batch = Batch::default();
        let mut answer = Err(super::feedback::LAYOUT_CHANGING_TEXT.to_string());
        self.modules_event(win, &mut batch, |w, cx, batch| {
            answer = verbs::chat::editor_request(w, cx, win, keys, batch);
        });
        self.settle_chrome(&mut batch);
        (answer, batch)
    }

    /// A plugin's follow command for `module`, through the runner's gates as the module key (a show) or
    /// `module.hide` (a hide) meets them, then shown with the keys where they are or hidden as any hide is.
    pub fn follow(&mut self, win: WinId, module: &ModuleId, show: bool) -> Batch {
        let action = if show {
            Action::Module(module.clone())
        } else {
            Action::ModuleHide
        };
        let mut batch = self.run_gated(win, action, Origin::Follow, Some(module));
        self.settle_chrome(&mut batch);
        batch
    }

    /// A plugin page's `layout.focus` on `module`: shown where it was hidden from (a start-hidden plugin's module
    /// this workspace does not hold yet opened by its rule), then given the keys and arrived at, as the module key
    /// lands. The answer the page is given: the refusal's text, or `Ok`.
    pub fn plugin_focus(&mut self, win: WinId, module: &ModuleId) -> (Result<(), String>, Batch) {
        let mut batch = Batch::default();
        let mut answer = Err(super::feedback::LAYOUT_CHANGING_TEXT.to_string());
        self.modules_event(win, &mut batch, |w, cx, batch| {
            answer = module_verbs::show_on_screen(w, cx, win, module, batch)
                .and_then(|()| super::focus::focus_and_arrive(w, win, module, batch))
                .map_err(|err| err.to_string());
        });
        self.settle_chrome(&mut batch);
        (answer, batch)
    }

    /// A plugin page's `terminal.at` on `win`'s base terminal, `dir` checked by the plugin host and `ticket` naming the
    /// answer ([`super::terminal_at`]): the directory kept for a terminal with no shell running, or the running
    /// shell read first, the answer coming back as [`PluginOp::TerminalAtAnswer`]. `Err` is a refusal now.
    pub fn plugin_terminal_at(
        &mut self,
        win: WinId,
        plugin: &str,
        dir: std::path::PathBuf,
        ticket: u64,
    ) -> (Result<(), String>, Batch) {
        let mut batch = Batch::default();
        let answer = match self.windows.get_mut(&win) {
            Some(window) => super::terminal_at::request(window, win, plugin, dir, ticket, &mut batch),
            None => Err("the window is gone".to_string()),
        };
        self.settle_chrome(&mut batch);
        (answer, batch)
    }

    /// Read-only handles on `win`'s layout and workspaces, for what reads them outside a decision: the Lua kernel's
    /// `eitri.layout.get` and `export`, which copy what they read. `None` for a window it does not hold.
    pub fn views(&self, win: WinId) -> Option<(super::window::LayoutView, super::window::WorkspacesView)> {
        self.windows.get(&win).map(Window::views)
    }

    /// The pair's end as `win` sees it ([`PairEvent`]): what it held away for the other window comes back in every
    /// workspace, or goes when it ended with that window, laid out as any change of the controller's; then the pair
    /// file is told what the end left. A returned plugin's holding reopens at once and reloads a kept view; a
    /// missing view waits for its show. Other pages reload through their lifecycle or an explicit reload.
    fn pair_event(&mut self, event: PairEvent, batch: &mut Batch) {
        let win = match &event {
            PairEvent::LinkEnded { win, .. } | PairEvent::RestoreFailed { win } => *win,
        };
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        let (layout, workspaces) = (window.layout.clone(), window.workspaces.clone());
        let plan = {
            let (Ok(mut layout), Ok(mut workspaces)) = (layout.try_borrow_mut(), workspaces.try_borrow_mut()) else {
                window.warn(format!(
                    "[window] BUG: the layout was borrowed when {event:?} arrived; nothing came back"
                ));
                return;
            };
            let away = crate::window_link::end::away_everywhere(&layout, &workspaces);
            let kind = match event {
                PairEvent::LinkEnded { .. } => crate::window_link::end::EndKind::LinkEnded,
                PairEvent::RestoreFailed { .. } => crate::window_link::end::EndKind::RestoreFailed,
            };
            let plan = crate::window_link::end::end_plan(&away, kind);
            crate::window_link::end::apply_end(&plan, &mut layout, &mut workspaces);
            plan
        };
        match &event {
            PairEvent::LinkEnded { who, .. } => window.say(format!(
                "[window] the other window ({who}) ended: back {:?}, back hidden {:?}, dropped {:?}",
                plan.back, plan.back_hidden, plan.dropped
            )),
            PairEvent::RestoreFailed { .. } => window.say(format!(
                "[window] window 2 is not restored: back {:?}, dropped {:?}",
                plan.back, plan.dropped
            )),
        }
        // Hosted again before the change is laid out, so its layout ops map what came back; the lifecycle's own
        // pass after them decides what each gets (a Lua panel reopened from its producer, a terminal started once it
        // is shown).
        for id in plan.back.iter().chain(&plan.back_hidden) {
            if matches!(window.lifecycle.kind(id), Some(SurfaceKind::PluginPage { .. })) {
                // Hidden pages must answer their follow feed and commands as soon as they are held here again.
                batch.push_effect(HostOp::Plugin(PluginOp::Hold {
                    win,
                    module: id.clone(),
                }));
            }
        }
        for id in &plan.back {
            window.lifecycle.returned(id);
        }
        for id in &plan.back_hidden {
            window.lifecycle.returned(id);
            window.lifecycle.reload_at_show(id);
        }
        // Killed pages are reopened by the lifecycle when shown; a second load here would replace that fresh
        // document. Pages taken back hidden defer their ReloadPage until shown; a deferred page with no view
        // stays unbuilt while hidden, because its first show builds it.
        let pages: Vec<_> = crate::window_link::end::pages_back(&plan)
            .into_iter()
            .filter(|id| !matches!(window.lifecycle.state(id), Some(super::module::ModuleState::Killed(_))))
            .filter(|id| {
                !(window.lifecycle.deferred_holding(id)
                    && !self.reads.page_built(win, id)
                    && layout.try_borrow().is_ok_and(|layout| !layout.is_shown(id)))
            })
            .collect();
        // The end changed the stored workspaces too, in whatever way the chrome shows them.
        window.chrome_state.dirty.workspaces += 1;
        window.changed_by_controller(win, batch);
        match event {
            // Orphaned, window 2 keeps the pair file from now on, naming what it holds once the end has been carried
            // out.
            PairEvent::LinkEnded { .. }
                if *window.role()
                    == super::window::WindowRole::Linked {
                        side: super::window::LinkSide::Second,
                    } =>
            {
                let held = match (layout.try_borrow(), workspaces.try_borrow()) {
                    (Ok(layout), Ok(workspaces)) => crate::window_link::end::held(&layout, &workspaces),
                    _ => Default::default(),
                };
                batch.push_effect(HostOp::Pair(PairOp::Orphaned { win, held }));
            }
            PairEvent::LinkEnded { .. } => {}
            PairEvent::RestoreFailed { .. } => batch.push_effect(HostOp::Pair(PairOp::Unrestored { win })),
        }
        for module in pages {
            batch.push_effect(HostOp::Pair(PairOp::ReloadPage { win, module }));
        }
    }

    /// Whether `module` is shown in `win`'s layout, and whether it is on screen too (not behind a zoom, not on a tab
    /// its container does not draw): what a plugin's follow commands read before they show or hide it. Neither for
    /// a window it does not hold, or a layout being written right now.
    pub fn module_shown(&self, win: WinId, module: &ModuleId) -> (bool, bool) {
        self.windows
            .get(&win)
            .and_then(|window| window.layout.try_borrow().ok())
            .map_or((false, false), |layout| {
                (layout.is_shown(module), layout.is_visible(module))
            })
    }

    /// Kills `module` with no y/n, as a `y` to its kill's question would: a terminal whose shell ended by itself.
    /// The last module on screen enters the same close flow as a native close request.
    pub fn kill_confirmed(&mut self, win: WinId, module: &ModuleId) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("kill_confirmed", module)) {
            return batch;
        }
        self.modules_event(win, &mut batch, |window, cx, batch| {
            module_verbs::kill_confirmed(window, cx, win, module, batch);
        });
        self.settle_chrome(&mut batch);
        batch
    }

    /// Records a window the shell opened, with the layouts it shares and its saver. Opening it again replaces
    /// its record, cancels the old record's save timer, and starts its count anew.
    pub fn open_window(&mut self, win: WinId, spec: WindowSpec, parts: WindowParts) {
        self.declare_window(win, spec);
        self.attach_parts(win, parts);
    }

    /// Records native presence before a window's layout exists.
    pub fn declare_window(&mut self, win: WinId, spec: WindowSpec) {
        if let Some(token) = self.windows.remove(&win).and_then(|old| old.save_timer) {
            self.scheduler.cancel(token);
        }
        self.declared.insert(win, spec);
    }

    /// Attaches the shared model and saver to a previously declared window.
    pub fn attach_parts(&mut self, win: WinId, mut parts: WindowParts) {
        let Some(spec) = self.declared.remove(&win) else {
            self.start_note("[wm] BUG: attach_parts for a window without a declaration".into());
            return;
        };
        let tiling = self.starts.get(&win).map(|start| start.tiling).unwrap_or_default();
        // A hub's shape is fixed and `eitri` mode's files are not its to touch, whatever saver it was handed.
        if tiling == super::window::Tiling::Hub {
            parts.saver = None;
        }
        if let Some(saver) = parts.saver.as_mut() {
            saver.set_waker(self.scheduler.waker());
        }
        let mut window = Window::new(spec, parts, self.timers.clone());
        window.own_window = self.starts.get(&win).and_then(|start| start.own_window.clone());
        window.tiling = tiling;
        window.monitors = self.monitors.clone();
        if let Some(placement) = self.placements.remove(&win) {
            window.set_placement(placement);
        }
        if let Some(spawn) = self.starts.get(&win).and_then(|start| start.spawn.clone()) {
            window.keep_spawn(spawn);
        }
        if let Some((keymap, module_keys)) = self.keymaps.get(&win) {
            window.keys.prefix = Some(Prefix::new(keymap.clone(), module_keys.clone()));
        }
        window.keys.hint_delay = self.hint_delays.get(&win).copied();
        self.windows.insert(win, window);
    }

    pub(super) fn keep_editor_spawn(&mut self, win: WinId, spawn: EditorSpawn) {
        if let Some(window) = self.windows.get_mut(&win) {
            window.keep_spawn(spawn);
        }
    }

    /// A launch with targets needs nvim to start now: the editor widget is what starts it, and only a shown
    /// editor renders. Shows the editor (placing it beside the keys when this workspace lacks it), ends a zoom
    /// over it and gives it the keys, on the layout itself: no ops go out and the saver measures from the layout
    /// as shown, so no save is armed and the close does not write it: the saved arrangement is left as it was.
    /// A refusal is a note and the keys stay where they are.
    pub(super) fn show_editor_for_targets(&mut self, win: WinId) {
        let Some(w) = self.windows.get_mut(&win) else { return };
        if w.pending_open.is_empty() || *w.presence() != EditorPresence::Owned {
            return;
        }
        let editor = ModuleId::editor();
        let layout = w.layout.clone();
        let Ok(mut l) = layout.try_borrow_mut() else {
            self.start_note("[wm] BUG: the layout was borrowed when the launch's editor was shown".into());
            return;
        };
        let shown = if l.contains(&editor) {
            l.show(&editor).map(|_| ())
        } else {
            let keys = l.focus().clone();
            crate::layout::ops::place(&mut l, &editor, &keys, crate::layout::Axis::Row)
        };
        let result = shown.and_then(|()| l.can_focus(&editor)).and_then(|()| {
            if l.zoom_hides(&editor) {
                l.unzoom();
            }
            l.set_focus(&editor)
        });
        // The show is part of the launch, not a change of the arrangement: the close must not write it.
        if let (Some(saver), Ok(workspaces)) = (w.saver.as_mut(), w.workspaces.try_borrow()) {
            saver.rebase(&l, &workspaces);
        }
        drop(l);
        if let Err(err) = result {
            self.start_note(format!("[wm] the launch's editor was not shown: {err:?}"));
        }
    }

    pub(super) fn start_note(&mut self, line: String) {
        eprintln!("{line}");
        #[cfg(test)]
        self.start_notes.push(line);
    }

    /// What `win` still has to open, whether its editor is ready, and whether a save timer is armed, for tests.
    #[cfg(test)]
    pub(super) fn open_state(&self, win: WinId) -> Option<(Vec<String>, bool, bool)> {
        self.windows
            .get(&win)
            .map(|w| (w.pending_open.clone(), w.editor_ready, w.save_timer.is_some()))
    }

    /// The `open` of the spawn a restart of `win`'s editor starts from, for tests.
    #[cfg(test)]
    pub(super) fn editor_spawn_open(&self, win: WinId) -> Option<Vec<String>> {
        self.windows
            .get(&win)?
            .editor_spawn
            .as_ref()
            .map(|spawn| spawn.open.clone())
    }

    #[cfg(test)]
    pub(super) fn start_notes(&self) -> &[String] {
        &self.start_notes
    }

    fn awaiting_layout(&mut self, win: WinId, event: impl std::fmt::Debug) -> bool {
        if !self.declared.contains_key(&win) {
            return false;
        }
        self.start_note(format!("[wm] BUG: {event:?} for a window with no layout yet"));
        true
    }

    /// How the shell described `win`; `None` for a window never opened.
    pub fn window_spec(&self, win: WinId) -> Option<&WindowSpec> {
        self.windows
            .get(&win)
            .map(Window::spec)
            .or_else(|| self.declared.get(&win))
    }

    /// The lifecycle facts recorded for a window, without borrowing its layout.
    pub fn lifecycle(&self, win: WinId) -> Option<&Lifecycle> {
        self.windows.get(&win).map(|window| &window.lifecycle)
    }

    /// `win`'s `terminal.at` reads out and its restart in place waiting, for tests.
    #[cfg(test)]
    pub(super) fn terminal_at_state(
        &self,
        win: WinId,
    ) -> (
        Vec<super::terminal_at::PendingAt>,
        Option<super::terminal_at::PendingRestart>,
    ) {
        self.windows
            .get(&win)
            .map(|w| (w.pending_at.clone(), w.pending_restart.clone()))
            .unwrap_or_default()
    }

    /// The lifecycle of a window, for tests that set up what it holds without its effects.
    #[cfg(test)]
    pub(crate) fn lifecycle_mut(&mut self, win: WinId) -> Option<&mut Lifecycle> {
        self.windows.get_mut(&win).map(|window| &mut window.lifecycle)
    }

    /// Declares surfaces in launch order; only an owned editor needs a local surface. The batch is decided as if
    /// every create applies, so each module counts as hosted from here; a create the host refuses comes back
    /// after the batch as `Refused` naming its module, which leaves that module declared and without effects.
    pub fn create_surfaces(&mut self, win: WinId, decls: &[SurfaceDecl]) -> Batch {
        let mut batch = Batch::default();
        let Some(window) = self.windows.get_mut(&win) else {
            return batch;
        };
        for decl in decls {
            if matches!(decl.kind, SurfaceKind::Editor { .. }) && *window.presence() != EditorPresence::Owned {
                continue;
            }
            window.lifecycle.declare(decl, true);
            batch.push_layout(HostOp::CreateSurface {
                win,
                module: decl.module.clone(),
                kind: decl.kind.clone(),
            });
        }
        batch
    }

    /// The window's first layout batch, with the hosts the shell placed declared: every visible hosted module
    /// mapped, the float band restacked, the grid allocated, then the modules' lifecycle effects and the chrome
    /// derived from it all. Never the saver: a launch is not a change.
    pub fn first_apply(&mut self, win: WinId, hosted: &[SurfaceDecl]) -> Batch {
        let mut batch = Batch::default();
        let Some(window) = self.windows.get_mut(&win) else {
            return batch;
        };
        for decl in hosted {
            window.lifecycle.declare(decl, true);
        }
        window.layout_ops(win, None, &mut batch);
        let layout = window.layout.clone();
        match layout.try_borrow() {
            Ok(layout) => window.lifecycle.after_change(win, &layout, &mut batch),
            Err(_) => window.warn("[wm] BUG: the layout was borrowed at the first apply; no lifecycle effect".into()),
        }
        window.layout_touched();
        self.settle_chrome(&mut batch);
        batch
    }

    /// One queued `eitri.layout.*` request whose family has moved here: the container selection's gate by the
    /// action the request stands for (its refusal is logged, with no toast), then the request's own operation
    /// with its own ids, its error logged, then the gate's selection reset. `run` meets both gates through the
    /// runner, as a key does.
    pub fn lua_request(&mut self, win: WinId, request: LayoutRequest) -> Batch {
        self.layout_request(win, request, "lua")
    }

    /// One request from `:Eitri` in the window's nvim: what [`lua_request`](Self::lua_request) decides for the same
    /// request, with its log lines tagged `[nvim]` and each refusal toasted too: whoever typed it is looking at the
    /// window, not at a log. Nothing runs while the window still awaits its layout.
    pub fn nvim_request(&mut self, win: WinId, request: LayoutRequest) -> Batch {
        self.layout_request(win, request, "nvim")
    }

    /// An `:Eitri` line the parser refused: the log line and the toast a refused request has.
    pub fn nvim_refused(&mut self, win: WinId, why: &str) -> Batch {
        let mut batch = Batch::default();
        if let Some(window) = self.windows.get_mut(&win) {
            window.warn(format!("[nvim] {why}"));
            chrome::refused(window, why);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// The one body of both: `source` tags the log lines.
    fn layout_request(&mut self, win: WinId, request: LayoutRequest, source: &str) -> Batch {
        let mut batch = Batch::default();
        let toasts = source == "nvim";
        if !super::lua::decided(&request) {
            self.verb_note(win, format!("[wm] BUG: {source}_request for the shell's {request:?}"));
            return batch;
        }
        if self.awaiting_layout(win, (format!("{source}_request"), &request)) {
            return batch;
        }
        let (reads, keymap, setup) = self.module_parts(win);
        let Some(window) = self.windows.get_mut(&win) else {
            return batch;
        };
        window.say(format!("[{source}] layout: {request:?}"));
        if let LayoutRequest::Run(action) = request {
            return self.run(win, action, Origin::Lua);
        }
        if window.tiling == super::window::Tiling::Hub
            && super::lua::hub_request(window, &*reads, win, &request, &mut batch)
        {
            self.settle_chrome(&mut batch);
            return batch;
        }
        let Ok(selection) = window.layout.try_borrow().map(|layout| layout.selection()) else {
            window.warn(format!("[wm] BUG: the layout was borrowed when {request:?} arrived"));
            return batch;
        };
        let gate = super::lua::lua_request_action(&request).map(|action| verbs::selection_gate(&action, selection));
        if gate == Some(SelectionGate::Refuse) {
            let text = verbs::one_module_text(&keymap);
            window.warn(format!("[{source}] layout: {request:?} refused: {text}"));
            if toasts {
                chrome::refused(window, &text);
                self.settle_chrome(&mut batch);
            }
            return batch;
        }
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        let refused = |err: &dyn std::fmt::Display| format!("[{source}] layout: {request:?} refused: {err}");
        let result: Result<(), String> = match &request {
            LayoutRequest::Mark(id) => match window.layout.try_borrow_mut().map(|mut layout| layout.set_mark(id)) {
                // A mark changes only the chrome's text: nothing moves, and the state file does not keep it.
                Ok(Ok(())) => {
                    window.layout_touched();
                    Ok(())
                }
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => {
                    window.warn(format!("[wm] BUG: the layout was borrowed when {request:?} ran"));
                    Ok(())
                }
            },
            LayoutRequest::Show(id) => {
                module_verbs::lua_show(window, &cx, win, id, &mut batch).map_err(|err| err.to_string())
            }
            LayoutRequest::Hide(id) => {
                module_verbs::hide(window, &cx, win, id, &mut batch).map_err(|err| err.to_string())
            }
            LayoutRequest::Focus(id)
                if window.has_peer() && module_verbs::peer_key(window, &cx, win, id, &mut batch) =>
            {
                Ok(())
            }
            LayoutRequest::Focus(id) => {
                let shown = if *id == ModuleId::editor() && window.editor_run != super::window::EditorRun::Running {
                    module_verbs::lua_show(window, &cx, win, id, &mut batch)
                } else {
                    Ok(())
                };
                shown
                    .and_then(|()| super::focus::focus_and_arrive(window, win, id, &mut batch))
                    .map_err(|err| err.to_string())
            }
            LayoutRequest::Split(id, axis) => {
                module_verbs::place_and_arrive(window, &cx, win, id, *axis, &mut batch).map_err(|err| err.to_string())
            }
            // Its refusals, now or once its surface is asked for, are logged under the request's own words, and
            // toasted for `:Eitri` (`modules::refusal` reads the tag).
            LayoutRequest::Open(kind, explicit) => {
                module_verbs::lua_open(
                    window,
                    &cx,
                    win,
                    *kind,
                    explicit.clone(),
                    format!("[{source}] layout: {request:?}"),
                    &mut batch,
                );
                Ok(())
            }
            LayoutRequest::Switch(name) => {
                verbs::workspaces::switch_or_make(window, win, name, &setup, &*reads, &mut batch).map(|_| ())
            }
            LayoutRequest::Float(id, rect) => {
                verbs::floats::float_at(window, &cx, win, id, *rect, &mut batch).map_err(|err| err.to_string())
            }
            LayoutRequest::Run(_) => {
                unreachable!("only the requests decided here get here")
            }
        };
        if let Err(err) = result {
            window.warn(refused(&err));
            if toasts {
                chrome::refused(window, &err);
            }
        }
        if gate == Some(SelectionGate::Run { reset: true }) && window.clear_selection() {
            window.layout_touched();
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// Empty refusals must not leave later requests waiting for an event that might never come.
    fn next_lua_request(&mut self, win: WinId) -> Batch {
        loop {
            let Some(window) = self.windows.get_mut(&win) else {
                return Batch::default();
            };
            if matches!(
                window.close.state,
                super::close::CloseState::TearingDown | super::close::CloseState::Closed
            ) {
                window.lua_pending.clear();
                return Batch::default();
            }
            let Some(request) = window.lua_pending.pop_front() else {
                return Batch::default();
            };
            let batch = self.lua_request(win, request);
            if !batch.is_empty() {
                return batch;
            }
        }
    }

    /// The window's allocation at `size`: the pinned splits shown for the first time settled (with the tree as
    /// the settle left it, for the saver to hear later), then the layout arranged in the frame the grid's
    /// measures give, kept as the window's last arrangement. `None` for a window never opened or a layout borrowed
    /// right now.
    pub fn allocate(&mut self, win: WinId, size: Size) -> Option<(Rc<Arrangement>, Option<Con>)> {
        let reads = self.reads.clone();
        let window = self.windows.get_mut(&win)?;
        let layout = window.layout.clone();
        let mut settled = None;
        // A pin keeps the length its ratio gives at a real allocation, never at a stand-in size; asked first, with
        // no geometry, so a drag or a resize sweep arranges once per allocation.
        if size.w > 0 && size.h > 0 && layout.try_borrow().is_ok_and(|layout| layout.has_pin_to_settle()) {
            if let Ok(mut layout) = layout.try_borrow_mut() {
                if frame_with(&*reads, win, size, |frame| {
                    crate::layout::settle_pins(&mut layout, frame)
                }) {
                    settled = Some(layout.root().clone());
                }
            }
        }
        let Ok(layout) = layout.try_borrow() else {
            window.warn("[wm] BUG: the layout was borrowed when the grid allocated".into());
            return None;
        };
        let arrangement = frame_with(&*reads, win, size, |frame| crate::layout::arrange(&layout, frame));
        drop(layout);
        if settled.is_some() {
            // The settle wrote the pins' lengths: its `Settled` event, heard from the idle, derives what they move.
            window.layout_touched();
            window.unheard_settles.push_back(settled.as_ref().unwrap().clone());
        }
        if window.allocated != Some(size) {
            window.allocated = Some(size);
            chrome::resized(window);
        }
        let arrangement = Rc::new(arrangement);
        // Kept for the drags, which move a divider or a float from where this allocation drew it.
        window.arranged = Some(arrangement.clone());
        Some((arrangement, settled))
    }

    /// The generation of the first-start screen `win` shows, `None` while none is up: what a fact about the screen
    /// that does not come from an op of its own (the editor's report of its mappings) is posted under. A screen the
    /// start opens by itself has generation `0`, so `0` is a generation like any other.
    pub fn onboarding_generation(&self, win: WinId) -> Option<u64> {
        self.windows.get(&win).and_then(super::onboarding::generation)
    }

    /// What `win`'s chrome shows, as a copy; a window never opened shows nothing.
    pub fn chrome(&self, win: WinId) -> ChromeModel {
        self.windows
            .get(&win)
            .map(|window| window.chrome.clone())
            .unwrap_or_default()
    }

    /// What `win`'s chrome names things by: the project, the Lua panels' titles, the tray's modules. The fields
    /// that read them are derived again, and the batch announces what changed.
    pub fn set_titles(&mut self, win: WinId, titles: ChromeTitles) -> Batch {
        let mut batch = Batch::default();
        if let Some(window) = self.windows.get_mut(&win) {
            chrome::set_titles(window, titles);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// How many times each of `win`'s chrome fields was derived; nothing for a window never opened.
    pub fn chrome_derivations(&self, win: WinId) -> Derivations {
        self.windows
            .get(&win)
            .map(|window| window.chrome_state.derivations)
            .unwrap_or_default()
    }

    /// Every window's chrome fields whose inputs moved, derived, and the model announced once per window whose
    /// values changed. Every entry that returns a batch ends here; a window with nothing moved allocates nothing.
    fn settle_chrome(&mut self, batch: &mut Batch) {
        let reads = self.reads.clone();
        for (win, window) in &mut self.windows {
            super::moves::observe_summary(window, *win, &*reads);
            chrome::settle(window, *win, &*reads, self.keymaps.get(win), batch);
        }
    }

    /// The chat facts the chrome shows: a view's attention or busy state, a page that came on screen (judged once
    /// the queue has drained), and a toast a chat asked for.
    fn chat_chrome(&mut self, event: &HostEvent) {
        let HostEvent::Chat(chat) = event else {
            return;
        };
        let win = match chat {
            ChatEvent::Attention { win, .. }
            | ChatEvent::Busy { win, .. }
            | ChatEvent::PageShown { win, .. }
            | ChatEvent::Toast { win, .. } => *win,
            _ => return,
        };
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        match chat {
            ChatEvent::Attention { view, .. } | ChatEvent::Busy { view, .. } => {
                super::moves::counts_changed(window);
                chrome::attention_changed(window, *view)
            }
            ChatEvent::PageShown { view, .. } => chrome::page_shown(window, *view),
            ChatEvent::Toast { text, kind, .. } => chrome::show_toast(window, text.clone(), *kind),
            _ => {}
        }
    }

    /// Whether the release of `keycode` in `win` is to be reported as [`HostEvent::KeyReleased`]: only the key
    /// the window's prefix consumed and has not seen released is. Asked on every release, so it allocates nothing.
    pub fn watches_release(&self, win: WinId, keycode: u32) -> bool {
        self.windows
            .get(&win)
            .is_some_and(|window| super::keys::watches_release(window, keycode))
    }

    /// Whether a Control key's release in `win` is to be reported as [`HostEvent::CtrlReleased`]: only while a text
    /// size run lasts on it. Asked on every release, so it allocates nothing.
    pub fn watches_ctrl(&self, win: WinId) -> bool {
        self.windows.get(&win).is_some_and(super::keys::watches_ctrl)
    }

    /// The workspace chooser was drawn: `win`'s prefix waits for its key, including a top-bar click.
    pub fn await_workspace(&mut self, win: WinId) -> Batch {
        let mut batch = Batch::default();
        if let Some(window) = self.windows.get_mut(&win) {
            super::keys::await_workspace(window, &mut batch);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// A command the window's prefix finished: an action through the runner as a key's, a digit landing in what
    /// `select.number` numbered, a split's placement, the hide key's hide, and a chooser's pick against the workspace
    /// order.
    fn prefix_command(&mut self, win: WinId, command: PrefixCommand, keycode: u32) -> Batch {
        let mut batch = Batch::default();
        match command {
            PrefixCommand::Run(action) => return self.run(win, action, Origin::Key { keycode }),
            PrefixCommand::Number(digit) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::keys::land_digit(window, win, digit, &mut batch);
                }
                batch
            }
            PrefixCommand::Place { module, axis, cwd } => {
                let (reads, keymap, setup) = self.module_parts(win);
                if let Some(window) = self.windows.get_mut(&win) {
                    let cx = module_verbs::Cx {
                        reads: &*reads,
                        keymap: &keymap,
                        setup: &setup,
                    };
                    module_verbs::place(window, &cx, win, &module, axis, cwd, &mut batch);
                }
                batch
            }
            PrefixCommand::Hide { module } => {
                let (reads, keymap, setup) = self.module_parts(win);
                if let Some(window) = self.windows.get_mut(&win) {
                    if verbs::module_windows::hide_key_of_a_window(window, &module) {
                        return batch;
                    }
                    let cx = module_verbs::Cx {
                        reads: &*reads,
                        keymap: &keymap,
                        setup: &setup,
                    };
                    module_verbs::hide_by_key(window, &cx, win, &module, &mut batch);
                }
                batch
            }
            PrefixCommand::Workspace(pick) => self.workspace_pick(win, pick, keycode),
        }
    }

    /// A module fact `f` decides on `win`'s record with the module verbs' reads; nothing for a window never opened.
    fn modules_event(
        &mut self,
        win: WinId,
        batch: &mut Batch,
        f: impl FnOnce(&mut Window, &module_verbs::Cx<'_>, &mut Batch),
    ) {
        let (reads, keymap, setup) = self.module_parts(win);
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        f(window, &cx, batch);
    }

    /// The keys' facts: a watched release, and the window losing activation or being clicked, which end a wait of
    /// the prefix's.
    fn keys_event(&mut self, event: &HostEvent, batch: &mut Batch) {
        match event {
            HostEvent::KeyReleased { win, keycode } => {
                if let Some(window) = self.windows.get_mut(win) {
                    super::keys::released(window, *keycode);
                }
            }
            HostEvent::CtrlReleased { win } => {
                if let Some(window) = self.windows.get_mut(win) {
                    super::keys::ctrl_released(window);
                }
            }
            HostEvent::Active { win, active: false }
            | HostEvent::Pointer {
                win,
                ev: PointerEv::Press { .. },
                ..
            } => {
                if let Some(window) = self.windows.get_mut(win) {
                    super::keys::cancel(window, batch);
                }
            }
            _ => {}
        }
    }

    /// What `win`'s layout needs at the least in the frame the grid's measures give now.
    pub fn min_size(&self, win: WinId) -> Size {
        let Some(window) = self.windows.get(&win) else {
            return Size::default();
        };
        let Ok(layout) = window.layout.try_borrow() else {
            return Size::default();
        };
        let size = self.reads.grid_size(win);
        frame_with(&*self.reads, win, size, |frame| crate::layout::min_size(&layout, frame))
    }

    /// The size of `win`'s last allocation; `None` before its first.
    pub fn allocated(&self, win: WinId) -> Option<Size> {
        self.windows.get(&win).and_then(|window| window.allocated)
    }

    /// The step of the first-start screen `win` shows and whether its press-test timer is armed, for tests.
    #[cfg(test)]
    pub(crate) fn onboarding_step(&self, win: WinId) -> Option<(crate::onboarding::screen::Step, bool)> {
        let state = self.windows.get(&win)?.onboarding.as_ref()?;
        Some((state.screen.step().clone(), state.press_timer.is_some()))
    }

    /// The lines `win`'s verbs printed, for tests.
    #[cfg(test)]
    pub(crate) fn notes(&self, win: WinId) -> Vec<String> {
        self.windows
            .get(&win)
            .map(|window| window.notes().to_vec())
            .unwrap_or_default()
    }

    /// The chat's tab verbs with the keys in `keys`, as a key's tab verb runs them, for tests that name where the
    /// keys are without a host to report them.
    #[cfg(test)]
    pub(crate) fn run_tab(&mut self, win: WinId, tab: crate::keymap::TabAction, keys: Option<ModuleId>) -> Batch {
        let mut batch = Batch::default();
        let setup = self.setups.get(&win).cloned().unwrap_or_default();
        let keymap = self
            .keymaps
            .get(&win)
            .map(|(keymap, _)| keymap.clone())
            .unwrap_or_else(|| Rc::new(Keymap::defaults()));
        let reads = self.reads.clone();
        if let Some(window) = self.windows.get_mut(&win) {
            let cx = module_verbs::Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            verbs::tabs::run(window, &cx, win, tab, keys, &mut batch);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    fn verb_note(&mut self, win: WinId, line: String) {
        match self.windows.get_mut(&win) {
            Some(window) => window.warn(line),
            None => eprintln!("{line}"),
        }
    }

    /// The keymap `win`'s refusals name keys from: its config's, or the defaults before its config phase.
    fn keymap(&self, win: WinId) -> Rc<Keymap> {
        self.keymaps
            .get(&win)
            .map(|(keymap, _)| keymap.clone())
            .unwrap_or_else(|| Rc::new(Keymap::defaults()))
    }

    fn lifecycle_event(&mut self, win: WinId, event: &HostEvent, batch: &mut Batch) {
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        match window.layout.try_borrow() {
            Ok(layout) => window.lifecycle.on_event(win, event, &layout, batch),
            Err(_) => {
                eprintln!("[wm] BUG: the layout was borrowed when {event:?} arrived; no lifecycle effect");
                window.lifecycle.record_event(win, event);
            }
        }
    }

    /// A test's own write of `win`'s layout or workspaces ([`Window::written_in_test`]), with the chrome derived
    /// again at the batch's end.
    #[cfg(test)]
    pub(super) fn changed_in_test(&mut self, win: WinId) -> Batch {
        let mut batch = Batch::default();
        if let Some(window) = self.windows.get_mut(&win) {
            window.written_in_test(win, &mut batch);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// A timer fired: the save's debounce of whichever window armed it, armed again or written, a window's
    /// restack after a float took the keys, the end of a window's digit wait, the hint panel's delay, a toast's
    /// time up, or the idle that derives what a resize changed.
    fn timer_fired(&mut self, token: TimerToken, batch: &mut Batch) {
        let fired = self.windows.iter_mut().find_map(|(win, window)| {
            match super::terminal_at::timer(window, *win, token, batch) {
                super::terminal_at::Fired::No => None,
                fired => Some(fired),
            }
        });
        match fired {
            None => {}
            Some(super::terminal_at::Fired::Replay(Some(held))) => {
                batch.append(self.handle(HostEvent::Terminal(held)));
                return;
            }
            Some(_) => return,
        }
        if let Some((win, window)) = self
            .windows
            .iter_mut()
            .find(|(_, window)| window.restore_idle == Some(token))
        {
            window.restore_idle = None;
            window.say("[window] starting window 2 again, as the pair last ended".to_string());
            batch.push_effect(HostOp::RestoreLinkedWindow { win: *win });
            return;
        }
        if let Some((win, window)) = self
            .windows
            .iter_mut()
            .find(|(_, window)| window.start_windows_idle == Some(token))
        {
            window.start_windows_idle = None;
            if let Some(start) = self.starts.get_mut(win) {
                verbs::module_windows::open_at_start(window, *win, std::mem::take(&mut start.windows), batch);
            }
            return;
        }
        if let Some(at) = self.view_empty.iter().position(|(armed, ..)| *armed == token) {
            let (_, win, view) = self.view_empty.remove(at);
            self.modules_event(win, batch, |window, cx, batch| {
                verbs::chat::view_empty(window, cx, win, view, batch)
            });
            return;
        }
        let wins: Vec<_> = self.windows.keys().copied().collect();
        for win in wins {
            let (reads, keymap, setup) = self.module_parts(win);
            let cx = module_verbs::Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            if super::close::timer(self.windows.get_mut(&win).unwrap(), &cx, win, token, batch) {
                return;
            }
        }
        if let Some(window) = self
            .windows
            .values_mut()
            .find(|window| window.save_timer == Some(token))
        {
            window.save_timer_fired();
            return;
        }
        for (win, window) in &mut self.windows {
            if super::focus::raise_timer_fired(window, *win, token, batch)
                || super::moves::deadline_fired(window, *win, token, batch)
                || super::moves::page_timer(window, *win, token, batch)
                || super::moves::summary_fired(window, token)
                || super::keys::number_timer_fired(window, token, batch)
                || super::keys::hint_timer_fired(window, token)
                || super::onboarding::timer_fired(window, *win, token, batch)
                || chrome::toast_timer_fired(window, token)
                || chrome::resize_idle_fired(window, token)
            {
                return;
            }
        }
    }

    /// The focus family's facts: where the host says the keys are, the window's activity, the `?` sheet
    /// closing, and every route that lands on a module or moves the keys by direction.
    fn focus_event(&mut self, event: &HostEvent, batch: &mut Batch) {
        let reads = self.reads.clone();
        let win = match event {
            HostEvent::NativeFocus { win, .. }
            | HostEvent::Active { win, .. }
            | HostEvent::Chat(ChatEvent::KeymapClosed { win, .. } | ChatEvent::NavFallthrough { win, .. })
            | HostEvent::Editor(EditorEvent::Navigate { win, .. })
            | HostEvent::Pointer { win, .. }
            | HostEvent::Hint(HintEvent::Landed { win, .. })
            | HostEvent::TopBarLeft { win } => *win,
            HostEvent::Refused {
                what: OpTag::Focus,
                surface,
                ..
            } => {
                // A refused grab never reports, so its request must not vouch for a later report of that module;
                // the other grabs of its batch still report, and their requests stand.
                if let Some((win, module)) = surface {
                    if let Some(window) = self.windows.get_mut(win) {
                        super::focus::grab_refused(window, module);
                    }
                }
                return;
            }
            _ => return,
        };
        let Some(w) = self.windows.get_mut(&win) else {
            return;
        };
        let strip_landing = |w: &mut Window, module: &ModuleId, batch: &mut Batch| {
            if let Err(err) = super::focus::focus_and_arrive(w, win, module, batch) {
                w.say(format!("[modules] {module}: its strip entry chose nothing: {err}"));
            }
        };
        match event {
            HostEvent::NativeFocus {
                owner,
                active,
                op_index,
                ..
            } => super::focus::on_native_focus(w, win, owner.clone(), *active, *op_index, batch),
            HostEvent::Active { active, .. } => {
                if !*active {
                    super::grid::drag::release(w);
                }
                super::focus::on_active(w, win, *active, batch);
            }
            HostEvent::Chat(ChatEvent::KeymapClosed {
                view, opening, forced, ..
            }) => super::focus::keymap_closed(w, &*reads, win, *view, *opening, *forced, batch),
            HostEvent::Chat(ChatEvent::NavFallthrough { view, direction, .. }) => {
                // The page could not apply a navigation key against its own state, so it asks for what the key
                // would have done.
                super::focus::move_focus(w, &*reads, win, &view.module(), *direction, batch);
            }
            HostEvent::Editor(EditorEvent::Navigate { direction, .. }) => {
                super::focus::move_focus(w, &*reads, win, &ModuleId::editor(), *direction, batch);
            }
            HostEvent::Pointer {
                on: PointerTarget::StripEntry(module),
                ev: PointerEv::Press { .. },
                ..
            } => strip_landing(w, module, batch),
            // A strip entry chosen by HINT lands as a click would; a module chosen by HINT only gets the keys.
            HostEvent::Hint(HintEvent::Landed {
                target: HintTarget::StripEntry(module),
                ..
            }) => strip_landing(w, module, batch),
            HostEvent::Hint(HintEvent::Landed {
                target: HintTarget::Module(module),
                ..
            }) => {
                super::focus::give_keys(w, win, module, vec![], batch);
            }
            HostEvent::TopBarLeft { .. } => super::focus::return_to_pane(w, win, batch),
            _ => {}
        }
    }

    /// The grid's presses and drags ([`super::grid::drag`]), after the strip entry's landing and the end of the
    /// prefix's wait that every press makes: a press on a float's bar or grip raises it and gives it the keys and
    /// begins its drag, a press on a divider begins that divider's, any other press ends a drag; a motion moves
    /// what is dragged; a release or cancellation ends the drag.
    fn pointer_event(&mut self, event: &HostEvent, batch: &mut Batch) {
        let HostEvent::Pointer { win, on, ev } = event else {
            return;
        };
        let reads = self.reads.clone();
        let Some(window) = self.windows.get_mut(win) else {
            return;
        };
        match ev {
            PointerEv::Press { button, .. } => super::grid::drag::press(window, *win, on, *button, batch),
            PointerEv::Motion { x, y } => super::grid::drag::motion(window, &*reads, *win, *x, *y, batch),
            PointerEv::Release { .. } | PointerEv::Cancel => super::grid::drag::release(window),
        }
    }

    /// The window's record, for tests that drive one of its families directly.
    #[cfg(test)]
    pub(crate) fn window_mut(&mut self, win: WinId) -> Option<&mut Window> {
        self.windows.get_mut(&win)
    }

    /// The window's record, read only, for tests.
    #[cfg(test)]
    pub(crate) fn window(&self, win: WinId) -> Option<&Window> {
        self.windows.get(&win)
    }

    /// What the window's chrome was last told the prefix waits for, and the digits it drew, for tests.
    #[cfg(test)]
    pub(crate) fn prefix_shown(&self, win: WinId) -> Option<(crate::keymap::prefix::Waiting, Vec<(char, ModuleId)>)> {
        self.windows
            .get(&win)
            .map(|window| (window.keys.waiting, window.keys.plan.clone()))
    }

    /// The window's focus state, for tests.
    #[cfg(test)]
    pub(crate) fn focus_state(&self, win: WinId) -> Option<&super::focus::FocusState> {
        self.windows.get(&win).map(|window| &window.focus)
    }

    /// What the controller reads of the shell, for tests that call a family's functions directly.
    #[cfg(test)]
    pub(crate) fn reads(&self) -> Rc<dyn HostReads> {
        self.reads.clone()
    }

    /// A pinned split of `win` got its length on screen, which is not a change. The queued `unheard_settles`
    /// entry is the tree as the settle left it: a change made between the allocation and this idle is
    /// still a change.
    fn settled(&mut self, win: WinId) {
        if let Some(window) = self.windows.get_mut(&win) {
            // A settle without a queued allocation is ignored, including one already heard by the close.
            let Some(root) = window.unheard_settles.pop_front() else {
                return;
            };
            if let Some(saver) = window.saver.as_mut() {
                saver.settled(&root);
            }
        }
    }

    /// The layout writer finished a write: every window takes what its writer finished.
    fn collect_saves(&mut self) {
        for window in self.windows.values_mut() {
            if let Some(saver) = window.saver.as_mut() {
                saver.collect(window.workspaces.try_borrow_mut().ok().as_deref_mut());
            }
        }
    }

    /// The saver of `win`, for the tests that wait for its writer.
    #[cfg(test)]
    pub(crate) fn saver(&self, win: WinId) -> Option<&super::persist::Saver> {
        self.windows.get(&win).and_then(|window| window.saver.as_ref())
    }
}

impl Decide for Controller {
    fn handle(&mut self, event: HostEvent) -> Batch {
        let mut batch = Batch::default();
        // An open with no window to take it is still owed its answer: left unanswered, its client would be told the
        // window has it.
        if let HostEvent::OpenFile { win, id, .. } = &event {
            if !self.windows.contains_key(win) {
                batch.push_effect(HostOp::OpenAnswer {
                    id: *id,
                    accepted: false,
                });
                return batch;
            }
        }
        if super::start::event_window(&event).is_some_and(|win| self.awaiting_layout(win, &event)) {
            return batch;
        }
        // A view stays busy while another of its tabs runs, and a `tab.break` waits only for its own tab: the panel
        // reports a change of which tabs run even when the view's counts come out the same.
        if let HostEvent::Chat(ChatEvent::Busy { win, busy: true, .. }) = event {
            self.modules_event(win, &mut batch, |window, cx, batch| {
                verbs::module_windows::turn_may_have_ended(window, cx.reads, win, batch);
            });
        }
        // While the base terminal is restarted in place, the end of the shell being replaced is not a kill.
        if let HostEvent::Terminal(ended @ TermEvent::Exited { win, module }) = &event {
            if let Some(window) = self.windows.get_mut(win) {
                if super::terminal_at::hold_exited(window, module, ended) {
                    return batch;
                }
            }
        }
        match &event {
            HostEvent::SurfaceEnded { win, module, .. }
            | HostEvent::Terminal(TermEvent::Started { win, module, .. } | TermEvent::Exited { win, module }) => {
                let started = matches!(event, HostEvent::Terminal(TermEvent::Started { .. }));
                if let Some(window) = self.windows.get_mut(win) {
                    super::terminal_at::shell_changed(window, *win, module, started, &mut batch);
                }
            }
            _ => {}
        }
        match &event {
            HostEvent::SurfaceEnded { win, .. }
            | HostEvent::Terminal(TermEvent::Started { win, .. } | TermEvent::Exited { win, .. })
            | HostEvent::Refused {
                what: OpTag::CreateSurface,
                surface: Some((win, _)),
                ..
            } => {
                self.lifecycle_event(*win, &event, &mut batch);
                if let Some(window) = self.windows.get_mut(win) {
                    super::focus::modules_changed(window, *win, &mut batch);
                }
            }
            _ => {}
        }
        match event {
            HostEvent::Lua(LuaEvent::LayoutRequests { win, requests }) => {
                // Queued only: `after_drain` decides the front once every event queued ahead of it and every
                // judgment of the batch before it has been heard, so neither an older pending request nor this
                // event's first one is decided against a state those have not caught up with.
                if let Some(window) = self.windows.get_mut(&win) {
                    window.lua_pending.extend(requests);
                }
                return Batch::default();
            }
            HostEvent::TextSize(event) => {
                let win = match &event {
                    TextSizeEvent::Both { win, .. }
                    | TextSizeEvent::Pane { win, .. }
                    | TextSizeEvent::PageAttached { win, .. }
                    | TextSizeEvent::Configured { win, .. }
                    | TextSizeEvent::Base { win, .. } => *win,
                };
                if let Some(window) = self.windows.get_mut(&win) {
                    verbs::text_size::event(window, event, &mut batch);
                }
            }
            HostEvent::Editor(EditorEvent::ScaleReported { win, scale }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    verbs::text_size::reported(window, win, scale, &mut batch);
                }
            }
            HostEvent::EndSignal { name } => {
                let wins: Vec<_> = self.windows.keys().copied().collect();
                for win in wins {
                    self.modules_event(win, &mut batch, |w, cx, batch| {
                        super::close::request(w, cx, win, super::close::CloseCause::Signal(name), batch)
                    });
                }
            }
            HostEvent::GroupClosed { win } => {
                self.modules_event(win, &mut batch, |w, cx, batch| {
                    super::close::request(w, cx, win, super::close::CloseCause::GroupClosed, batch)
                });
            }
            HostEvent::Editor(
                event @ (EditorEvent::Ready { win }
                | EditorEvent::Released { win }
                | EditorEvent::ReleaseRefused { win }
                | EditorEvent::RestartRefused { win, .. }
                | EditorEvent::StartFailed { win, .. }
                | EditorEvent::Exited { win, .. }
                | EditorEvent::Unreachable { win }
                | EditorEvent::Watched { win }
                | EditorEvent::Leaving { win }
                | EditorEvent::Recoverable { win, .. }
                | EditorEvent::QuitCancelled { win, .. }
                | EditorEvent::QuitNotSent { win, .. }
                | EditorEvent::OpenFailed { win, .. }),
            ) => {
                self.modules_event(win, &mut batch, |w, cx, batch| {
                    super::close::editor_event(w, cx, win, event, batch)
                });
            }
            HostEvent::Terminal(TermEvent::Exited { win, module }) => {
                self.modules_event(win, &mut batch, |w, cx, batch| {
                    if matches!(
                        w.close.state,
                        super::close::CloseState::TearingDown | super::close::CloseState::Closed
                    ) {
                        return;
                    }
                    let scope = w
                        .layout
                        .try_borrow()
                        .ok()
                        .map(|layout| crate::layout::can_kill(&layout, &module));
                    match scope {
                        Some(Ok(scope)) => match super::close::on_shell_exit(scope) {
                            super::close::OnExit::CloseWindow => {
                                w.say(format!(
                                    "[terminal] {module}: the shell ended; the last module on screen closes Eitri"
                                ));
                                super::close::request(
                                    w,
                                    cx,
                                    win,
                                    super::close::CloseCause::LastTerminalExit(module),
                                    batch,
                                )
                            }
                            // The kill's own hang-up closes the pane, so the shell's hook never closes it too.
                            super::close::OnExit::CloseModule => {
                                w.say(format!("[terminal] {module}: the shell ended; the terminal is closed"));
                                module_verbs::kill_confirmed(w, cx, win, &module, batch);
                            }
                        },
                        Some(Err(err)) => w.say(format!(
                            "[terminal] {module}: the shell ended, and the terminal stays: {err}"
                        )),
                        None => w.warn(format!(
                            "[terminal] BUG: the layout was borrowed when {module}'s shell ended"
                        )),
                    }
                });
            }
            // A session the other window holds, picked here: that window is asked to move it here.
            HostEvent::Chat(ChatEvent::ChooserPartnerPick { win, session, .. }) => {
                if let Some(w) = self.windows.get_mut(&win) {
                    super::moves::ask_session(w, session);
                }
            }
            HostEvent::Chat(ChatEvent::TabVerb { win, view, action }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    window.tab_view = Some(view);
                }
                batch.append(self.run_action(win, action, Origin::PanelTab));
                if let Some(window) = self.windows.get_mut(&win) {
                    window.tab_view = None;
                    super::moves::tab_verb(window);
                }
            }
            // A card arrived under a float: reveal it without moving the keys before the shell checks its toast.
            HostEvent::Chat(ChatEvent::Covered { win, view }) => {
                self.modules_event(win, &mut batch, |w, cx, batch| {
                    module_verbs::reveal_chat(w, cx, win, view, batch);
                });
            }
            HostEvent::Chat(ChatEvent::Reveal { win, view }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    verbs::chat::reveal(window, win, view, &mut batch);
                }
            }
            HostEvent::Chat(ChatEvent::ShowView { win, view }) => {
                self.modules_event(win, &mut batch, |w, cx, batch| {
                    verbs::chat::show_view(w, cx, win, view, batch)
                })
            }
            // Judged from an idle: the panel says so in the middle of its own call, which may give the view a tab
            // again before it returns.
            HostEvent::Chat(ChatEvent::ViewEmpty { win, view }) => {
                if let Some(window) = self.windows.get_mut(&win).filter(|_| !view.is_base()) {
                    let token = window.arm_idle();
                    self.view_empty.push((token, win, view));
                }
            }
            HostEvent::OpenFile {
                win,
                id,
                path,
                line,
                col,
                token,
            } => self.modules_event(win, &mut batch, |w, cx, batch| {
                let file = QueuedOpen { path, line, col };
                module_verbs::open_file(w, cx, win, id, file, token, batch);
            }),
            HostEvent::Chat(ChatEvent::ReviewOpen { win }) => self.modules_event(win, &mut batch, |w, cx, batch| {
                verbs::chat::review_open(w, cx, win, batch)
            }),
            HostEvent::Chat(ChatEvent::EditorDone { win, view }) => {
                self.modules_event(win, &mut batch, |w, cx, batch| {
                    verbs::chat::editor_done(w, cx, win, view, batch)
                })
            }
            HostEvent::Chat(ChatEvent::EditSelection { win }) => self.modules_event(win, &mut batch, |w, cx, batch| {
                module_verbs::edit_selection(w, cx, win, batch)
            }),
            // The chat calls that name a module when refused are a join back and a move's freeze.
            HostEvent::Refused {
                what: OpTag::Chat,
                why,
                surface: Some((win, module)),
            } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    match super::moves::chat_freezing(window, &module) {
                        Some(mv) => {
                            let of = super::moves::FreezeOf::Move {
                                module: module.clone(),
                                mv,
                            };
                            super::moves::freeze_refused(window, win, of, why, &mut batch);
                        }
                        None => verbs::chat::join_refused(window, win, &why, &mut batch),
                    }
                }
            }
            HostEvent::Timer(token) => self.timer_fired(token, &mut batch),
            HostEvent::Settled { win } => self.settled(win),
            HostEvent::Wake => self.collect_saves(),
            HostEvent::Created { win, module } => self.modules_event(win, &mut batch, |window, cx, batch| {
                module_verbs::created(window, cx, win, &module, batch);
            }),
            HostEvent::TopBarEntry { win, ev } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    verbs::workspaces::entry(window, win, ev, &mut batch);
                }
            }
            HostEvent::Onboarding(event) => {
                let win = match &event {
                    OnboardingEvent::Open { win, .. }
                    | OnboardingEvent::Found { win, .. }
                    | OnboardingEvent::Written { win, .. }
                    | OnboardingEvent::CheckFailed { win, .. }
                    | OnboardingEvent::NoScreen { win } => *win,
                };
                if let Some(window) = self.windows.get_mut(&win) {
                    super::onboarding::event(window, win, event, &mut batch);
                }
            }
            HostEvent::AnswerRefused { win, text } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    window.say(format!("[modules] refused: {text}"));
                    window.refuse(win, text, &mut batch);
                }
            }
            HostEvent::Terminal(TermEvent::Job {
                win,
                module,
                read,
                shell,
                cwd,
                busy,
            }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::terminal_at::job(window, win, &module, read, shell, cwd, busy, &mut batch);
                }
            }
            // The pane no longer runs the shell the restart would have ended: an `Exited` held meanwhile is taken now
            // as it would have been.
            HostEvent::Refused {
                what: OpTag::Respawn,
                why,
                surface: Some((win, module)),
            } => {
                let held = self
                    .windows
                    .get_mut(&win)
                    .and_then(|window| super::terminal_at::respawn_refused(window, win, &module, &why, &mut batch));
                if let Some(held) = held {
                    batch.append(self.handle(HostEvent::Terminal(held)));
                }
            }
            HostEvent::Terminal(TermEvent::Cwd { win, module, read, dir }) => {
                self.modules_event(win, &mut batch, |window, cx, batch| {
                    module_verbs::cwd_read(window, cx, win, &module, read, dir, batch)
                })
            }
            HostEvent::MonitorsChanged { monitors } => {
                self.monitors = monitors;
                for (win, window) in self.windows.iter_mut() {
                    window.monitors = self.monitors.clone();
                    // A remembered output that has just appeared is where the window goes.
                    window.put_back_on_its_monitor(*win, &mut batch);
                }
            }
            HostEvent::WindowMonitor { win, monitor } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    if window.monitor != monitor {
                        window.monitor = monitor;
                        super::moves::say_monitor(window);
                    }
                    window.put_back_on_its_monitor(win, &mut batch);
                }
            }
            // Neither is the user's doing, so neither is toasted: the window stays where the window manager put it,
            // and a window 2 that is not started again has had what it held given back here by the host.
            HostEvent::Refused {
                what: OpTag::PlaceWindow,
                why,
                ..
            } => eprintln!("[window] the window could not be put back on its monitor: {why}"),
            HostEvent::Refused {
                what: OpTag::RestoreLinkedWindow,
                why,
                ..
            } => eprintln!("[window] window 2 could not be started again: {why}"),
            // Asked for only by a window 2's own question, so its refusal is toasted there.
            HostEvent::Refused {
                what: what @ OpTag::StartEditorWindow,
                why,
                ..
            } => {
                for (win, window) in &mut self.windows {
                    if matches!(
                        window.role(),
                        super::window::WindowRole::Linked {
                            side: super::window::LinkSide::Second
                        }
                    ) {
                        verbs::windows::refused(window, *win, &what, &why, &mut batch);
                    }
                }
            }
            HostEvent::Peer(event) => self.peer_event(event, &mut batch),
            HostEvent::Pair(event) => self.pair_event(event, &mut batch),
            HostEvent::Terminal(TermEvent::Detached {
                win,
                module,
                mv,
                record,
                handoff,
            }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    let kind = super::moves::MoveKind::Terminal;
                    super::moves::frozen(window, win, kind, &module, mv, record, handoff, &mut batch);
                }
            }
            HostEvent::Plugin(PluginEvent::Frozen {
                win,
                module,
                mv,
                record,
                handoff,
            }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::frozen(
                        window,
                        win,
                        super::moves::MoveKind::PluginPage,
                        &module,
                        mv,
                        record,
                        handoff,
                        &mut batch,
                    );
                }
            }
            HostEvent::Plugin(PluginEvent::FreezeRefused { win, module, mv, why }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::page_freeze_refused(window, win, &module, mv, why, &mut batch);
                }
            }
            HostEvent::Plugin(PluginEvent::Ready { win, module }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::page_ready(window, win, &module, &mut batch);
                }
            }
            HostEvent::Chat(ChatEvent::Frozen {
                win,
                view,
                mv,
                record,
                handoff,
            }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    let kind = super::moves::MoveKind::Chat;
                    super::moves::frozen(window, win, kind, &view.module(), mv, record, handoff, &mut batch);
                }
            }
            HostEvent::Chat(ChatEvent::FreezeRefused { win, view, mv, why }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    let of = super::moves::FreezeOf::Move {
                        module: view.module(),
                        mv,
                    };
                    super::moves::freeze_refused(window, win, of, why, &mut batch);
                }
            }
            HostEvent::Chat(ChatEvent::PresenceHeld { win, mv }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::presence_answered(window, win, mv, Ok(()), &mut batch);
                }
            }
            HostEvent::Chat(ChatEvent::PresenceRefused { win, mv, why }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::presence_answered(window, win, mv, Err(why), &mut batch);
                }
            }
            // A chat's move that waited for its turn's end goes now, and the counts another window shows change.
            HostEvent::Chat(ChatEvent::Busy { win, view, busy: false }) => {
                self.modules_event(win, &mut batch, |window, cx, batch| {
                    super::moves::turn_ended(window, cx, win, view, batch);
                    verbs::module_windows::turn_may_have_ended(window, cx.reads, win, batch);
                });
                self.chat_chrome(&HostEvent::Chat(ChatEvent::Busy { win, view, busy: false }));
            }
            HostEvent::Chat(ChatEvent::TabKept { win, why }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    verbs::module_windows::tab_kept(window, why);
                }
            }
            HostEvent::Chat(ChatEvent::TabLeft { win }) => self.modules_event(win, &mut batch, |window, cx, batch| {
                verbs::module_windows::tab_left(window, cx, win, batch);
            }),
            HostEvent::Prepared { win, module } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::prepared(window, &module);
                }
            }
            HostEvent::Refused {
                what: OpTag::PrepareSurface,
                why,
                surface: Some((win, module)),
            } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::prepare_refused(window, win, &module, why, &mut batch);
                }
            }
            // Of the terminal's ops only a freeze names its module when refused.
            HostEvent::Refused {
                what: OpTag::Terminal,
                why,
                surface: Some((win, module)),
            } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    let of = super::moves::FreezeOf::Module(module);
                    super::moves::freeze_refused(window, win, of, why, &mut batch);
                }
            }
            HostEvent::Refused {
                what: OpTag::AdoptMoved,
                why,
                surface: Some((win, module)),
            } => self.modules_event(win, &mut batch, |window, cx, batch| {
                super::moves::adopt_refused(window, cx, win, &module, why, batch);
            }),
            HostEvent::Refused {
                what: OpTag::EndPeer,
                why,
                surface: None,
            } => {
                for (win, window) in &mut self.windows {
                    if window.has_peer() {
                        window.say(format!("[move] the other window could not be ended: {why}"));
                        window.refuse(*win, why.clone(), &mut batch);
                    }
                }
            }
            HostEvent::Refused {
                what: what @ (OpTag::OpenLinkedWindow | OpTag::RaisePeer | OpTag::ForwardRaise),
                why,
                ..
            } => self.window_op_refused(what, &why, &mut batch),
            // One module window did not come up: what waited for it is dropped and the key that asked can start it
            // again.
            HostEvent::ModuleWindowFailed { win, name, why } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    if verbs::module_windows::start_failed(window, Some(&name), &why) {
                        window.refuse(win, super::feedback::module_window_failed_text(&name, &why), &mut batch);
                    }
                }
            }
            // The compositor's own verbs refused: the refusal's text is what the user is shown, or the flash alone.
            HostEvent::Refused {
                what:
                    what @ (OpTag::CompositorFocus | OpTag::PeerRun | OpTag::HubAsk | OpTag::HandOverTab | OpTag::KeepTab),
                why,
                ..
            } => {
                for (win, window) in &mut self.windows {
                    if matches!(window.role(), super::window::WindowRole::Companion) {
                        continue;
                    }
                    window.say(format!("[window] {what:?} refused: {why}"));
                    window.refuse(*win, why.clone(), &mut batch);
                }
            }
            // The refusal names no window: every module window still starting is given up.
            HostEvent::Refused {
                what: OpTag::OpenModuleWindow,
                why,
                ..
            } => {
                for (win, window) in &mut self.windows {
                    if verbs::module_windows::start_failed(window, None, &why) {
                        window.refuse(*win, why.clone(), &mut batch);
                    }
                }
            }
            event => {
                if let HostEvent::Refused {
                    what: OpTag::CreateSurface,
                    why,
                    surface: Some((win, module)),
                } = &event
                {
                    if let Some(window) = self.windows.get_mut(win) {
                        module_verbs::create_refused(window, *win, module, why, &mut batch);
                    }
                }
                self.keys_event(&event, &mut batch);
                self.focus_event(&event, &mut batch);
                self.pointer_event(&event, &mut batch);
                self.chat_chrome(&event);
            }
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// One capture stage's answer for `press` ([`super::keys`]). A key that passes through, which is nearly
    /// every key, builds nothing on its way.
    fn key(&mut self, win: WinId, stage: &KeyStage, press: &KeyPress) -> (Verdict, Batch) {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("key", stage, press)) {
            return (Verdict::PassThrough, batch);
        }
        let reads = self.reads.clone();
        if *stage == KeyStage::Window {
            if press.press == Press::Escape
                && self
                    .windows
                    .get_mut(&win)
                    .is_some_and(|window| window.drag.take().is_some())
            {
                return (Verdict::Consumed, batch);
            }
            let answer = self
                .windows
                .get(&win)
                .and_then(|window| super::keys::prompt_press(window, press));
            if let Some(answer) = answer {
                let (reads, keymap, setup) = self.module_parts(win);
                if let Some(window) = self.windows.get_mut(&win) {
                    let cx = module_verbs::Cx {
                        reads: &*reads,
                        keymap: &keymap,
                        setup: &setup,
                    };
                    if window.chrome.prompt.as_ref().is_some_and(|prompt| prompt.closes_window) {
                        super::close::answer(window, &cx, win, answer, &mut batch);
                    } else {
                        module_verbs::prompt_answer(window, &cx, win, answer, &mut batch);
                    }
                }
                self.settle_chrome(&mut batch);
                return (Verdict::Consumed, batch);
            }
            // The first-start screen answers after the y/n and before the prefix.
            if let Some(window) = self.windows.get_mut(&win) {
                if let Some(effects) = super::keys::onboarding_press(window, press) {
                    super::onboarding::carry(window, win, effects, &mut batch);
                    self.settle_chrome(&mut batch);
                    return (Verdict::Consumed, batch);
                }
            }
        }
        let Some(window) = self.windows.get_mut(&win) else {
            return (Verdict::PassThrough, batch);
        };
        match stage {
            KeyStage::Window => {
                let (verdict, command) = super::keys::window_press(window, win, press, &mut batch);
                if let Some(command) = command {
                    let more = self.prefix_command(win, command, press.keycode);
                    batch.append(more);
                }
                self.settle_chrome(&mut batch);
                (verdict, batch)
            }
            KeyStage::Module(module) => {
                let verdict = super::keys::module_press(window, &*reads, win, module, press, &mut batch);
                self.settle_chrome(&mut batch);
                (verdict, batch)
            }
        }
    }

    fn close_requested(&mut self, win: WinId, by: CloseBy) -> (CloseVerdict, Batch) {
        let mut batch = Batch::default();
        if !self.awaiting_layout(win, ("close_requested", &by)) {
            self.modules_event(win, &mut batch, |w, cx, batch| {
                super::close::request(
                    w,
                    cx,
                    win,
                    match by {
                        CloseBy::User => super::close::CloseCause::User,
                        CloseBy::Compositor => super::close::CloseCause::Compositor,
                    },
                    batch,
                )
            });
        }
        self.settle_chrome(&mut batch);
        (CloseVerdict::Stop, batch)
    }

    /// The one runner ([`Controller::run_action`]), and the chrome its batch leaves.
    fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
        let mut batch = self.run_action(win, action, origin);
        self.settle_chrome(&mut batch);
        batch
    }

    /// The coverage the batches the queue held left, judged when the layout or the size moved or a chat page came
    /// on screen; once those judgments answer nothing, the front of a window's pending Lua layout requests.
    fn after_drain(&mut self) -> Batch {
        let mut batch = Batch::default();
        let reads = self.reads.clone();
        for (win, window) in &mut self.windows {
            super::moves::after_batch(window, *win, &*reads);
            let keymap = self
                .keymaps
                .get(win)
                .map(|(keymap, _)| keymap.clone())
                .unwrap_or_else(|| Rc::new(Keymap::defaults()));
            let setup = self.setups.get(win).cloned().unwrap_or_default();
            let cx = module_verbs::Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            super::moves::workspace::progress(window, &cx, *win, &mut batch);
            super::close::moves_progress(window, &cx, *win, &mut batch);
            if chrome::coverage_due(window) {
                chrome::reconcile_coverage(window, *win, &*reads, &mut batch);
            }
        }
        self.settle_chrome(&mut batch);
        // A request must see the state left by all judgments of the preceding batch.
        if !batch.is_empty() {
            return batch;
        }
        while let Some(win) = self
            .windows
            .iter()
            .find_map(|(win, window)| (!window.lua_pending.is_empty()).then_some(*win))
        {
            let batch = self.next_lua_request(win);
            if !batch.is_empty() {
                return batch;
            }
        }
        batch
    }
}

impl Controller {
    /// The one runner: the selection's gate (a refusal says how to get back to one module), then the float's (a
    /// refusal says how to sink the float), then the action's family, decided here, then the gate's selection
    /// reset, whose text the chrome derives again. [`Decide::run`] derives the chrome once the whole batch is built.
    // Every key action, every `eitri.layout.run` and every panel tab verb goes through the controller's one
    // runner, so a key and a Lua call cannot drift apart; the runner meets the container selection's and the
    // float's rules and decides every family. The focus and keys verbs are the controller's too: what holds
    // the keys is reported from the host, for the verbs that act on it.
    fn run_action(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
        self.run_gated(win, action, origin, None)
    }

    /// [`Controller::run_action`], with `subject` the module a follow command names: its gates are the action's,
    /// and its operation is the follow's own on `subject`.
    fn run_gated(&mut self, win: WinId, action: Action, origin: Origin, subject: Option<&ModuleId>) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("run", &action, &origin)) {
            return batch;
        }
        let keycode = match origin {
            Origin::Key { keycode } => keycode,
            _ => 0,
        };
        if !self.windows.contains_key(&win) {
            // Nothing else decides a verb: one for a window the controller does not hold has nowhere to run.
            eprintln!("[wm] BUG: {} ran in {win:?}, which is not held", action.name());
            return batch;
        }
        let setup = self.setups.get(&win).cloned().unwrap_or_default();
        let keymap = self.keymaps.get(&win).map(|(keymap, _)| keymap.clone());
        let keymap = || keymap.unwrap_or_else(|| Rc::new(Keymap::defaults()));
        let reads = self.reads.clone();
        let window = self.windows.get_mut(&win).expect("checked above");
        // In compositor mode the compositor owns the layout: window 1 and a module window sort every action first.
        // What another window sent runs as it is, but for what only this window's verbs carry out, and a follow
        // command has its own operation.
        let module_gate = if origin == Origin::Hub {
            verbs::module_windows::gate_sent
        } else {
            verbs::module_windows::gate
        };
        let decided = match subject {
            Some(module) => {
                let show = matches!(action, Action::Module(_));
                verbs::module_windows::follow(window, win, module, show, &mut batch)
            }
            None => module_gate(window, &*reads, win, &action, &mut batch),
        };
        if decided {
            return batch;
        }
        let facts = window.layout.try_borrow().map(|layout| {
            (
                layout.selection(),
                layout.is_floating(layout.focus()),
                *layout.focus() == ModuleId::editor(),
            )
        });
        let Ok((selection, keys_on_a_float, keys_on_the_editor)) = facts else {
            window.warn(format!("[wm] BUG: the layout was borrowed when {} ran", action.name()));
            return batch;
        };
        // A chip is a module key from the top bar, which never holds the keys: neither gate is about it.
        let gated = origin != Origin::Chip;
        let gate = if gated {
            verbs::selection_gate(&action, selection)
        } else {
            SelectionGate::Run { reset: false }
        };
        if gate == SelectionGate::Refuse {
            let text = verbs::one_module_text(&keymap());
            window.say(format!("[modules] {}: {text}", action.name()));
            window.refuse(win, text, &mut batch);
            return batch;
        }
        if gated && verbs::float_gate(&action, keys_on_a_float) == FloatGate::Refuse {
            let text = verbs::no_split_text(&keymap());
            window.say(format!("[modules] {}: {text}", action.name()));
            window.refuse(win, text, &mut batch);
            return batch;
        }
        let reset = gate == SelectionGate::Run { reset: true };
        match verbs::family_of(&action, keys_on_a_float, keys_on_the_editor) {
            verbs::Family::Tiling => verbs::tiling::run(window, &*reads, win, &action, &mut batch),
            verbs::Family::Focus => verbs::focus::run(window, &*reads, win, &action, &mut batch),
            verbs::Family::Keys => verbs::keys::run(window, win, &action, keycode, &keymap(), &mut batch),
            verbs::Family::Chrome => verbs::chrome::run(window, win, &action, &mut batch),
            verbs::Family::Workspaces => verbs::workspaces::run(window, &*reads, win, &action, &setup, &mut batch),
            verbs::Family::Floats => {
                let keymap = keymap();
                let cx = module_verbs::Cx {
                    reads: &*reads,
                    keymap: &keymap,
                    setup: &setup,
                };
                verbs::floats::run(window, &cx, win, &action, &mut batch);
            }
            verbs::Family::Windows => {
                let keymap = keymap();
                let cx = module_verbs::Cx {
                    reads: &*reads,
                    keymap: &keymap,
                    setup: &setup,
                };
                verbs::windows::run(window, &cx, win, &action, &mut batch);
            }
            verbs::Family::Modules | verbs::Family::Close => {
                let keymap = keymap();
                let cx = module_verbs::Cx {
                    reads: &*reads,
                    keymap: &keymap,
                    setup: &setup,
                };
                match subject {
                    Some(module) => {
                        let show = matches!(action, Action::Module(_));
                        module_verbs::follow(window, &cx, win, module, show, &mut batch);
                    }
                    None => {
                        module_verbs::run(window, &cx, win, &action, &origin, &mut batch);
                    }
                }
            }
            verbs::Family::Panel => {
                let keymap = keymap();
                let cx = module_verbs::Cx {
                    reads: &*reads,
                    keymap: &keymap,
                    setup: &setup,
                };
                verbs::chat::run(window, &cx, win, &action, &mut batch);
            }
            verbs::Family::TextSize => match action {
                // A window of one module has one size, so both scopes are the same action there.
                Action::Text(change) if window.tiling != super::window::Tiling::ModuleWindow => {
                    // The module that holds the keys right now, where a key typed now would go.
                    let keys = window.focus.native_module().cloned();
                    verbs::text_size::run(window, win, change, keys.as_ref(), &mut batch);
                }
                Action::Text(change) | Action::TextAll(change) => {
                    verbs::text_size::all(window, win, change.into(), &mut batch);
                }
                _ => {}
            },
        }
        if reset && window.clear_selection() {
            window.layout_touched();
        }
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{key::KeyName, prefix::Press};
    use crate::layout::Workspaces;
    use crate::wm::fake::FakeScheduler;
    use crate::wm::persist::first_launch_layout;
    use crate::wm::window::{EditorPresence, WindowRole};
    use std::cell::RefCell;

    fn controller() -> Controller {
        Controller::new(FakeScheduler::new())
    }

    fn lua_driver() -> (Rc<super::super::Driver>, Rc<super::super::fake::FakeHost>) {
        use crate::wm::verbs::rig::{decl, rig, WIN};
        let r = rig(first_launch_layout(&[]).unwrap());
        r.host.set_grid_size(Size { w: 1200, h: 800 });
        for module in r.layout.borrow().modules() {
            r.host.with_surface(WIN, module.clone(), decl(&module).kind, false);
        }
        let driver = super::super::Driver::new(r.controller, r.host.clone(), r.scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        r.host.attach(Rc::downgrade(&sink));
        r.scheduler.attach(Rc::downgrade(&sink));
        driver.test_batch(|_| r.first);
        driver.test_batch(|_| {
            let mut batch = Batch::default();
            batch.push_layout(HostOp::Focus {
                win: WIN,
                to: FocusTo::Module(ModuleId::editor()),
                on_grab: vec![],
            });
            batch
        });
        (driver, r.host)
    }

    fn lua_event(requests: Vec<LayoutRequest>) -> HostEvent {
        HostEvent::Lua(LuaEvent::LayoutRequests {
            win: WinId::MAIN,
            requests,
        })
    }

    fn lua_ops(host: &super::super::fake::FakeHost, from: usize) -> Vec<HostOp> {
        host.log()[from..]
            .iter()
            .filter_map(|entry| match entry {
                super::super::fake::Logged::Op(op, Applied::Done) => Some(op.clone()),
                _ => None,
            })
            .collect()
    }

    fn individually_applied(requests: Vec<LayoutRequest>) -> Vec<HostOp> {
        let (driver, host) = lua_driver();
        let from = host.log().len();
        for request in requests {
            driver.test_batch(|c| c.lua_request(WinId::MAIN, request));
        }
        lua_ops(&host, from)
    }

    #[test]
    fn the_kernels_requests_run_in_order_through_one_event() {
        let (driver, host) = lua_driver();
        let from = host.log().len();
        let requests = vec![
            LayoutRequest::Run(Action::Zoom),
            LayoutRequest::Mark(ModuleId::agent()),
            LayoutRequest::Show(ModuleId::terminal()),
        ];
        let expected = individually_applied(requests.clone());
        assert!(expected
            .iter()
            .any(|op| matches!(op, HostOp::Map { module, .. } if *module == ModuleId::terminal())));
        driver.post(lua_event(requests));
        assert_eq!(lua_ops(&host, from), expected);
        assert_eq!(
            driver.query(|c| c.chrome(WinId::MAIN).top_bar.mark),
            Some("marked: agent".into())
        );
    }

    #[test]
    fn a_lua_focus_then_hide_hides_the_module_the_focus_reached() {
        let (driver, host) = lua_driver();
        assert_eq!(host.focus_owner(WinId::MAIN), Some(ModuleId::editor()));
        let from = host.log().len();
        let requests = vec![
            LayoutRequest::Focus(ModuleId::agent()),
            LayoutRequest::Run(Action::ModuleHide),
        ];
        driver.post(lua_event(requests.clone()));
        let ops = lua_ops(&host, from);
        assert_eq!(ops, individually_applied(requests));
        assert!(ops.contains(&HostOp::Unmap {
            win: WinId::MAIN,
            module: ModuleId::agent()
        }));
        assert!(!ops.contains(&HostOp::Unmap {
            win: WinId::MAIN,
            module: ModuleId::editor()
        }));
    }

    #[test]
    fn a_request_drained_while_a_batch_applies_runs_after_it() {
        let (driver, host) = lua_driver();
        let from = host.log().len();
        let weak = Rc::downgrade(&driver);
        host.on_apply(move |host, _, op| {
            if matches!(op, HostOp::Focus { to: FocusTo::Module(module), .. } if *module == ModuleId::agent()) {
                host.on_apply(|_, _, _| {});
                let driver = weak.upgrade().unwrap();
                let before = driver.query(|c| c.notes(WinId::MAIN).to_vec());
                host.post(lua_event(vec![
                    LayoutRequest::Mark(ModuleId::agent()),
                    LayoutRequest::Focus(ModuleId::terminal()),
                    LayoutRequest::Run(Action::ModuleHide),
                ]));
                assert_eq!(driver.query(|c| c.notes(WinId::MAIN).to_vec()), before);
            }
        });
        driver.post(lua_event(vec![
            LayoutRequest::Focus(ModuleId::agent()),
            LayoutRequest::Show(ModuleId::terminal()),
        ]));
        let expected = individually_applied(vec![
            LayoutRequest::Focus(ModuleId::agent()),
            LayoutRequest::Show(ModuleId::terminal()),
            LayoutRequest::Mark(ModuleId::agent()),
            LayoutRequest::Focus(ModuleId::terminal()),
            LayoutRequest::Run(Action::ModuleHide),
        ]);
        assert_eq!(lua_ops(&host, from), expected);
        assert!(driver.query(|c| c.notes(WinId::MAIN).iter().any(|note| note.contains("Mark("))));
    }

    /// A drain posted while an earlier drain's requests wait decides none of them itself: the events queued
    /// behind it by the batch that is applying are heard first, so the waiting hide meets the focus they report.
    #[test]
    fn a_drain_posted_behind_pending_requests_waits_for_the_queued_events() {
        let (driver, host) = lua_driver();
        let from = host.log().len();
        host.on_apply(move |host, _, op| {
            if matches!(op, HostOp::Focus { to: FocusTo::Module(module), .. } if *module == ModuleId::agent()) {
                host.on_apply(|_, _, _| {});
                host.post(lua_event(vec![LayoutRequest::Mark(ModuleId::agent())]));
                // The focus moves on before the queue drains, as a click elsewhere during the apply would.
                host.post(HostEvent::NativeFocus {
                    win: WinId::MAIN,
                    owner: Some(FocusOwner::Module(ModuleId::editor())),
                    chain: FocusChain(vec![ModuleId::editor()]),
                    active: true,
                    op_index: None,
                });
            }
        });
        driver.post(lua_event(vec![
            LayoutRequest::Focus(ModuleId::agent()),
            LayoutRequest::Run(Action::ModuleHide),
        ]));
        let unmapped: Vec<_> = lua_ops(&host, from)
            .into_iter()
            .filter_map(|op| match op {
                HostOp::Unmap { module, .. } => Some(module),
                _ => None,
            })
            .collect();
        assert_eq!(
            unmapped,
            vec![ModuleId::editor()],
            "the hide reads the focus the queued events left"
        );
    }

    #[test]
    fn a_refused_request_does_not_stall_the_ones_after_it() {
        let (driver, host) = lua_driver();
        let from = host.log().len();
        let refused = LayoutRequest::Hide(ModuleId::lua("missing"));
        assert!(individually_applied(vec![refused.clone()]).is_empty());
        driver.post(lua_event(vec![refused, LayoutRequest::Show(ModuleId::terminal())]));
        assert_eq!(
            lua_ops(&host, from),
            individually_applied(vec![LayoutRequest::Show(ModuleId::terminal())])
        );
        assert!(lua_ops(&host, from).contains(&HostOp::Map {
            win: WinId::MAIN,
            module: ModuleId::terminal()
        }));
    }

    #[test]
    fn the_drain_judges_coverage_before_deciding_the_next_request() {
        let (driver, host) = lua_driver();
        let from = host.log().len();
        let requests = vec![
            LayoutRequest::Hide(ModuleId::agent()),
            LayoutRequest::Show(ModuleId::agent()),
            LayoutRequest::Hide(ModuleId::agent()),
        ];
        let expected = individually_applied(requests.clone());
        assert!(expected
            .iter()
            .any(|op| matches!(op, HostOp::Chat(ChatOp::PageShown { .. }))));
        driver.post(lua_event(requests));
        assert_eq!(lua_ops(&host, from), expected);
    }

    #[test]
    fn the_lua_store_reads_through_the_controller() {
        use crate::lua::layout::{install, LayoutStore, Query};
        let (driver, host) = lua_driver();
        let (layout, workspaces) = driver.query(|c| c.views(WinId::MAIN)).expect("the window is held");
        let lua = Rc::new(mlua::Lua::new());
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(LayoutStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        store.borrow_mut().attach(Query::new(move || layout.copy()));
        store
            .borrow_mut()
            .attach_workspaces(Query::new(move || workspaces.copy()));
        store.borrow_mut().accept_requests();
        let hidden = |lua: &mlua::Lua| -> String {
            lua.load("return table.concat(eitri.layout.get().hidden, ',') .. '|' .. eitri.layout.get().workspace")
                .eval()
                .unwrap()
        };
        assert_eq!(hidden(&lua), "terminal|main", "the first launch hides the terminal");
        // A Lua call made while the show's batch is carried out reads the layout the batch's decision left: the
        // copy is taken when it asks, and the controller is not in the middle of deciding then.
        let during: Rc<RefCell<Option<String>>> = Rc::default();
        {
            let (lua, during) = (lua.clone(), during.clone());
            host.on_apply(move |_, _, op| {
                if matches!(op, HostOp::Map { module, .. } if *module == ModuleId::terminal()) {
                    *during.borrow_mut() = Some(hidden(&lua));
                }
            });
        }
        driver.post(lua_event(vec![LayoutRequest::Show(ModuleId::terminal())]));
        assert_eq!(during.borrow().as_deref(), Some("|main"));
        host.on_apply(|_, _, _| {});
        assert_eq!(hidden(&lua), "|main", "after the batch: its result");
    }

    /// A window holding `layout` in role `role` behind a driver over the fake host, every module of it hosted.
    fn pair_driver(
        layout: crate::layout::Layout,
        role: WindowRole,
    ) -> (
        Rc<super::super::Driver>,
        Rc<super::super::fake::FakeHost>,
        super::super::LayoutView,
    ) {
        use crate::wm::verbs::rig::decl;
        let host = super::super::fake::FakeHost::new();
        host.set_grid_size(Size { w: 1200, h: 800 });
        let scheduler = FakeScheduler::new();
        let mut c = Controller::new(scheduler.clone());
        c.set_reads(host.clone());
        let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
        for d in &hosted {
            host.with_surface(WinId::MAIN, d.module.clone(), d.kind.clone(), false);
        }
        let presence = if role == WindowRole::Main {
            EditorPresence::Owned
        } else {
            EditorPresence::None
        };
        c.open_window(
            WinId::MAIN,
            WindowSpec {
                role,
                presence,
                title: "project".into(),
            },
            WindowParts::new(layout, Workspaces::single(), None),
        );
        let first = c.first_apply(WinId::MAIN, &hosted);
        let (view, _) = c.views(WinId::MAIN).unwrap();
        let driver = super::super::Driver::new(c, host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        driver.test_batch(|_| first);
        (driver, host, view)
    }

    /// `editor | (agent / lua:notes / terminal#2)`, the page and the terminal instance held away for the other
    /// window.
    fn held_away() -> crate::layout::Layout {
        use crate::layout::{Axis, Layout, Node};
        let (notes, t2) = (ModuleId::lua("notes"), ModuleId::parse("terminal#2").unwrap());
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::split(Axis::Column, 0.5, Node::Leaf(notes.clone()), Node::Leaf(t2.clone())),
                ),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        layout.set_away(&notes, true).unwrap();
        layout.set_away(&t2, true).unwrap();
        layout
    }

    fn applied_since(host: &super::super::fake::FakeHost, from: usize) -> Vec<HostOp> {
        lua_ops(host, from)
            .into_iter()
            .filter(|op| !matches!(op, HostOp::Chrome { .. }))
            .collect()
    }

    fn position_of(ops: &[HostOp], what: impl Fn(&HostOp) -> bool) -> usize {
        ops.iter()
            .position(what)
            .unwrap_or_else(|| panic!("not in the batch: {ops:#?}"))
    }

    #[test]
    fn pair_returns_reopen_plugin_holdings_before_any_show_or_reload() {
        use crate::layout::{Layout, ModuleDecl, Placement};
        let page = ModuleId::lua("page");
        for event in [
            PairEvent::LinkEnded {
                win: WinId::MAIN,
                who: "process 7".into(),
            },
            PairEvent::RestoreFailed { win: WinId::MAIN },
        ] {
            for (deferred, built) in [(false, true), (true, false), (true, true)] {
                let mut layout = Layout::initial(&[ModuleDecl {
                    id: page.clone(),
                    placement: Placement::RightOfRoot,
                }])
                .unwrap();
                if deferred {
                    crate::layout::hide(
                        &mut layout,
                        &page,
                        &crate::layout::Frame::new(Size { w: 1200, h: 800 }, 1),
                    )
                    .unwrap();
                }
                layout.set_away(&page, true).unwrap();
                let mut c = controller();
                let host = super::super::fake::FakeHost::new();
                host.set_page_built(page.clone(), built);
                c.set_reads(host);
                c.open_window(
                    WinId::MAIN,
                    WindowSpec {
                        role: WindowRole::Main,
                        presence: EditorPresence::Owned,
                        title: "project".into(),
                    },
                    WindowParts::new(layout, Workspaces::single(), None),
                );
                c.first_apply(
                    WinId::MAIN,
                    &[SurfaceDecl {
                        module: page.clone(),
                        kind: SurfaceKind::PluginPage { plugin: "page".into() },
                        deferred,
                    }],
                );
                let batch = c.handle(HostEvent::Pair(event.clone()));
                let ops: Vec<_> = batch.ops().cloned().collect();
                let hold = HostOp::Plugin(PluginOp::Hold {
                    win: WinId::MAIN,
                    module: page.clone(),
                });
                assert_eq!(ops.iter().filter(|op| **op == hold).count(), 1, "{event:?}: {batch:?}");
                let held = position_of(&ops, |op| *op == hold);
                for (index, op) in ops.iter().enumerate() {
                    if matches!(op,
                        HostOp::Plugin(PluginOp::Build { module, .. })
                        | HostOp::Pair(PairOp::ReloadPage { module, .. }) if module == &page
                    ) {
                        assert!(held < index, "{event:?}: {batch:?}");
                    }
                }
                if deferred || matches!(event, PairEvent::LinkEnded { .. }) {
                    assert_eq!(c.module_shown(WinId::MAIN, &page), (false, false));
                    assert!(!ops.iter().any(|op| matches!(op,
                        HostOp::Plugin(PluginOp::Build { module, .. }) if module == &page
                    )));
                }
                if deferred && !built {
                    assert!(
                        !ops.iter().any(|op| matches!(op,
                            HostOp::Pair(PairOp::ReloadPage { module, .. }) if module == &page
                        )),
                        "a hidden page with no view waits for its first show: {batch:?}"
                    );
                    let batch = c.lua_request(WinId::MAIN, LayoutRequest::Show(page.clone()));
                    assert_eq!(
                        batch
                            .ops()
                            .filter(|op| matches!(op,
                                HostOp::Plugin(PluginOp::Build { module, .. }) if module == &page
                            ))
                            .count(),
                        1,
                        "{batch:?}"
                    );
                    let batch = c.lua_request(WinId::MAIN, LayoutRequest::Hide(page.clone()));
                    assert!(!batch
                        .ops()
                        .any(|op| matches!(op, HostOp::Plugin(PluginOp::Build { .. }))));
                    let batch = c.lua_request(WinId::MAIN, LayoutRequest::Show(page.clone()));
                    assert!(!batch
                        .ops()
                        .any(|op| matches!(op, HostOp::Plugin(PluginOp::Build { .. }))));
                } else if matches!(event, PairEvent::RestoreFailed { .. }) {
                    assert_eq!(
                        ops.iter()
                            .filter(|op| matches!(op,
                                HostOp::Pair(PairOp::ReloadPage { module, .. }) if module == &page
                            ))
                            .count(),
                        1,
                        "a kept view remains available for reload: {batch:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_link_end_brings_pages_back_hidden_and_defers_their_reload_op_until_show() {
        use crate::layout::{Layout, ModuleDecl, Placement};
        use crate::wm::module::SurfaceDecl;
        let (panel, page) = (ModuleId::lua("panel"), ModuleId::lua("page"));
        let mut layout = Layout::initial(&[panel.clone(), page.clone()].map(|id| ModuleDecl {
            id,
            placement: Placement::RightOfRoot,
        }))
        .unwrap();
        for id in [&panel, &page] {
            layout.set_away(id, true).unwrap();
        }
        let mut c = controller();
        c.open_window(
            WinId::MAIN,
            WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::Owned,
                title: "project".into(),
            },
            WindowParts::new(layout, Workspaces::single(), None),
        );
        c.first_apply(
            WinId::MAIN,
            &[
                SurfaceDecl {
                    module: panel.clone(),
                    kind: SurfaceKind::LuaPanel {
                        url: "eitri://panel".into(),
                        title: "panel".into(),
                    },
                    deferred: false,
                },
                SurfaceDecl {
                    module: page.clone(),
                    kind: SurfaceKind::PluginPage { plugin: "page".into() },
                    deferred: false,
                },
            ],
        );
        let batch = c.handle(HostEvent::Pair(PairEvent::LinkEnded {
            win: WinId::MAIN,
            who: "process 7".into(),
        }));
        assert_eq!(
            batch
                .ops()
                .filter_map(|op| match op {
                    HostOp::Plugin(PluginOp::Hold { module, .. }) => Some(module),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [&page],
            "only the plugin page takes a holding; the Lua panel reopens through its lifecycle"
        );
        assert!(!batch.ops().any(|op| matches!(
            op,
            HostOp::Plugin(PluginOp::Reload { .. }) | HostOp::Pair(PairOp::ReloadPage { .. })
        )));
        for id in [&panel, &page] {
            assert_eq!(c.module_shown(WinId::MAIN, id), (false, false));
            let batch = c.lua_request(WinId::MAIN, LayoutRequest::Show(id.clone()));
            let loads = batch.ops().filter(|op| matches!(op,
                HostOp::Plugin(PluginOp::Reload { module, .. }) | HostOp::Pair(PairOp::ReloadPage { module, .. }) if module == id
            )).count();
            assert_eq!(loads, 1, "{id}: {batch:?}");
            let ops: Vec<_> = batch.ops().cloned().collect();
            let allocated = position_of(&ops, |op| matches!(op, HostOp::Allocate { .. }));
            let loaded = position_of(&ops, |op| {
                matches!(op,
                    HostOp::Plugin(PluginOp::Reload { module, .. }) | HostOp::Pair(PairOp::ReloadPage { module, .. }) if module == id
                )
            });
            assert!(allocated < loaded, "{batch:?}");
            if id == &panel {
                assert!(batch.ops().any(|op| matches!(op,
                    HostOp::Plugin(PluginOp::Reload { module, reset_guard: true, .. }) if module == id
                )));
            } else {
                assert!(batch.ops().any(|op| matches!(op,
                    HostOp::Pair(PairOp::ReloadPage { module, .. }) if module == id
                )));
            }
            c.lua_request(WinId::MAIN, LayoutRequest::Hide(id.clone()));
            let batch = c.lua_request(WinId::MAIN, LayoutRequest::Show(id.clone()));
            assert!(!batch.ops().any(|op| matches!(
                op,
                HostOp::Plugin(PluginOp::Reload { .. }) | HostOp::Pair(PairOp::ReloadPage { .. })
            )));
        }
    }

    #[test]
    fn a_link_end_brings_the_modules_back_through_the_controller() {
        let (notes, t2) = (ModuleId::lua("notes"), ModuleId::parse("terminal#2").unwrap());
        for (role, orphaned) in [
            (WindowRole::Main, false),
            (
                WindowRole::Linked {
                    side: super::super::LinkSide::Second,
                },
                true,
            ),
        ] {
            let (driver, host, layout) = pair_driver(held_away(), role.clone());
            let from = host.log().len();
            driver.post(HostEvent::Pair(PairEvent::LinkEnded {
                win: WinId::MAIN,
                who: "process 7".into(),
            }));
            {
                let layout = layout.read();
                assert!(
                    !layout.is_shown(&notes) && !layout.is_away(&notes),
                    "{role:?}: the page is back"
                );
                assert!(
                    !layout.contains(&t2),
                    "{role:?}: the terminal ended with the other window"
                );
            }
            let ops = applied_since(&host, from);
            assert!(!ops.iter().any(|op| matches!(op,
                HostOp::Map { module, .. } if module == &notes
            )));
            assert!(!ops.iter().any(|op| matches!(
                op,
                HostOp::Plugin(PluginOp::Reload { .. }) | HostOp::Pair(PairOp::ReloadPage { .. })
            )));
            let allocated = position_of(&ops, |op| *op == HostOp::Allocate { win: WinId::MAIN });
            let told = ops
                .iter()
                .position(|op| matches!(op, HostOp::Pair(PairOp::Orphaned { .. })));
            assert_eq!(
                told.is_some(),
                orphaned,
                "{role:?}: only an orphaned window 2 keeps the file: {ops:#?}"
            );
            if let Some(told) = told {
                assert!(allocated < told, "{ops:#?}");
                let HostOp::Pair(PairOp::Orphaned { held, .. }) = &ops[told] else {
                    unreachable!()
                };
                assert!(held.contains("lua:notes") && held.contains("agent"), "{held:?}");
                assert!(!held.contains("terminal#2"), "{held:?}");
            }
            assert!(
                !ops.iter().any(|op| format!("{op:?}").contains("terminal#2")),
                "{role:?}: nothing is done for a module that did not come back: {ops:#?}"
            );
        }
    }

    #[test]
    fn a_window_2_that_does_not_come_back_returns_its_modules_through_the_controller() {
        let (notes, t2) = (ModuleId::lua("notes"), ModuleId::parse("terminal#2").unwrap());
        let (driver, host, layout) = pair_driver(held_away(), WindowRole::Main);
        let from = host.log().len();
        driver.post(HostEvent::Pair(PairEvent::RestoreFailed { win: WinId::MAIN }));
        assert!(layout.read().is_shown(&notes) && !layout.read().contains(&t2));
        let ops = applied_since(&host, from);
        let allocated = position_of(&ops, |op| *op == HostOp::Allocate { win: WinId::MAIN });
        let unrestored = position_of(&ops, |op| *op == HostOp::Pair(PairOp::Unrestored { win: WinId::MAIN }));
        let reload = position_of(&ops, |op| {
            *op == HostOp::Plugin(PluginOp::Reload {
                win: WinId::MAIN,
                module: notes.clone(),
                reset_guard: true,
            })
        });
        assert!(allocated < reload && reload < unrestored, "{ops:#?}");
        assert!(
            !ops.iter().any(|op| format!("{op:?}").contains("terminal#2")),
            "nothing is done for a module that did not come back: {ops:#?}"
        );
        let from = host.log().len();
        driver.post(HostEvent::Pair(PairEvent::RestoreFailed { win: WinId::MAIN }));
        let again = applied_since(&host, from);
        assert!(
            !again
                .iter()
                .any(|op| matches!(op, HostOp::Pair(PairOp::ReloadPage { .. }))),
            "nothing is away any more: {again:#?}"
        );
    }

    #[test]
    fn the_controller_passes_keys_through_and_stops_native_closes() {
        let mut c = controller();
        let win = WinId::MAIN;
        let press = KeyPress {
            keycode: 38,
            press: Press::Other,
            key: Some(KeyName::Char('a')),
            mods: Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
        };
        assert_eq!(
            c.key(win, &KeyStage::Window, &press),
            (Verdict::PassThrough, Batch::default())
        );
        assert_eq!(
            c.close_requested(win, CloseBy::User),
            (CloseVerdict::Stop, Batch::default())
        );
    }
}
