//! The GTK shell's side of the window contract: `GtkHost` carries out the controller's ops on real
//! widgets and reports what GTK did through the attached sink; `scheduler::GlibScheduler` arms its
//! timers, idles and watches on GLib's default context.
//!
//! **Re-entrancy.** GTK emits signals while an op is still running: `grab_focus` notifies
//! `focus-widget` before it returns, `Unmap` of the surface holding the keys clears the window's
//! focus (GTK itself would leave the keys on the hidden widget) and so notifies it, and `close()`
//! emits `close-request` from inside itself. Every handler here only posts to the sink, which queues
//! what arrives while a batch applies, and no handler or op holds a borrow of the host while it calls
//! into GTK or the sink, so a signal fired from inside an op finds the host free.
//!
//! **Closing.** `CloseWindow` means "close without asking": the host marks the window as closing by
//! op before `close()`, and its own `close-request` handler then lets the close proceed without
//! asking the sink. Only a close the user or the compositor asked for reaches `close_requested`.
//! A `CloseWindow` applied while the sink is answering that window's own close request cannot call
//! `close()` (GTK ignores a close from inside its close-request emission); it is noted instead,
//! and the handler lets the close under way proceed whatever the sink answered.
//!
//! **Surfaces and services.** The host builds and drops module surfaces through a [`SurfaceMaker`]
//! and carries out the service ops the controller's module lifecycle emits (a terminal's start, a
//! plugin page's build, reload, visibility and stop, a chat view's chooser) through a
//! [`ServiceSet`]. Both are traits because what they act on (the grid, the terminal registry, the
//! plugin pages, the chat panel) belongs to the shell's binary, and this module is also compiled
//! alone by its conformance test, which hands it stubs. Until one is set, its ops are refused as
//! not carried out.
//!
//! **Starting a window.** `OpenWindow` builds the window (created, not shown) for the application
//! set with [`GtkHost::set_application`] and adopts it; `Present` shows it. `Lua` ops go to the
//! [`LuaRunner`] the shell sets (`init.lua` and the events it emits are the binary's).
//!
//! **Focus, keys and the prefix's chrome.** A module's `Focus` goes through the shell's own grab when
//! one is set ([`GtkHost::set_focus`]), since the shell knows what a bare grab does not: a page still
//! to be built, the editor's grab that also tells the input method; whether the module is on screen
//! is the controller's to know before it asks. Without one it is the bare grab. A top-bar item's
//! `Focus` goes through [`GtkHost::set_top_bar`], which brings a hidden bar out first, and the rename entry's
//! through [`GtkHost::set_rename_entry`], which does the same and shows the entry so it can take them. The
//! `select.number` digits and HINT's toggle go to the window's [`HintPort`]; a `Chrome` op reads the
//! window's model through [`GtkHost::set_chrome_model`] and redraws what changed on the widgets
//! [`GtkHost::set_chrome_widgets`] gives ([`chrome::reconcile`]), every flash and toast included. The cursor
//! signals, the literal keys and a chat page's claimed navigation key are service ops
//! ([`ServiceSet`]), and whether a chat page claims a key is the read [`GtkHost::set_chat_claims`]
//! gives.
//!
//! **The grid and the editor.** Each window's grid is reached through a [`GridPort`] and its editor
//! through [`EditorReads`] ([`GtkHost::set_grid`], [`GtkHost::set_editor`]): `Allocate` asks the grid
//! for a new allocation, `Restack` puts its float band in the op's order, and `Map`/`Unmap` leave
//! alone a host the grid's probe places by hand. The host also answers the controller's
//! [`HostReads`] from them, and with [`NoReads`]' values for a window that has given neither yet;
//! a chat view's attention is the read [`GtkHost::set_view_attention`] gives.
//!
//! **The pair.** `OpenLinkedWindow` starts or raises the project's second window through the [`LinkedWindows`] the
//! shell sets ([`GtkHost::set_linked_windows`]), with an activation token from the window's display; `RaisePeer` asks
//! that token and answers with `PeerEvent::Raised`, on which the controller asks the other window to come forward.
//! The controller's way onto the link itself is [`peer::LinkPort`], attached by the link's own code, not the host's.
//! An arriving module's `PrepareSurface` and `AdoptMoved` are refused: no kind's arrival is built here yet.
//!
//! **Lifetime.** The host holds its windows weakly. GTK drops its own reference when a window
//! closes, and only then does the window dispose and emit `destroy`, which is where the host
//! forgets the window and its surfaces; a strong reference here would keep every closed window.

// The conformance test compiles this module alone, without the binary's callers of some of it.
#![allow(dead_code)]

pub(crate) mod chrome;
pub(crate) mod instance_pool;
pub(crate) mod keys;
pub(crate) mod peer;
pub(crate) mod scheduler;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::{Rc, Weak};

use eitri_core::attention::Attention;
use eitri_core::layout::{Direction, ModuleId, Rect, Size};
use eitri_core::tabs::ViewId;
use eitri_core::wm::chrome::ChromeModel;
use eitri_core::wm::{
    ActivationToken, Applied, ChatKillCounts, ChatOp, CloseBy, CloseVerdict, DropWhy, EditorCall, FocusChain,
    FocusOwner, FocusTo, HintOp, Host, HostEvent, HostOp, HostReads, HostSink, LuaOp, Monitor, MonitorId, NoReads,
    OpTag, PeerEvent, PeerId, PluginOp, ShellCall, SurfaceKind, TermOp, WinId, WindowSpec,
};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// What a shell verb runs on: the window, the call (an action of a family the shell still carries out) and the
/// keycode that carried it (zero when no key did).
type ShellVerbArm = dyn Fn(WinId, &ShellCall, u32);

