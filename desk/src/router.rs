//! The host's op router: every op the window controller emits is matched here and carried out through narrow
//! traits, so a host is a router plus a toolkit back end. The back end is two traits: [`Views`] (a window's modules
//! shown and hidden, given the keys, its chrome drawn, its grid and editor read) and [`WindowPort`] (the native
//! window opened, shown, closed, put in a mode, placed on an output). What a module's surface is, what a service
//! does, the pair, Lua, HINT and the rest are the collaborators a shell gives the router (`set_*`), each a trait or
//! a read that names no toolkit. Until one is set, its ops are refused as not carried out.
//!
//! **Re-entrancy.** A toolkit emits signals while an op is still running (a grab notifies the focus change before it
//! returns, a close emits the close request from inside itself), and a collaborator may post to the sink or call
//! back into the router. No method here holds a borrow of the router while it calls a collaborator, the views, the
//! window port or the sink: each clones what it calls out first.
//!
//! **The editor's quit.** `Editor(Quit)` goes to the editor's service as a watched request; one that could not be
//! sent is refused and also posted as `EditorEvent::QuitNotSent` with the quit's generation, since a refusal names
//! no call. `Editor(ReleaseExited)` is told as its own fact either way, `EditorEvent::Released` or
//! `ReleaseRefused`. `Editor(Restart)` starts a fresh nvim on the released pane with the spawn it carries
//! ([`ServiceSet::restart_editor`]); a pane that refused from the state it was in is refused as any failed op is
//! and told as `EditorEvent::RestartRefused` naming that state.
//!
//! **The pair.** `OpenLinkedWindow`, `RestoreLinkedWindow`, `StartEditorWindow`, `EndPeer`, `OpenModuleWindow`,
//! `PeerRun`, `HubAsk`, `HandOverTab` and `KeepTab` go through the [`LinkedWindows`] the shell sets, with an
//! activation token from the window port where the window that comes forward needs one; `RaisePeer` and
//! `ForwardRaise` answer with `PeerEvent::Raised`, on which the controller asks the other window to come forward.
//!
//! **Moves.** An arriving module's `PrepareSurface` and `AdoptMoved` go to the [`SurfaceMaker`]; its place must
//! then be registered with the views, and an adoption takes the descriptors out of the op whatever comes of it.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use eitri_core::attention::Attention;
use eitri_core::layout::{Direction, ModuleId, Size};
use eitri_core::tabs::ViewId;
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::Msg;
use eitri_core::wm::{
    ActivationToken, Applied, ChatKillCounts, ChooserEnv, DropWhy, EditorCall, EditorEvent, FocusTo, HintOp, Host,
    HostEvent, HostOp, HostReads, HostSink, LuaOp, Monitor, NoReads, OnboardingOp, OpTag, PeerEvent, PeerId, ProjectOp,
    SurfaceKind, WinId, WindowSpec,
};

use crate::host_traits::{
    BasePageMaker, BesidePort, EditorReads, GridPort, HintPort, LinkedWindows, LuaRunner, ProjectHost, RestartRefused,
    ServiceSet, SurfaceMaker, NOT_CARRIED_OUT,
};

/// What carries out the first-start screen's own ops (the checks, the detection, the write, the marker, the editor):
/// the shell's onboarding host. It answers each as a fact later, through the driver.
pub type OnboardingFn = dyn Fn(&OnboardingOp) -> Applied;

/// What a chat view holds for the user, as its chip and its strip entry show it: read only, it never calls back.
pub type ViewAttentionFn = dyn Fn(ViewId) -> Attention;

/// Whether a chat page keeps a navigation key for itself: read only, it never calls back.
pub type ChatClaimsFn = dyn Fn(ViewId, Direction) -> bool;

/// The chat view holding the oldest card waiting for an answer: read only, it never calls back.
pub type OldestCardFn = dyn Fn(WinId) -> Option<ViewId>;

/// What a chat view's kill would end, and whether its tabs can close now: read only, it never calls back.
pub type KillCountsFn = dyn Fn(ViewId) -> ChatKillCounts;

/// Whether a new chat view can be given its first tab now (a break-out when the flag is set): read only, it never
/// calls back.
pub type FirstTabFn = dyn Fn(ViewId, bool) -> Result<(), String>;

/// What a window's chat summary reads: read only, it never calls back.
pub type ChatSummaryFn = dyn Fn(WinId) -> eitri_core::wm::ChatSummary;

/// A window's modules as its toolkit shows them: which have a surface, which hold the keys, its chrome, and the grid
/// and editor the controller's reads answer from. Each call does what it names at once and reports a focus change
/// it causes through the sink it was attached to; `Err` is the reason the op it serves is refused with.
pub trait Views {
    /// Where the views report what the toolkit did: the keys moving, a pointer gesture on the grid.
    fn attach(&self, sink: Weak<dyn HostSink>);
    /// Whether `module` has a surface registered in `win`.
    fn has_surface(&self, win: WinId, module: &ModuleId) -> bool;
    /// `win`'s grid, once the shell has given it.
    fn grid(&self, win: WinId) -> Option<Rc<dyn GridPort>>;
    /// `win`'s editor, once the shell has given it.
    fn editor(&self, win: WinId) -> Option<Rc<dyn EditorReads>>;
    /// Gives `module` the keys, and reports where they are even when the toolkit tells nothing (they were there).
    fn focus_module(&self, win: WinId, module: &ModuleId) -> Result<(), String>;
    /// Gives the first top-bar item showing from `item` on the keys, the bar brought out first if it is hidden.
    fn focus_top_bar(&self, win: WinId, item: u32) -> Result<(), String>;
    /// Gives the top bar's rename entry the keys, the bar brought out and the entry shown first.
    fn focus_rename_entry(&self, win: WinId) -> Result<(), String>;
    /// Gives the first-start screen the keys, the screen shown first.
    fn focus_onboarding(&self, win: WinId) -> Result<(), String>;
    /// Runs `grab`, which gives something in `win` the keys (the project chooser), and when it took, reports where
    /// they are if the toolkit told nothing. Whether it took.
    fn grab_reported(&self, win: WinId, grab: &dyn Fn() -> bool) -> bool;
    /// Takes the keys from `module`'s surface in `win`, if they are in it, before it is hidden: they would go on
    /// into a surface no one can see.
    fn release_keys(&self, win: WinId, module: &ModuleId);
    /// Shows or hides `module`'s surface in `win`.
    fn show(&self, win: WinId, module: &ModuleId, shown: bool);
    /// Draws what changed in `win`'s chrome model since it was last drawn.
    fn chrome(&self, win: WinId) -> Result<(), String>;
}