/// What gives a module the keys when the shell knows more than a bare grab: whether it took.
type FocusFn = dyn Fn(&ModuleId) -> bool;

/// What gives a top-bar item the keys: the first item showing from that index on, the bar brought out
/// first if it is hidden. Whether one took them.
type TopBarFn = dyn Fn(WinId, u32) -> bool;

/// What gives the top bar's rename entry the keys: the bar brought out first if it is hidden, the entry shown so it
/// can take them, its text left to the `Chrome` op that follows. Whether it took them.
type RenameEntryFn = dyn Fn(WinId) -> bool;

/// What reads a window's chrome model from the controller; nothing for a window it does not hold.
type ChromeModelFn = dyn Fn(WinId) -> Option<ChromeModel>;

/// What a chat view holds for the user, as its chip and its strip entry show it: read only, it never calls back.
type ViewAttentionFn = dyn Fn(ViewId) -> Attention;

/// Whether a chat page keeps a navigation key for itself: read only, it never calls back.
type ChatClaimsFn = dyn Fn(ViewId, Direction) -> bool;

/// The chat view holding the oldest card waiting for an answer: read only, it never calls back.
type OldestCardFn = dyn Fn(WinId) -> Option<ViewId>;

/// What a chat view's kill would end, and whether its tabs can close now: read only, it never calls back.
type KillCountsFn = dyn Fn(ViewId) -> ChatKillCounts;

/// Whether a new chat view can be given its first tab now (a break-out when the flag is set): read only, it never
/// calls back.
type FirstTabFn = dyn Fn(ViewId, bool) -> Result<(), String>;

/// The window's `select.number` digits and HINT's session, which stays the shell's.
pub(crate) trait HintPort {
    /// Draws each module's digit, the tenth module's as `0`.
    fn show_digits(&self, win: WinId, digits: &[(ModuleId, u8)]);
    /// Takes the digits down; nothing if none are up.
    fn hide_digits(&self, win: WinId);
    /// Starts or ends a HINT from the prefix; `keycode` is the key still held down that ran it.
    fn toggle(&self, win: WinId, keycode: u32);
}

/// The answer to every op this shell does not carry out through the host yet; its own code still
/// does those, so a controller that emits one learns it went nowhere.
pub(crate) const NOT_CARRIED_OUT: &str = "not carried out by this shell yet";

/// What starts the project's second window for `OpenLinkedWindow`: the pair's own code, which belongs to the shell's
/// binary. A host without it cannot start one.
pub(crate) trait LinkedWindows {
    /// Starts the second window, or raises it when it runs; `token` is the activation token this window's display
    /// gave, so the window that comes forward gets the keys. `Err` says why nothing was started.
    fn open(&self, win: WinId, token: Option<String>) -> Result<(), String>;
}

/// What builds a module's surface for `CreateSurface` and releases it for `DropSurface`.
pub(crate) trait SurfaceMaker {
    /// Builds `module`'s host for `kind` as the window's startup builds one, places it off screen,
    /// registers it with the host and wires its controllers. A module whose host already exists keeps
    /// it as it is: a host never gets a second set of controllers. `Err` says why nothing was built.
    fn create(&self, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Result<(), String>;
    /// Releases what `why` ends: a killed web module's web process, a retired chat view's page. The
    /// host widget stays in the window for the module's next instance.
    fn drop_surface(&self, win: WinId, module: &ModuleId, why: &DropWhy) -> Result<(), String>;
}

/// The window's services the controller's effect ops call. `Err` says why an op was not carried out.
pub(crate) trait ServiceSet {
    fn editor(&self, op: &EditorCall) -> Result<(), String>;
    fn terminal(&self, op: &TermOp) -> Result<(), String>;
    fn plugin(&self, op: &PluginOp) -> Result<(), String>;
    fn chat(&self, op: &ChatOp) -> Result<(), String>;
}

/// What the `Lua` ops run on: the window's Lua kernel. `Err` says why an op was not carried out.
pub(crate) trait LuaRunner {
    fn run(&self, op: &LuaOp) -> Result<(), String>;
}

/// A window's grid, as the host reads and drives it. A trait because the grid belongs to the shell's
/// binary, and this module is also compiled alone by its conformance test, which hands it a stub.
pub(crate) trait GridPort {
    /// The grid's allocation, or the stand-in content size before its first one.
    fn size(&self) -> Size;
    /// The divider thickness, as the grid's dividers measure.
    fn handle_px(&self) -> i32;
    /// A container strip's row height, as the grid's strips measure.
    fn strip_px(&self) -> i32;
    /// `module`'s minimum size, as its host measures.
    fn module_min(&self, module: &ModuleId) -> Size;
    /// Asks for one new measure and allocation of the grid.
    fn queue_allocation(&self);
    /// Puts the floats' hosts in `order`, bottom first, above everything tiled; nothing else moves.
    fn restack_floats(&self, order: &[ModuleId]);
    /// Whether the grid's probe places `module`'s host by hand, so no map or unmap may touch it.
    fn probe_places(&self, module: &ModuleId) -> bool;
    /// `module`'s host as the grid last allocated it; nothing for a host not laid out.
    fn module_size(&self, module: &ModuleId) -> Option<Size> {
        let _ = module;
        None
    }
}

/// What the host reads of a window's editor.
pub(crate) trait EditorReads {
    /// The editor's cell size in logical pixels, when it can tell.
    fn cell(&self) -> Option<(f64, f64)>;
    /// Whether the editor's child is running.
    fn running(&self) -> bool;
}

/// The `Map` and `Unmap` ops of `ops` the grid carries out, as each module and whether it is to be shown:
/// a host the grid's probe places by hand is left where the probe put it, as the grid's own apply leaves it.
pub(crate) fn hosts_to_map(ops: &[HostOp], probe_places: impl Fn(&ModuleId) -> bool) -> Vec<(WinId, ModuleId, bool)> {
    ops.iter()
        .filter_map(|op| match op {
            HostOp::Map { win, module } => Some((*win, module.clone(), true)),
            HostOp::Unmap { win, module } => Some((*win, module.clone(), false)),
            _ => None,
        })
        .filter(|(_, module, _)| !probe_places(module))
        .collect()
}

/// A window's chrome as the host draws it: its widgets, and the model last drawn on them.
struct ChromeDrawn {
    widgets: chrome::ChromeWidgets,
    last: RefCell<Option<ChromeModel>>,
}

pub(crate) struct GtkHost {
    me: Weak<GtkHost>,
    sink: RefCell<Option<Weak<dyn HostSink>>>,
    windows: RefCell<BTreeMap<WinId, glib::WeakRef<gtk4::Window>>>,
    surfaces: RefCell<BTreeMap<(WinId, ModuleId), gtk4::Widget>>,
    /// Windows being closed by `CloseWindow`: their `close-request` proceeds without asking.
    closing: RefCell<BTreeSet<WinId>>,
    /// Windows whose close request the sink is answering now, each with whether a `CloseWindow`
    /// for it arrived meanwhile (then the close under way proceeds).
    asking: RefCell<BTreeMap<WinId, bool>>,
    shell_verb_arm: RefCell<Option<Rc<ShellVerbArm>>>,
    surface_maker: RefCell<Option<Rc<dyn SurfaceMaker>>>,
    services: RefCell<Option<Rc<dyn ServiceSet>>>,
    lua: RefCell<Option<Rc<dyn LuaRunner>>>,
    /// The application `OpenWindow` builds windows for; held weakly, as GTK holds its windows.
    application: RefCell<Option<glib::WeakRef<gtk4::Application>>>,
    focus_fn: RefCell<Option<Rc<FocusFn>>>,
    top_bar_fn: RefCell<Option<Rc<TopBarFn>>>,
    rename_entry_fn: RefCell<Option<Rc<RenameEntryFn>>>,
    chrome_model: RefCell<Option<Rc<ChromeModelFn>>>,
    /// Each window's chrome widgets, with the model last drawn on them.
    chrome: RefCell<BTreeMap<WinId, Rc<ChromeDrawn>>>,
    chat_claims_fn: RefCell<Option<Rc<ChatClaimsFn>>>,
    view_attention_fn: RefCell<Option<Rc<ViewAttentionFn>>>,
    oldest_card_fn: RefCell<Option<Rc<OldestCardFn>>>,
    kill_counts_fn: RefCell<Option<Rc<KillCountsFn>>>,
    first_tab_fn: RefCell<Option<Rc<FirstTabFn>>>,
    hints: RefCell<Option<Rc<dyn HintPort>>>,
    linked_windows: RefCell<Option<Rc<dyn LinkedWindows>>>,
    grids: RefCell<BTreeMap<WinId, Rc<dyn GridPort>>>,
    editors: RefCell<BTreeMap<WinId, Rc<dyn EditorReads>>>,
    /// How many native focus reports have been posted, so a grab that GTK did not notify (the
    /// widget already held the keys) can still report where they are.
    focus_reports: Cell<u64>,
}

impl GtkHost {
    pub(crate) fn new() -> Rc<GtkHost> {
        Rc::new_cyclic(|me| GtkHost {
            me: me.clone(),
            sink: RefCell::new(None),
            windows: RefCell::new(BTreeMap::new()),
            surfaces: RefCell::new(BTreeMap::new()),
            closing: RefCell::new(BTreeSet::new()),
            asking: RefCell::new(BTreeMap::new()),
            shell_verb_arm: RefCell::new(None),
            surface_maker: RefCell::new(None),
            services: RefCell::new(None),
            lua: RefCell::new(None),
            application: RefCell::new(None),
            focus_fn: RefCell::new(None),
            top_bar_fn: RefCell::new(None),
            rename_entry_fn: RefCell::new(None),
            chrome_model: RefCell::new(None),
            chrome: RefCell::new(BTreeMap::new()),
            chat_claims_fn: RefCell::new(None),
            view_attention_fn: RefCell::new(None),
            oldest_card_fn: RefCell::new(None),
            kill_counts_fn: RefCell::new(None),
            first_tab_fn: RefCell::new(None),
            hints: RefCell::new(None),
            linked_windows: RefCell::new(None),
            grids: RefCell::new(BTreeMap::new()),
            editors: RefCell::new(BTreeMap::new()),
            focus_reports: Cell::new(0),
        })
    }