/// A host's native windows: opened, shown, closed, put in a mode, placed; where their close requests, activation
/// and the compositor's tokens come from.
pub trait WindowPort {
    /// Where the windows report their close requests and activation.
    fn attach(&self, sink: Weak<dyn HostSink>);
    /// Whether `win` is a window this host holds now.
    fn is_open(&self, win: WinId) -> bool;
    /// Builds `win`'s window, created and not shown, and adopts it.
    fn open(&self, win: WinId, spec: &WindowSpec) -> Result<(), String>;
    /// Shows `win`, with the launcher's activation token when the controller has one.
    fn present(&self, win: WinId, activation: Option<&ActivationToken>) -> Result<(), String>;
    /// Closes `win` without asking.
    fn close(&self, win: WinId) -> Result<(), String>;
    /// Toggles `win`'s mode as the window's own toggle does.
    fn toggle_mode(&self, win: WinId) -> Result<(), String>;
    /// An activation token from `win`'s display for the window that comes forward next; `None` when none is given.
    fn activation_token(&self, win: WinId) -> Option<String>;
    /// Moves `win` to the output `connector` if needed and applies `role`'s focus, never waiting.
    fn place(&self, win: WinId, connector: Option<&str>, role: eitri_core::wm::PlacementRole) -> Result<(), String>;
    /// Has the compositor move the keys one window `direction`. `Err(Some(text))` is a refusal to show; `Err(None)`
    /// a refusal already said once.
    fn compositor_focus(&self, direction: Direction) -> Result<(), Option<String>>;
    /// The name of the window manager this window's moves go through.
    fn compositor_name(&self) -> &'static str;
    /// The outputs the windows can be placed on.
    fn monitors(&self) -> Vec<Monitor>;
}

/// The router over a host's views `V` and windows `W`, with the collaborators the shell sets.
pub struct Router<V: Views + ?Sized, W: WindowPort + ?Sized> {
    me: Weak<Router<V, W>>,
    sink: RefCell<Option<Weak<dyn HostSink>>>,
    views: Rc<V>,
    windows: Rc<W>,
    surface_maker: RefCell<Option<Rc<dyn SurfaceMaker>>>,
    base_page_maker: RefCell<Option<Rc<dyn BasePageMaker>>>,
    services: RefCell<Option<Rc<dyn ServiceSet>>>,
    lua: RefCell<Option<Rc<dyn LuaRunner>>>,
    onboarding_fn: RefCell<Option<Rc<OnboardingFn>>>,
    project_host: RefCell<Option<Rc<dyn ProjectHost>>>,
    chat_claims_fn: RefCell<Option<Rc<ChatClaimsFn>>>,
    view_attention_fn: RefCell<Option<Rc<ViewAttentionFn>>>,
    oldest_card_fn: RefCell<Option<Rc<OldestCardFn>>>,
    kill_counts_fn: RefCell<Option<Rc<KillCountsFn>>>,
    summary_fn: RefCell<Option<Rc<ChatSummaryFn>>>,
    first_tab_fn: RefCell<Option<Rc<FirstTabFn>>>,
    hints: RefCell<Option<Rc<dyn HintPort>>>,
    linked_windows: RefCell<Option<Rc<dyn LinkedWindows>>>,
    beside: RefCell<Option<Rc<dyn BesidePort>>>,
}

impl<V: Views + ?Sized + 'static, W: WindowPort + ?Sized + 'static> Router<V, W> {
    pub fn new(views: Rc<V>, windows: Rc<W>) -> Rc<Router<V, W>> {
        Rc::new_cyclic(|me| Router {
            me: me.clone(),
            sink: RefCell::new(None),
            views,
            windows,
            surface_maker: RefCell::new(None),
            base_page_maker: RefCell::new(None),
            services: RefCell::new(None),
            lua: RefCell::new(None),
            onboarding_fn: RefCell::new(None),
            project_host: RefCell::new(None),
            chat_claims_fn: RefCell::new(None),
            view_attention_fn: RefCell::new(None),
            oldest_card_fn: RefCell::new(None),
            kill_counts_fn: RefCell::new(None),
            summary_fn: RefCell::new(None),
            first_tab_fn: RefCell::new(None),
            hints: RefCell::new(None),
            linked_windows: RefCell::new(None),
            beside: RefCell::new(None),
        })
    }

    /// Where the router, its views and its windows report. Held weakly: the sink (the driver) owns the router.
    pub fn attach(&self, sink: Weak<dyn HostSink>) {
        *self.sink.borrow_mut() = Some(sink.clone());
        self.views.attach(sink.clone());
        self.windows.attach(sink);
    }