    /// Where the host reports. Held weakly: the sink (the driver) owns the host.
    pub(crate) fn attach(&self, sink: Weak<dyn HostSink>) {
        *self.sink.borrow_mut() = Some(sink);
    }

    fn sink(&self) -> Option<Rc<dyn HostSink>> {
        let sink = self.sink.borrow().clone();
        sink.and_then(|s| s.upgrade())
    }

    fn post(&self, event: HostEvent) {
        match self.sink() {
            Some(sink) => sink.post(event),
            None => eprintln!("[wm] the GTK host has no sink attached: {event:?} dropped"),
        }
    }

    /// Takes `window` as `win`: its close requests, focus changes and activation are reported from
    /// now on, and its entries are forgotten when it is destroyed.
    pub(crate) fn adopt_window(&self, win: WinId, window: &gtk4::Window) {
        self.windows.borrow_mut().insert(win, window.downgrade());
        let me = self.me.clone();
        window.connect_close_request(move |_| {
            let Some(host) = me.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if host.closing.borrow().contains(&win) {
                return glib::Propagation::Proceed;
            }
            // Without a sink there is no one to keep the window open, so GTK's own answer stands.
            let Some(sink) = host.sink() else {
                return glib::Propagation::Proceed;
            };
            host.asking.borrow_mut().insert(win, false);
            let verdict = sink.close_requested(win, CloseBy::User);
            let closed_by_op = host.asking.borrow_mut().remove(&win).unwrap_or(false);
            if closed_by_op {
                // A `CloseWindow` the sink applied while answering is carried out by this close.
                return glib::Propagation::Proceed;
            }
            match verdict {
                CloseVerdict::Stop => glib::Propagation::Stop,
                CloseVerdict::Proceed => glib::Propagation::Proceed,
            }
        });
        let me = self.me.clone();
        window.connect_focus_widget_notify(move |_| {
            if let Some(host) = me.upgrade() {
                host.report_focus(win);
            }
        });
        let me = self.me.clone();
        window.connect_is_active_notify(move |window| {
            if let Some(host) = me.upgrade() {
                host.post(HostEvent::Active {
                    win,
                    active: window.is_active(),
                });
            }
        });
        let me = self.me.clone();
        window.connect_destroy(move |_| {
            if let Some(host) = me.upgrade() {
                host.windows.borrow_mut().remove(&win);
                host.asking.borrow_mut().remove(&win);
                host.surfaces.borrow_mut().retain(|(w, _), _| *w != win);
                host.grids.borrow_mut().remove(&win);
                host.editors.borrow_mut().remove(&win);
                host.chrome.borrow_mut().remove(&win);
            }
        });
    }

    /// Names `widget` as `module`'s surface in `win`: the widget focus, map and unmap act on, and
    /// the one a focus report names when the keys are inside it.
    pub(crate) fn register_surface(&self, win: WinId, module: ModuleId, widget: &gtk4::Widget) {
        self.surfaces.borrow_mut().insert((win, module), widget.clone());
    }

    /// What `run_shell_verb` calls: the shell's own code for the actions it still carries out.
    pub(crate) fn set_shell_verb_arm(&self, arm: Box<ShellVerbArm>) {
        *self.shell_verb_arm.borrow_mut() = Some(Rc::from(arm));
    }

    /// What `CreateSurface` and `DropSurface` build and release through.
    pub(crate) fn set_surface_maker(&self, maker: Rc<dyn SurfaceMaker>) {
        *self.surface_maker.borrow_mut() = Some(maker);
    }

    /// What the service ops call. Set once, before the first batch that carries one.
    pub(crate) fn set_services(&self, services: Rc<dyn ServiceSet>) {
        *self.services.borrow_mut() = Some(services);
    }

    /// What the `Lua` ops run on. Set before the first batch that carries one.
    pub(crate) fn set_lua(&self, lua: Rc<dyn LuaRunner>) {
        *self.lua.borrow_mut() = Some(lua);
    }

    /// The application whose windows `OpenWindow` builds. Set before the first `OpenWindow`.
    pub(crate) fn set_application(&self, app: &gtk4::Application) {
        *self.application.borrow_mut() = Some(app.downgrade());
    }

    /// What `Focus` gives a module the keys through. Set before the first `Focus` on a window that
    /// holds modules; until then a `Focus` is the bare grab of the module's surface.
    pub(crate) fn set_focus(&self, focus: Rc<FocusFn>) {
        *self.focus_fn.borrow_mut() = Some(focus);
    }

    /// What a top-bar item's `Focus` goes through. Until it is set, such a `Focus` is not carried out.
    pub(crate) fn set_top_bar(&self, top_bar: Rc<TopBarFn>) {
        *self.top_bar_fn.borrow_mut() = Some(top_bar);
    }

    /// What the rename entry's `Focus` goes through. Until it is set, such a `Focus` is not carried out.
    pub(crate) fn set_rename_entry(&self, rename: Rc<RenameEntryFn>) {
        *self.rename_entry_fn.borrow_mut() = Some(rename);
    }

    /// Where a `Chrome` op reads the window's model. Until it is set, a `Chrome` is not carried out.
    pub(crate) fn set_chrome_model(&self, model: Rc<ChromeModelFn>) {
        *self.chrome_model.borrow_mut() = Some(model);
    }

    /// The widgets `win`'s chrome is drawn on, nothing drawn on them yet. Until they are set, a `Chrome` for `win`
    /// is not carried out.
    pub(crate) fn set_chrome_widgets(&self, win: WinId, widgets: chrome::ChromeWidgets) {
        let drawn = Rc::new(ChromeDrawn {
            widgets,
            last: RefCell::new(None),
        });
        self.chrome.borrow_mut().insert(win, drawn);
    }

    /// Draws what changed in `win`'s chrome model since it was last drawn, as a `Chrome` op does: the first draw
    /// once the widgets are set, and every announcement after it. `Err` says why nothing was drawn.
    pub(crate) fn draw_chrome(&self, win: WinId) -> Result<(), &'static str> {
        // Cloned out first: the widgets' setters move widgets, and GTK may call back into the host meanwhile.
        let model = self.chrome_model.borrow().clone();
        let drawn = self.chrome.borrow().get(&win).cloned();
        let (Some(model), Some(drawn)) = (model, drawn) else {
            return Err(NOT_CARRIED_OUT);
        };
        let Some(now) = model(win) else {
            return Err("the controller holds no such window");
        };
        // Taken while it draws, so a draw that comes back here meanwhile draws everything rather than finding it
        // borrowed.
        let mut last = drawn.last.take();
        chrome::reconcile(&drawn.widgets, &mut last, &now);
        *drawn.last.borrow_mut() = last;
        Ok(())
    }

    /// What a chat view holds for the user, as the controller reads it for the chips and the strips. Until it is
    /// set, every view holds nothing.
    pub(crate) fn set_view_attention(&self, attention: Rc<ViewAttentionFn>) {
        *self.view_attention_fn.borrow_mut() = Some(attention);
    }

    /// Whether a chat page claims a navigation key, as the controller reads it while it decides that key.
    /// Until it is set, no page claims one.
    pub(crate) fn set_chat_claims(&self, claims: Rc<ChatClaimsFn>) {
        *self.chat_claims_fn.borrow_mut() = Some(claims);
    }

    /// Which chat view holds the oldest waiting card, as the chat's key reads it. Until it is set, none does.
    pub(crate) fn set_oldest_card(&self, oldest: Rc<OldestCardFn>) {
        *self.oldest_card_fn.borrow_mut() = Some(oldest);
    }

    /// What a chat view's kill would end, as its y/n names it, and whether its tabs can close now. Until it is set,
    /// nothing runs in any view and every tab can close.
    pub(crate) fn set_chat_kill_counts(&self, counts: Rc<KillCountsFn>) {
        *self.kill_counts_fn.borrow_mut() = Some(counts);
    }

    /// Whether a new chat view can be given its first tab, asked before it is placed. Until it is set, it can.
    pub(crate) fn set_chat_first_tab(&self, first_tab: Rc<FirstTabFn>) {
        *self.first_tab_fn.borrow_mut() = Some(first_tab);
    }

    /// What the `Hint` ops draw and toggle through. Until it is set, they are not carried out.
    pub(crate) fn set_hints(&self, hints: Rc<dyn HintPort>) {
        *self.hints.borrow_mut() = Some(hints);
    }

    /// What `OpenLinkedWindow` starts the second window through. Until it is set, none can be started.
    pub(crate) fn set_linked_windows(&self, linked: Rc<dyn LinkedWindows>) {
        *self.linked_windows.borrow_mut() = Some(linked);
    }

    /// `win`'s grid: what `Allocate` and `Restack` act on and what the grid reads answer from.
    pub(crate) fn set_grid(&self, win: WinId, grid: Rc<dyn GridPort>) {
        self.grids.borrow_mut().insert(win, grid);
    }

    /// `win`'s editor: what the editor reads answer from.
    pub(crate) fn set_editor(&self, win: WinId, editor: Rc<dyn EditorReads>) {
        self.editors.borrow_mut().insert(win, editor);
    }

    fn grid(&self, win: WinId) -> Option<Rc<dyn GridPort>> {
        self.grids.borrow().get(&win).cloned()
    }

    fn editor(&self, win: WinId) -> Option<Rc<dyn EditorReads>> {
        self.editors.borrow().get(&win).cloned()
    }

    /// The window the host holds as `win`, while it exists.
    pub(crate) fn window(&self, win: WinId) -> Option<gtk4::Window> {
        self.windows.borrow().get(&win).and_then(|w| w.upgrade())
    }

    /// Whether the host still holds `win` or any surface of it: false once the window is gone.
    pub(crate) fn knows_window(&self, win: WinId) -> bool {
        self.windows.borrow().contains_key(&win) || self.surfaces.borrow().keys().any(|(w, _)| *w == win)
    }

    fn surface(&self, win: WinId, module: &ModuleId) -> Option<gtk4::Widget> {
        self.surfaces.borrow().get(&(win, module.clone())).cloned()
    }

    /// The registered surfaces of `win` around its focus widget, innermost first; empty for a window
    /// with no focus widget, or one in no module.
    fn focus_chain(&self, window: &gtk4::Window, win: WinId) -> Vec<ModuleId> {
        let surfaces: Vec<(ModuleId, gtk4::Widget)> = self
            .surfaces
            .borrow()
            .iter()
            .filter(|((w, _), _)| *w == win)
            .map(|((_, m), widget)| (m.clone(), widget.clone()))
            .collect();
        let mut chain = Vec::new();
        let mut at = GtkWindowExt::focus(window);
        while let Some(widget) = at {
            if let Some((module, _)) = surfaces.iter().find(|(_, s)| *s == widget) {
                chain.push(module.clone());
            }
            at = widget.parent();
        }
        chain
    }