    fn post(&self, event: HostEvent) {
        let sink = self.sink.borrow().clone();
        match sink.and_then(|s| s.upgrade()) {
            Some(sink) => sink.post(event),
            None => eprintln!("[wm] the host has no sink attached: {event:?} dropped"),
        }
    }

    /// What `CreateSurface` and `DropSurface` build and release through.
    pub fn set_surface_maker(&self, maker: Rc<dyn SurfaceMaker>) {
        *self.surface_maker.borrow_mut() = Some(maker);
    }

    /// What the service ops call. Set once, before the first batch that carries one.
    pub fn set_services(&self, services: Rc<dyn ServiceSet>) {
        *self.services.borrow_mut() = Some(services);
    }

    /// What `BuildBasePage` builds the window's base chat page through.
    pub fn set_base_page_maker(&self, maker: Rc<dyn BasePageMaker>) {
        *self.base_page_maker.borrow_mut() = Some(maker);
    }

    /// What the `Lua` ops run on. Set before the first batch that carries one.
    pub fn set_lua(&self, lua: Rc<dyn LuaRunner>) {
        *self.lua.borrow_mut() = Some(lua);
    }

    /// What carries out the first-start screen's ops. Until it is set, they are not carried out.
    pub fn set_onboarding(&self, carry: Rc<OnboardingFn>) {
        *self.onboarding_fn.borrow_mut() = Some(carry);
    }

    /// The native project chooser and its asynchronous work.
    pub fn set_project_host(&self, host: Rc<dyn ProjectHost>) {
        *self.project_host.borrow_mut() = Some(host);
    }

    /// What a chat view holds for the user, as the controller reads it for the chips and the strips. Until it is
    /// set, every view holds nothing.
    pub fn set_view_attention(&self, attention: Rc<ViewAttentionFn>) {
        *self.view_attention_fn.borrow_mut() = Some(attention);
    }

    /// Whether a chat page claims a navigation key, as the controller reads it while it decides that key.
    /// Until it is set, no page claims one.
    pub fn set_chat_claims(&self, claims: Rc<ChatClaimsFn>) {
        *self.chat_claims_fn.borrow_mut() = Some(claims);
    }

    /// Which chat view holds the oldest waiting card, as the chat's key reads it. Until it is set, none does.
    pub fn set_oldest_card(&self, oldest: Rc<OldestCardFn>) {
        *self.oldest_card_fn.borrow_mut() = Some(oldest);
    }

    /// What a chat view's kill would end, as its y/n names it, and whether its tabs can close now. Until it is set,
    /// nothing runs in any view and every tab can close.
    pub fn set_chat_summary(&self, read: Rc<ChatSummaryFn>) {
        *self.summary_fn.borrow_mut() = Some(read);
    }

    pub fn set_chat_kill_counts(&self, counts: Rc<KillCountsFn>) {
        *self.kill_counts_fn.borrow_mut() = Some(counts);
    }

    /// Whether a new chat view can be given its first tab, asked before it is placed. Until it is set, it can.
    pub fn set_chat_first_tab(&self, first_tab: Rc<FirstTabFn>) {
        *self.first_tab_fn.borrow_mut() = Some(first_tab);
    }

    /// What the `Hint` ops draw and toggle through. Until it is set, they are not carried out.
    pub fn set_hints(&self, hints: Rc<dyn HintPort>) {
        *self.hints.borrow_mut() = Some(hints);
    }

    /// What carries out the ops of a page window beside nvim, in that window only.
    pub fn set_beside(&self, port: Rc<dyn BesidePort>) {
        *self.beside.borrow_mut() = Some(port);
    }

    /// What `OpenLinkedWindow` starts the second window through. Until it is set, none can be started.
    pub fn set_linked_windows(&self, linked: Rc<dyn LinkedWindows>) {
        *self.linked_windows.borrow_mut() = Some(linked);
    }

    fn refuse(&self, op: &HostOp, why: &str) -> Applied {
        self.post(HostEvent::Refused {
            what: OpTag::of(op),
            why: why.into(),
            surface: op.surface(),
        });
        Applied::Refused
    }