    /// The module holding `win`'s keys right now, as GTK has them: where a key typed now would go. `None`
    /// for the top bar, the rename entry, no focus widget, or a window the host does not hold. For the arms
    /// still in the shell, which act on the module with the keys.
    pub(crate) fn focused_module(&self, win: WinId) -> Option<ModuleId> {
        let window = self.window(win)?;
        self.focus_chain(&window, win).into_iter().next()
    }

    /// Posts where `win`'s keys are now: the registered surfaces around the focus widget, innermost
    /// first, the innermost one owning the keys.
    fn report_focus(&self, win: WinId) {
        let Some(window) = self.window(win) else {
            return;
        };
        let chain = self.focus_chain(&window, win);
        self.focus_reports.set(self.focus_reports.get() + 1);
        self.post(HostEvent::NativeFocus {
            win,
            owner: chain.first().cloned().map(FocusOwner::Module),
            chain: FocusChain(chain),
            active: window.is_active(),
            op_index: None,
        });
    }

    fn refuse(&self, op: &HostOp, why: &str) -> Applied {
        self.post(HostEvent::Refused {
            what: OpTag::of(op),
            why: why.into(),
            surface: op.surface(),
        });
        Applied::Refused
    }

    fn focus(&self, op: &HostOp, win: WinId, to: &FocusTo) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        let module = match to {
            FocusTo::Module(module) => module,
            FocusTo::TopBarItem(item) => return self.focus_top_bar(op, win, *item),
            FocusTo::RenameEntry => return self.focus_rename_entry(op, win),
            FocusTo::Nothing => return self.refuse(op, NOT_CARRIED_OUT),
        };
        // Cloned out first: the function grabs, and GTK notifies the focus change from inside it.
        let focus_fn = self.focus_fn.borrow().clone();
        let reports = self.focus_reports.get();
        if let Some(focus_fn) = focus_fn {
            if !focus_fn(module) {
                return self.refuse(op, "the shell did not give the module the keys");
            }
        } else {
            let Some(widget) = self.surface(win, module) else {
                return self.refuse(op, "the module has no surface in this window");
            };
            if !widget.is_mapped() {
                return self.refuse(op, "the module's surface is not on screen");
            }
            if !widget.grab_focus() {
                return self.refuse(op, "GTK did not move the keys to the module's surface");
            }
        }
        // A grab on the widget that already holds the keys changes nothing GTK notifies; report
        // anyway, so every focus that took is reported while it applies, as on any host.
        if self.focus_reports.get() == reports {
            self.report_focus(win);
        }
        Applied::Done
    }

    fn focus_top_bar(&self, op: &HostOp, win: WinId, item: u32) -> Applied {
        // Cloned out first: the grab notifies the focus change from inside it.
        let top_bar = self.top_bar_fn.borrow().clone();
        let Some(top_bar) = top_bar else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if top_bar(win, item) {
            Applied::Done
        } else {
            self.refuse(op, "no top-bar item took the keys")
        }
    }

    fn focus_rename_entry(&self, op: &HostOp, win: WinId) -> Applied {
        // Cloned out first: the grab notifies the focus change from inside it.
        let rename = self.rename_entry_fn.borrow().clone();
        let Some(rename) = rename else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if rename(win) {
            Applied::Done
        } else {
            self.refuse(op, "the rename entry did not take the keys")
        }
    }

    fn chrome(&self, op: &HostOp, win: WinId) -> Applied {
        match self.draw_chrome(win) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, why),
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
        // Cloned out first: the probe's answer reads the grid, and nothing it calls should find the host borrowed.
        let grid = self.grid(win);
        let probe_places = |module: &ModuleId| grid.as_ref().is_some_and(|grid| grid.probe_places(module));
        match hosts_to_map(std::slice::from_ref(op), probe_places).pop() {
            Some((win, module, mapped)) => self.set_mapped(op, win, &module, mapped),
            None => Applied::Done,
        }
    }

    /// Runs `f` on `win`'s grid; refused for a window that has given none.
    fn on_grid(&self, op: &HostOp, win: WinId, f: impl FnOnce(&dyn GridPort)) -> Applied {
        // Cloned out first: the grid allocates and restacks, which may call back into the host.
        let Some(grid) = self.grid(win) else {
            return self.refuse(op, "the window has no grid");
        };
        f(grid.as_ref());
        Applied::Done
    }

    fn set_mapped(&self, op: &HostOp, win: WinId, module: &ModuleId, mapped: bool) -> Applied {
        let Some(widget) = self.surface(win, module) else {
            return self.refuse(op, "the module has no surface in this window");
        };
        if !mapped {
            self.release_keys_inside(win, &widget);
        }
        widget.set_child_visible(mapped);
        Applied::Done
    }

    /// Takes the keys from a surface about to be hidden. GTK leaves the window's focus widget on a
    /// widget made child-invisible, with no notify, so the keys would go on into a surface no one
    /// can see; clearing the focus first moves them, and the focus report says so while the op
    /// applies.
    fn release_keys_inside(&self, win: WinId, widget: &gtk4::Widget) {
        let Some(window) = self.window(win) else {
            return;
        };
        let holds_keys =
            GtkWindowExt::focus(&window).is_some_and(|focus| focus == *widget || focus.is_ancestor(widget));
        if holds_keys {
            GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>);
        }
    }

    fn create_surface(&self, op: &HostOp, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Applied {
        // Cloned out first: the maker adds widgets, and GTK may call back into the host meanwhile.
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if let Err(why) = maker.create(win, module, kind) {
            return self.refuse(op, &why);
        }
        if self.surface(win, module).is_none() {
            return self.refuse(op, "the surface was built but never registered");
        }
        Applied::Done
    }

    fn drop_surface(&self, op: &HostOp, win: WinId, module: &ModuleId, why: &DropWhy) -> Applied {
        if self.surface(win, module).is_none() {
            return self.refuse(op, "the module has no surface in this window");
        }
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match maker.drop_surface(win, module, why) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// Builds `win`'s window, created and not shown, and adopts it. Undecorated: the window's own top
    /// bar is its only titlebar.
    fn open_window(&self, op: &HostOp, win: WinId, spec: &WindowSpec) -> Applied {
        if self.window(win).is_some() {
            return self.refuse(op, "the window is already open");
        }
        let app = self.application.borrow().as_ref().and_then(|app| app.upgrade());
        let Some(app) = app else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        let window = gtk4::ApplicationWindow::builder()
            .application(&app)
            .title(spec.title.as_str())
            .default_width(1280)
            .default_height(760)
            .decorated(false)
            .build();
        self.adopt_window(win, window.upcast_ref());
        Applied::Done
    }

    /// Shows `win`, with the launcher's activation token when the controller has one; without one GTK
    /// uses the token it read from the environment at startup, if any.
    fn present(&self, op: &HostOp, win: WinId, activation: Option<&ActivationToken>) -> Applied {
        let Some(window) = self.window(win) else {
            return self.refuse(op, "no such window");
        };
        if let Some(token) = activation {
            window.set_startup_id(&token.0);
        }
        window.present();
        Applied::Done
    }

    /// An activation token from `win`'s display, naming the surface with the keys and the seat's last input, for the
    /// other window of the pair to come forward with. `None` when the compositor gives none.
    fn activation_token(&self, win: WinId) -> Option<String> {
        let window = self.window(win)?;
        let context = WidgetExt::display(&window).app_launch_context();
        context
            .startup_notify_id(None::<&gtk4::gio::AppInfo>, &[])
            .map(|token| token.to_string())
            .filter(|token| !token.is_empty())
    }

    fn open_linked_window(&self, op: &HostOp, win: WinId) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        // Cloned out first: the start wires the new link and may call back into GTK.
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        match linked.open(win, self.activation_token(win)) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// The other window comes forward by its own raise once it hears `focus{window}` with this token: under GNOME
    /// the token lets it take the keys from this window, and under sway, Hyprland and niri, which turn a token into
    /// an urgency hint, it also has the compositor focus it by its pid. The controller sends that message on the
    /// answer posted here.
    fn raise_peer(&self, op: &HostOp, win: WinId, peer: PeerId) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        let token = self.activation_token(win);
        self.post(HostEvent::Peer(PeerEvent::Raised { peer, token }));
        Applied::Done
    }

    fn lua(&self, op: &HostOp, call: &LuaOp) -> Applied {
        // Cloned out first: the kernel runs Lua, which may post to the sink or call into GTK.
        let lua = self.lua.borrow().clone();
        let Some(lua) = lua else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match lua.run(call) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    fn service(&self, op: &HostOp, call: impl FnOnce(&dyn ServiceSet) -> Result<(), String>) -> Applied {
        // Cloned out first: a service may post to the sink or call into GTK.
        let services = self.services.borrow().clone();
        let Some(services) = services else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match call(services.as_ref()) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    fn close_window(&self, op: &HostOp, win: WinId) -> Applied {
        let Some(window) = self.window(win) else {
            return self.refuse(op, "no such window");
        };
        if let Some(noted) = self.asking.borrow_mut().get_mut(&win) {
            // Inside this window's own close request GTK ignores `close()`; the request's handler
            // lets the close under way proceed once the sink has answered.
            *noted = true;
            return Applied::Done;
        }
        if !window.is_realized() {
            // GTK ignores `close()` on a window that was never realized; nothing can have been
            // shown in it, so it is destroyed without a close request either way.
            window.destroy();
            return Applied::Done;
        }
        self.closing.borrow_mut().insert(win);
        window.close();
        self.closing.borrow_mut().remove(&win);
        Applied::Done
    }
}

impl Host for GtkHost {
    fn apply(&self, op: &HostOp) -> Applied {
        // Every variant is named, so a new op is a compile error here until this shell decides.
        match op {
            HostOp::Focus { win, to, on_grab: _ } => self.focus(op, *win, to),
            HostOp::Map { win, .. } | HostOp::Unmap { win, .. } => self.map_or_unmap(op, *win),
            HostOp::Allocate { win } => self.on_grid(op, *win, |grid| grid.queue_allocation()),
            // The floats only, bottom first: each move places its float after the one below it.
            HostOp::Restack { win, moves } => {
                let order: Vec<ModuleId> = moves.iter().map(|step| step.module.clone()).collect();
                self.on_grid(op, *win, |grid| grid.restack_floats(&order))
            }
            HostOp::CloseWindow { win } => self.close_window(op, *win),
            HostOp::CreateSurface { win, module, kind } => self.create_surface(op, *win, module, kind),
            HostOp::DropSurface { win, module, why } => self.drop_surface(op, *win, module, why),
            HostOp::Chrome { win, .. } => self.chrome(op, *win),
            HostOp::Hint(call) => self.hint(op, call),
            HostOp::Editor(call) => self.service(op, |s| s.editor(call)),
            HostOp::Terminal(call) => self.service(op, |s| s.terminal(call)),
            HostOp::Plugin(call) => self.service(op, |s| s.plugin(call)),
            HostOp::Chat(call) => self.service(op, |s| s.chat(call)),
            HostOp::OpenWindow { win, spec } => self.open_window(op, *win, spec),
            HostOp::Present { win, activation } => self.present(op, *win, activation.as_ref()),
            HostOp::Lua(call) => self.lua(op, call),
            HostOp::OpenLinkedWindow { win } => self.open_linked_window(op, *win),
            HostOp::RaisePeer { win, peer } => self.raise_peer(op, *win, *peer),
            // An arriving module is built by its kind's own code, which no kind has here yet: a refused prepare makes
            // this window refuse the offer before the other window has frozen anything.
            HostOp::PrepareSurface { .. } | HostOp::AdoptMoved { .. } => self.refuse(op, NOT_CARRIED_OUT),
            HostOp::SetMode { .. } | HostOp::MoveToMonitor { .. } | HostOp::Quit | HostOp::Cleanup(..) => {
                self.refuse(op, NOT_CARRIED_OUT)
            }
        }
    }

    fn run_shell_verb(&self, win: WinId, call: &ShellCall, keycode: u32) {
        // Cloned out first: the arm may set a new arm, or apply ops, from inside.
        let arm = self.shell_verb_arm.borrow().clone();
        match arm {
            Some(arm) => arm(win, call, keycode),
            None => eprintln!("[wm] no shell verb arm is set: {call:?} in {win:?} dropped"),
        }
    }

    fn monitors(&self) -> Vec<Monitor> {
        let Some(display) = gdk::Display::default() else {
            return Vec::new();
        };
        let list = display.monitors();
        (0..list.n_items())
            .filter_map(|i| Some((i, list.item(i)?.downcast::<gdk::Monitor>().ok()?)))
            .map(|(i, monitor)| {
                let geometry = monitor.geometry();
                let id = monitor
                    .connector()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| format!("monitor-{i}"));
                Monitor {
                    name: monitor
                        .description()
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| id.clone()),
                    id: MonitorId(id),
                    rect: Rect {
                        x: geometry.x(),
                        y: geometry.y(),
                        w: geometry.width(),
                        h: geometry.height(),
                    },
                    scale: monitor.scale(),
                }
            })
            .collect()
    }
}

/// Every read answers at once from the window's grid and editor, and calls nothing that could reach the
/// controller; a window that has given neither answers as [`NoReads`] does.
impl HostReads for GtkHost {
    fn grid_size(&self, win: WinId) -> Size {
        match self.grid(win) {
            Some(grid) => grid.size(),
            None => NoReads.grid_size(win),
        }
    }
    fn handle_px(&self, win: WinId) -> i32 {
        match self.grid(win) {
            Some(grid) => grid.handle_px(),
            None => NoReads.handle_px(win),
        }
    }
    fn strip_px(&self, win: WinId) -> i32 {
        match self.grid(win) {
            Some(grid) => grid.strip_px(),
            None => NoReads.strip_px(win),
        }
    }
    fn module_min(&self, win: WinId, module: &ModuleId) -> Size {
        match self.grid(win) {
            Some(grid) => grid.module_min(module),
            None => NoReads.module_min(win, module),
        }
    }
    fn editor_cell(&self, win: WinId) -> Option<(f64, f64)> {
        self.editor(win).and_then(|editor| editor.cell())
    }
    /// The module's host as the grid last allocated it; nothing before its first allocation.
    fn module_size(&self, win: WinId, module: &ModuleId) -> Option<Size> {
        self.grid(win)?.module_size(module)
    }
    fn editor_running(&self, win: WinId) -> bool {
        self.editor(win).is_some_and(|editor| editor.running())
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

// Only the probe places hosts by hand, so only a build with it has a host to leave alone.
#[cfg(all(test, feature = "probe"))]
mod tests {
    use super::*;

    /// The probe's hosts stay where it put them: a map or an unmap of one reaches nothing, every other host's
    /// op is carried out as it came, and no other op is a map.
    #[test]
    fn map_and_unmap_skip_a_host_the_probe_places() {
        let placed = ModuleId::terminal();
        let laid_out = ModuleId::agent();
        let win = WinId::MAIN;
        let ops = vec![
            HostOp::Map {
                win,
                module: placed.clone(),
            },
            HostOp::Map {
                win,
                module: laid_out.clone(),
            },
            HostOp::Allocate { win },
            HostOp::Unmap {
                win,
                module: placed.clone(),
            },
            HostOp::Unmap {
                win,
                module: laid_out.clone(),
            },
        ];
        assert_eq!(
            hosts_to_map(&ops, |module| *module == placed),
            vec![(win, laid_out.clone(), true), (win, laid_out.clone(), false)]
        );
        assert_eq!(
            hosts_to_map(&ops, |_| false),
            vec![
                (win, placed.clone(), true),
                (win, laid_out.clone(), true),
                (win, placed, false),
                (win, laid_out, false),
            ],
            "without the probe every map and unmap is carried out"
        );
    }
}