    /// The answer to an op the views or the window port carried out, or refused with their reason.
    fn answer(&self, op: &HostOp, result: Result<(), String>) -> Applied {
        match result {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    fn focus(&self, op: &HostOp, win: WinId, to: &FocusTo) -> Applied {
        if !self.windows.is_open(win) {
            return self.refuse(op, "no such window");
        }
        let result = match to {
            FocusTo::Module(module) => self.views.focus_module(win, module),
            FocusTo::TopBarItem(item) => self.views.focus_top_bar(win, *item),
            FocusTo::RenameEntry => self.views.focus_rename_entry(win),
            FocusTo::Onboarding => self.views.focus_onboarding(win),
            FocusTo::ProjectChooser => return self.focus_project_chooser(op, win),
            FocusTo::Nothing => Err(NOT_CARRIED_OUT.into()),
        };
        self.answer(op, result)
    }

    fn focus_project_chooser(&self, op: &HostOp, win: WinId) -> Applied {
        let host = self.project_host.borrow().clone();
        if self
            .views
            .grab_reported(win, &|| host.as_ref().is_some_and(|host| host.focus(win)))
        {
            Applied::Done
        } else {
            self.refuse(op, "the project chooser did not take the keys")
        }
    }

    fn onboarding(&self, op: &HostOp, call: &OnboardingOp) -> Applied {
        // Cloned out first: the work it starts may post an event at once.
        let carry = self.onboarding_fn.borrow().clone();
        match carry {
            Some(carry) => match carry(call) {
                Applied::Done => Applied::Done,
                _ => self.refuse(op, "the first-start screen's work was not started"),
            },
            None => self.refuse(op, NOT_CARRIED_OUT),
        }
    }

    fn project(&self, op: &HostOp, call: &ProjectOp) -> Applied {
        let win = match call {
            ProjectOp::List { win, .. }
            | ProjectOp::Read { win, .. }
            | ProjectOp::FolderDialog { win, .. }
            | ProjectOp::Open { win, .. } => *win,
        };
        if !self.windows.is_open(win) {
            return self.refuse(op, "no such window");
        }
        let host = self.project_host.borrow().clone();
        let applied = match (host, call) {
            // The token names the input that asked, so it is taken here, before the start leaves this thread.
            (Some(host), ProjectOp::Open { target, .. }) => host.open(win, target, self.windows.activation_token(win)),
            (Some(host), _) => host.apply(call),
            (None, _) => Applied::Refused,
        };
        if applied == Applied::Done {
            Applied::Done
        } else {
            self.refuse(op, NOT_CARRIED_OUT)
        }
    }

    fn hint(&self, op: &HostOp, call: &HintOp) -> Applied {
        // Cloned out first: the digits draw into the window, and a HINT adds its controllers.
        let hints = self.hints.borrow().clone();
        let Some(hints) = hints else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match call {
            HintOp::ShowDigits(digits) => hints.show_digits(WinId::MAIN, digits),
            HintOp::Hide => hints.hide_digits(WinId::MAIN),
            HintOp::Toggle { keycode } => hints.toggle(WinId::MAIN, *keycode),
        }
        Applied::Done
    }

    /// A `Map` or an `Unmap`: nothing for a host the grid's probe places by hand.
    fn map_or_unmap(&self, op: &HostOp, win: WinId) -> Applied {
        // Cloned out first: the probe's answer reads the grid, and nothing it calls should find the router borrowed.
        let grid = self.views.grid(win);
        let probe_places = |module: &ModuleId| grid.as_ref().is_some_and(|grid| grid.probe_places(module));
        match hosts_to_map(std::slice::from_ref(op), probe_places).pop() {
            Some((win, module, mapped)) => self.set_mapped(op, win, &module, mapped),
            None => Applied::Done,
        }
    }

    /// Runs `f` on `win`'s grid; refused for a window that has given none.
    fn on_grid(&self, op: &HostOp, win: WinId, f: impl FnOnce(&dyn GridPort)) -> Applied {
        // Cloned out first: the grid allocates and restacks, which may call back into the host.
        let Some(grid) = self.views.grid(win) else {
            return self.refuse(op, "the window has no grid");
        };
        f(grid.as_ref());
        Applied::Done
    }

    fn set_mapped(&self, op: &HostOp, win: WinId, module: &ModuleId, mapped: bool) -> Applied {
        if !self.views.has_surface(win, module) {
            return self.refuse(op, "the module has no surface in this window");
        }
        if !mapped {
            self.views.release_keys(win, module);
        }
        let maker = self.surface_maker.borrow().clone();
        if let Some(maker) = maker {
            maker.mapped(module, mapped);
        }
        self.views.show(win, module, mapped);
        Applied::Done
    }

    fn create_surface(&self, op: &HostOp, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Applied {
        // Cloned out first: the maker adds widgets, and the toolkit may call back into the host meanwhile.
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if let Err(why) = maker.create(win, module, kind) {
            return self.refuse(op, &why);
        }
        if !self.views.has_surface(win, module) {
            return self.refuse(op, "the surface was built but never registered");
        }
        Applied::Done
    }

    fn drop_surface(&self, op: &HostOp, win: WinId, module: &ModuleId, why: &DropWhy) -> Applied {
        if !self.views.has_surface(win, module) {
            return self.refuse(op, "the module has no surface in this window");
        }
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        self.answer(op, maker.drop_surface(win, module, why))
    }

    fn prepare_surface(
        &self,
        op: &HostOp,
        win: WinId,
        module: &ModuleId,
        shape: &eitri_core::window_link::Shape,
        size: (u32, u32),
    ) -> Applied {
        // Cloned out first: the maker adds widgets, and the toolkit may call back into the host meanwhile.
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if let Err(why) = maker.prepare(win, module, shape, size) {
            return self.refuse(op, &why);
        }
        if !self.views.has_surface(win, module) {
            return self.refuse(op, "the arriving module's place was made but never registered");
        }
        Applied::Done
    }

    /// The descriptors are taken out of the op here, whatever comes of it: refused, they close as they drop.
    fn adopt_moved(
        &self,
        op: &HostOp,
        win: WinId,
        module: &ModuleId,
        kind: eitri_core::wm::moves::MoveKind,
        record: &serde_json::Value,
        fds: &eitri_core::wm::Descriptors,
    ) -> Applied {
        let fds = fds.take();
        if !self.views.has_surface(win, module) {
            return self.refuse(op, "the arriving module has no place in this window");
        }
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        self.answer(op, maker.adopt(win, module, kind, record, fds))
    }

    /// The pair's code, cloned out first: a start wires the new link and may call back into the toolkit. Without
    /// one, every op that reaches another window is refused with the words that say there is none.
    fn linked(&self, op: &HostOp, f: impl FnOnce(&dyn LinkedWindows) -> Result<(), String>) -> Applied {
        self.linked_or(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT, f)
    }

    fn linked_or(&self, op: &HostOp, none: &str, f: impl FnOnce(&dyn LinkedWindows) -> Result<(), String>) -> Applied {
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, none);
        };
        self.answer(op, f(linked.as_ref()))
    }

    fn open_linked_window(&self, op: &HostOp, win: WinId) -> Applied {
        if !self.windows.is_open(win) {
            return self.refuse(op, "no such window");
        }
        self.linked(op, |linked| {
            linked.open(win, &WindowName::Second, self.windows.activation_token(win))
        })
    }

    fn restore_linked_window(&self, op: &HostOp, win: WinId) -> Applied {
        if !self.windows.is_open(win) {
            return self.refuse(op, "no such window");
        }
        self.linked(op, |linked| linked.restore(win, &WindowName::Second))
    }

    /// Starts the module window `name`, or raises it when it runs. The window that comes forward takes the activation
    /// token of the key that asked, or of this window's surface when none came; a window the start opens takes none, and
    /// leaves the keys where they are.
    fn open_module_window(
        &self,
        op: &HostOp,
        win: WinId,
        name: &WindowName,
        token: Option<&str>,
        keep_keys: bool,
    ) -> Applied {
        self.linked(op, |linked| {
            let token = if keep_keys {
                None
            } else {
                match token {
                    Some(token) => Some(token.to_string()),
                    None if self.windows.is_open(win) => self.windows.activation_token(win),
                    None => None,
                }
            };
            linked.open_module(win, name, token, keep_keys)
        })
    }

    /// A raise a module window asked for: the token that came with it is the one the window that comes forward gets,
    /// so it is answered as it would be had this window made one.
    fn forward_raise(&self, peer: PeerId, token: Option<&str>) -> Applied {
        self.post(HostEvent::Peer(PeerEvent::Raised {
            peer,
            token: token.map(str::to_string),
        }));
        Applied::Done
    }

    /// The key action `name` goes to the module window `peer`, which parses and runs it itself.
    fn peer_run(
        &self,
        op: &HostOp,
        win: WinId,
        peer: PeerId,
        name: &str,
        opts: &[(String, serde_json::Value)],
    ) -> Applied {
        self.linked(op, |linked| {
            let msg = Msg::RunAction {
                name: name.to_string(),
                opts: opts.to_vec(),
            };
            linked.send(win, peer, msg)
        })
    }

    /// A module window asks window 1 for another window, with an activation token from its own surface for the window
    /// that comes forward.
    fn hub_ask(&self, op: &HostOp, win: WinId, ask: &eitri_core::window_link::HubAsk) -> Applied {
        self.linked(op, |linked| {
            let msg = Msg::HubAsk {
                ask: ask.clone(),
                token: self.windows.activation_token(win),
            };
            linked.send_to_hub(win, msg)
        })
    }

    /// Window 1 tells a module window that the tab it offered stays there.
    fn keep_tab(&self, op: &HostOp, win: WinId, peer: PeerId, session: &str, why: &str) -> Applied {
        self.linked(op, |linked| {
            let msg = Msg::TabKept {
                session: session.to_string(),
                why: why.to_string(),
            };
            linked.send(win, peer, msg)
        })
    }

    /// A beside op, through the page window's carrier: refused now, or later by a worker, as [`OpTag::Beside`].
    fn beside(&self, op: &HostOp, beside: &eitri_core::wm::BesideOp) -> Applied {
        let port = self.beside.borrow().clone();
        let Some(port) = port else {
            eprintln!("[page] {beside:?}: no window beside nvim here to carry it out");
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        let me = self.me.clone();
        let later = Box::new(move |why: String| {
            if let Some(router) = me.upgrade() {
                router.post(HostEvent::Refused {
                    what: OpTag::Beside,
                    why,
                    surface: None,
                });
            }
        });
        self.answer(op, port.carry_out(beside, later))
    }

    fn start_editor_window(&self, op: &HostOp, win: WinId) -> Applied {
        if !self.windows.is_open(win) {
            return self.refuse(op, "no such window");
        }
        self.linked_or(op, eitri_core::wm::feedback::EDITOR_WINDOW_CLOSED_TEXT, |linked| {
            linked.start_editor(win)
        })
    }

    /// The other window comes forward by its own raise once it hears `focus{window}` with this token: under GNOME
    /// the token lets it take the keys from this window, and under sway, Hyprland and niri, which turn a token into
    /// an urgency hint, it also has the compositor focus it by its pid. The controller sends that message on the
    /// answer posted here.
    fn raise_peer(&self, op: &HostOp, win: WinId, peer: PeerId) -> Applied {
        if !self.windows.is_open(win) {
            return self.refuse(op, "no such window");
        }
        let token = self.windows.activation_token(win);
        self.post(HostEvent::Peer(PeerEvent::Raised { peer, token }));
        Applied::Done
    }

    /// A focus move past this window's edge: the compositor's own tool moves it, on its worker. With none, the notice
    /// that says so is the refusal, shown once.
    fn compositor_focus(&self, op: &HostOp, direction: Direction) -> Applied {
        match self.windows.compositor_focus(direction) {
            Ok(()) => Applied::Done,
            Err(Some(text)) => self.refuse(op, &text),
            Err(None) => Applied::Refused,
        }
    }

    fn lua(&self, op: &HostOp, call: &LuaOp) -> Applied {
        // Cloned out first: the kernel runs Lua, which may post to the sink or call into the toolkit.
        let lua = self.lua.borrow().clone();
        let Some(lua) = lua else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        self.answer(op, lua.run(call))
    }

    fn service(&self, op: &HostOp, call: impl FnOnce(&dyn ServiceSet) -> Result<(), String>) -> Applied {
        // Cloned out first: a service may post to the sink or call into the toolkit.
        let services = self.services.borrow().clone();
        let Some(services) = services else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        self.answer(op, call(services.as_ref()))
    }

    /// The restart's refusal, as any failed op's and as its own fact naming the pane's state, so the editor can be
    /// shown and tried again; a window with nothing to restart says `unavailable`.
    fn restart_editor(&self, op: &HostOp, win: WinId, spawn: &eitri_core::wm::EditorSpawn) -> Applied {
        // Cloned out first: the pane's restart may queue a render, and a service may post to the sink.
        let services = self.services.borrow().clone();
        let refused = match services {
            Some(services) => match services.restart_editor(spawn) {
                Ok(()) => return Applied::Done,
                Err(refused) => refused,
            },
            None => RestartRefused::Unavailable(NOT_CARRIED_OUT.into()),
        };
        let (why, state) = match refused {
            RestartRefused::Pane { state } => (
                format!("the editor pane is {state}; only a released pane restarts"),
                state,
            ),
            RestartRefused::Unavailable(why) => (why, "unavailable".to_string()),
        };
        let applied = self.refuse(op, &why);
        self.post(HostEvent::Editor(EditorEvent::RestartRefused { win, state }));
        applied
    }

    fn services(&self) -> Option<Rc<dyn ServiceSet>> {
        self.services.borrow().clone()
    }
}

impl<V: Views + ?Sized + 'static, W: WindowPort + ?Sized + 'static> Host for Router<V, W> {
    fn apply(&self, op: &HostOp) -> Applied {
        // Every variant is named, so a new op is a compile error here until the router decides.
        match op {
            HostOp::Focus { win, to, on_grab: _ } => self.focus(op, *win, to),
            HostOp::Map { win, .. } | HostOp::Unmap { win, .. } => self.map_or_unmap(op, *win),
            HostOp::Allocate { win } => self.on_grid(op, *win, |grid| grid.queue_allocation()),
            // The floats only, bottom first: each move places its float after the one below it.
            HostOp::Restack { win, moves } => {
                let order: Vec<ModuleId> = moves.iter().map(|step| step.module.clone()).collect();
                self.on_grid(op, *win, |grid| grid.restack_floats(&order))
            }
            HostOp::CloseWindow { win } => self.answer(op, self.windows.close(*win)),
            HostOp::CreateSurface { win, module, kind } => self.create_surface(op, *win, module, kind),
            HostOp::DropSurface { win, module, why } => self.drop_surface(op, *win, module, why),
            HostOp::Chrome { win, .. } => self.answer(op, self.views.chrome(*win)),
            HostOp::Hint(call) => self.hint(op, call),
            HostOp::Onboarding(call) => self.onboarding(op, call),
            HostOp::Project(call) => self.project(op, call),
            HostOp::Editor(call @ EditorCall::Quit { win, generation, .. }) => {
                let applied = self.service(op, |s| s.editor(call));
                // A refusal names no call, so the quit that sent nothing is told as its own fact, with its
                // generation, for the quit flow to clear it.
                if applied == Applied::Refused {
                    self.post(HostEvent::Editor(EditorEvent::QuitNotSent {
                        win: *win,
                        generation: *generation,
                    }));
                }
                applied
            }
            // Told as a fact either way: the restart a show asked for waits for the release.
            HostOp::Editor(call @ EditorCall::ReleaseExited { win }) => {
                let applied = self.service(op, |s| s.editor(call));
                self.post(HostEvent::Editor(match applied {
                    Applied::Done => EditorEvent::Released { win: *win },
                    _ => EditorEvent::ReleaseRefused { win: *win },
                }));
                applied
            }
            HostOp::Editor(EditorCall::Restart { win, spawn, .. }) => self.restart_editor(op, *win, spawn),
            HostOp::Editor(call) => self.service(op, |s| s.editor(call)),
            HostOp::Terminal(call) => self.service(op, |s| s.terminal(call)),
            HostOp::Plugin(call) => self.service(op, |s| s.plugin(call)),
            HostOp::Chat(call) => self.service(op, |s| s.chat(call)),
            HostOp::AgentsScan { win, step, at } => self.service(op, |s| s.agents_scan(*win, *step, at.as_ref())),
            HostOp::OpenWindow { win, spec } => self.answer(op, self.windows.open(*win, spec)),
            HostOp::Present { win, activation } => self.answer(op, self.windows.present(*win, activation.as_ref())),
            HostOp::OpenAnswer { id, accepted } => self.service(op, |s| s.open_answer(*id, *accepted)),
            HostOp::Lua(call) => self.lua(op, call),
            HostOp::OpenLinkedWindow { win } => self.open_linked_window(op, *win),
            HostOp::RestoreLinkedWindow { win } => self.restore_linked_window(op, *win),
            HostOp::StartEditorWindow { win } => self.start_editor_window(op, *win),
            HostOp::PlaceWindow { win, monitor, role } => {
                let placed = self.windows.place(*win, monitor.as_ref().map(|m| m.0.as_str()), *role);
                self.answer(op, placed)
            }
            HostOp::RaisePeer { win, peer } => self.raise_peer(op, *win, *peer),
            // An arriving module is built by its kind's own code: a refused prepare makes this window refuse the offer
            // before the other window has frozen anything.
            HostOp::PrepareSurface {
                win,
                module,
                shape,
                size,
            } => self.prepare_surface(op, *win, module, shape, *size),
            HostOp::AdoptMoved {
                win,
                module,
                kind,
                record,
                fds,
            } => self.adopt_moved(op, *win, module, *kind, record, fds),
            HostOp::OrderBaseTabs { tabs, active, .. } => self.service(op, |s| s.order_base_tabs(tabs, *active)),
            HostOp::EndPeer { win, peer } => self.linked(op, |linked| linked.end_peer(*win, *peer)),
            HostOp::Cleanup(call) => self.service(op, |s| s.cleanup(call)),
            HostOp::Pair(call) => self.service(op, |s| s.pair(call)),
            HostOp::SetMode { win, .. } => self.answer(op, self.windows.toggle_mode(*win)),
            HostOp::OpenModuleWindow {
                win,
                name,
                token,
                keep_keys,
            } => self.open_module_window(op, *win, name, token.as_deref(), *keep_keys),
            HostOp::ForwardRaise { peer, token, .. } => self.forward_raise(*peer, token.as_deref()),
            HostOp::PeerRun { win, peer, name, opts } => self.peer_run(op, *win, *peer, name, opts),
            HostOp::CompositorFocus { direction, .. } => self.compositor_focus(op, *direction),
            HostOp::HubAsk { win, ask } => self.hub_ask(op, *win, ask),
            // Only a window beside nvim carries these out, by the nvim it follows.
            HostOp::Beside(beside) => self.beside(op, beside),
            HostOp::BuildBasePage { .. } => {
                // Cloned out first: the page's build reaches the panel, and through it the host again.
                let maker = self.base_page_maker.borrow().clone();
                match maker {
                    Some(maker) => {
                        maker.build();
                        Applied::Done
                    }
                    None => self.refuse(op, NOT_CARRIED_OUT),
                }
            }
            // Window 1 hands a tab between module windows; the links carry the rest.
            HostOp::HandOverTab {
                win,
                from,
                agent,
                session,
                to,
                token,
            } => self.linked(op, |linked| {
                linked.hand_over(*win, *from, agent, session, to, token.clone())
            }),
            HostOp::KeepTab {
                win,
                peer,
                session,
                why,
            } => self.keep_tab(op, *win, *peer, session, why),
            HostOp::Quit => self.refuse(op, NOT_CARRIED_OUT),
        }
    }

    fn monitors(&self) -> Vec<Monitor> {
        self.windows.monitors()
    }
}

/// Every read answers at once from the window's services, grid and editor and the reads the shell gave, and calls
/// nothing that could reach the controller; a window that has given none answers as [`NoReads`] does.
impl<V: Views + ?Sized + 'static, W: WindowPort + ?Sized + 'static> HostReads for Router<V, W> {
    fn view_of_tab(&self, tab: eitri_core::tabs::TabId) -> Option<ViewId> {
        self.services().and_then(|services| services.view_of_tab(tab))
    }
    fn tab_moving(&self, tab: eitri_core::tabs::TabId) -> bool {
        self.services().is_none_or(|services| services.tab_moving(tab))
    }
    fn tab_has_work(&self, tab: eitri_core::tabs::TabId) -> bool {
        self.services().is_some_and(|services| services.tab_has_work(tab))
    }
    fn project_chooser_env(&self, win: WinId) -> ChooserEnv {
        let host = self.project_host.borrow().clone();
        host.map_or_else(|| NoReads.project_chooser_env(win), |host| host.env(win))
    }

    /// The name of the window manager this window's moves go through, which `companion.wm` picks.
    fn compositor_name(&self) -> &'static str {
        self.windows.compositor_name()
    }
    fn module_window_open(&self, name: &WindowName) -> bool {
        let linked = self.linked_windows.borrow().clone();
        linked.is_some_and(|linked| linked.module_window_open(name))
    }
    fn page_built(&self, _win: WinId, module: &ModuleId) -> bool {
        self.services().is_some_and(|services| services.page_built(module))
    }

    fn plugin_keys_help(&self, _win: WinId, module: &ModuleId) -> Option<eitri_core::plugin::keys::PluginKeysHelp> {
        self.services().and_then(|services| services.plugin_keys_help(module))
    }
    fn chat_views(&self) -> Vec<ViewId> {
        self.services().map_or_else(Vec::new, |services| services.chat_views())
    }
    fn can_break_out(&self, _win: WinId) -> Result<(), String> {
        self.services()
            .map_or_else(|| Err(NOT_CARRIED_OUT.into()), |services| services.can_break_out())
    }
    fn chat_move(
        &self,
        _win: WinId,
        view: ViewId,
        session: Option<&str>,
    ) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        self.services().map_or_else(
            || Err(NOT_CARRIED_OUT.into()),
            |services| services.chat_move(view, session),
        )
    }
    fn running_chats(&self, _win: WinId) -> Vec<(ModuleId, Option<eitri_core::tabs::TabId>)> {
        self.services().map_or_else(Vec::new, |s| s.running_chats())
    }
    fn terminal_job_names(&self, _win: WinId) -> Vec<String> {
        self.services()
            .map_or_else(Vec::new, |services| services.terminal_job_names())
    }
    fn terminal_job_modules(&self, _win: WinId) -> Vec<ModuleId> {
        self.services().map_or_else(Vec::new, |s| s.terminal_job_modules())
    }
    fn pages_with_tasks(&self, _win: WinId) -> Vec<ModuleId> {
        self.services().map_or_else(Vec::new, |s| s.pages_with_tasks())
    }
    fn base_tabs(&self, _win: WinId) -> Vec<eitri_core::tabs::TabId> {
        self.services().map_or_else(Vec::new, |s| s.base_tabs())
    }
    fn active_base_tab(&self, _win: WinId) -> Option<eitri_core::tabs::TabId> {
        self.services().and_then(|s| s.active_base_tab())
    }
    fn arrived_tab(&self, _win: WinId, mv: eitri_core::window_link::moves::MoveId) -> Option<eitri_core::tabs::TabId> {
        self.services().and_then(|s| s.arrived_tab(mv))
    }
    fn tab_move(&self, _win: WinId, tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        self.services()
            .map_or_else(|| Err(NOT_CARRIED_OUT.into()), |s| s.tab_move(tab))
    }
    fn tab_hand_over(&self, _win: WinId, tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::TabHandOver, String> {
        self.services()
            .map_or_else(|| Err(NOT_CARRIED_OUT.into()), |s| s.tab_hand_over(tab))
    }
    fn moved_tab_key(
        &self,
        _win: WinId,
        tab: eitri_core::tabs::TabId,
    ) -> Option<eitri_core::window_link::base_tabs::MovedTabKey> {
        self.services().and_then(|s| s.moved_tab_key(tab))
    }
    fn view_of_session(&self, _win: WinId, session: &str) -> Option<ViewId> {
        self.services().and_then(|services| services.view_of_session(session))
    }
    fn tab_can_step(&self, _win: WinId, delta: i32) -> bool {
        self.services().is_some_and(|services| services.tab_can_step(delta))
    }
    fn tab_has_last(&self, _win: WinId) -> bool {
        self.services().is_some_and(|services| services.tab_has_last())
    }
    fn tab_place(&self, _win: WinId, n: u16) -> Option<eitri_core::tab_set::TabPlace> {
        self.services().and_then(|services| services.tab_place(n))
    }

    fn grid_size(&self, win: WinId) -> Size {
        match self.views.grid(win) {
            Some(grid) => grid.size(),
            None => NoReads.grid_size(win),
        }
    }
    fn handle_px(&self, win: WinId) -> i32 {
        match self.views.grid(win) {
            Some(grid) => grid.handle_px(),
            None => NoReads.handle_px(win),
        }
    }
    fn strip_px(&self, win: WinId) -> i32 {
        match self.views.grid(win) {
            Some(grid) => grid.strip_px(),
            None => NoReads.strip_px(win),
        }
    }
    fn module_min(&self, win: WinId, module: &ModuleId) -> Size {
        match self.views.grid(win) {
            Some(grid) => grid.module_min(module),
            None => NoReads.module_min(win, module),
        }
    }
    fn editor_cell(&self, win: WinId) -> Option<(f64, f64)> {
        self.views.editor(win).and_then(|editor| editor.cell())
    }
    /// The module's host as the grid last allocated it; nothing before its first allocation.
    fn module_size(&self, win: WinId, module: &ModuleId) -> Option<Size> {
        self.views.grid(win)?.module_size(module)
    }
    fn editor_facts(&self, win: WinId) -> eitri_core::wm::EditorFacts {
        self.views
            .editor(win)
            .map_or_else(Default::default, |editor| editor.facts())
    }
    fn terminal_jobs(&self, _win: WinId) -> usize {
        self.services().map_or(0, |services| services.terminal_jobs())
    }
    fn editor_running(&self, win: WinId) -> bool {
        self.views.editor(win).is_some_and(|editor| editor.running())
    }
    /// The chat page's own answer, read from what the page last reported; none is claimed before the shell
    /// gives the read.
    fn chat_claims(&self, view: ViewId, direction: Direction) -> bool {
        let claims = self.chat_claims_fn.borrow().clone();
        match claims {
            Some(claims) => claims(view, direction),
            None => NoReads.chat_claims(view, direction),
        }
    }
    /// The panel's own count for the view; nothing held before the shell gives the read.
    fn view_attention(&self, view: ViewId) -> Attention {
        let attention = self.view_attention_fn.borrow().clone();
        match attention {
            Some(attention) => attention(view),
            None => NoReads.view_attention(view),
        }
    }
    /// The panel's own answer; none before the shell gives the read.
    fn oldest_card_view(&self, win: WinId) -> Option<ViewId> {
        let oldest = self.oldest_card_fn.borrow().clone();
        match oldest {
            Some(oldest) => oldest(win),
            None => NoReads.oldest_card_view(win),
        }
    }
    /// The panel's own counts for the view; nothing running before the shell gives the read.
    fn chat_summary(&self, win: WinId) -> eitri_core::wm::ChatSummary {
        let read = self.summary_fn.borrow().clone();
        read.map_or_else(Default::default, |read| read(win))
    }
    fn chat_kill_counts(&self, view: ViewId) -> ChatKillCounts {
        let counts = self.kill_counts_fn.borrow().clone();
        match counts {
            Some(counts) => counts(view),
            None => NoReads.chat_kill_counts(view),
        }
    }
    /// The panel's own answer; a view can be given its tab before the shell gives the read.
    fn chat_first_tab(&self, view: ViewId, breaks: bool) -> Result<(), String> {
        let first_tab = self.first_tab_fn.borrow().clone();
        match first_tab {
            Some(first_tab) => first_tab(view, breaks),
            None => NoReads.chat_first_tab(view, breaks),
        }
    }
}

/// The `Map` and `Unmap` ops of `ops` the grid carries out, as each module and whether it is to be shown:
/// a host the grid's probe places by hand is left where the probe put it, as the grid's own apply leaves it.
pub fn hosts_to_map(ops: &[HostOp], probe_places: impl Fn(&ModuleId) -> bool) -> Vec<(WinId, ModuleId, bool)> {
    ops.iter()
        .filter_map(|op| match op {
            HostOp::Map { win, module } => Some((*win, module.clone(), true)),
            HostOp::Unmap { win, module } => Some((*win, module.clone(), false)),
            _ => None,
        })
        .filter(|(_, module, _)| !probe_places(module))
        .collect()
}

#[cfg(test)]
#[path = "router/tests.rs"]
mod tests;
