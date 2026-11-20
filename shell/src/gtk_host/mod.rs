//! The GTK shell's side of the window contract. The op router ([`eitri_desk::router`], the type [`WmHost`]) matches
//! every op the window controller emits and carries it out through the collaborators the shell gives it; `GtkHost` is
//! the GTK back end it does that through, its [`Views`] (a module's surface shown, hidden and given the keys, the
//! chrome drawn, the grid and the editor read) and its [`WindowPort`] (a window opened, shown, closed, put in a mode,
//! placed), reporting what GTK did through the attached sink. `scheduler::GlibScheduler` arms the controller's timers,
//! idles and watches on GLib's default context.
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
//! asking the sink. Only a close the user or the compositor asked for reaches `close_requested`
//! (GTK does not tell the two apart, so both arrive as the user's), and the handler decides nothing
//! else: the controller answers `Stop` and, once the close may go, tears the window down in a batch of
//! its own -- the layout saved, the pair told, nvim ended, the terminals hung up, the directories and
//! feeds removed, the plugins and chats ended -- whose last op is that `CloseWindow`. Each teardown step
//! is a service op ([`ServiceSet`], [`ServiceSet::cleanup`]), carried out in the batch's order.
//! A `CloseWindow` applied while the sink is answering that window's own close request cannot call
//! `close()` (GTK ignores a close from inside its close-request emission); it is noted instead,
//! and the handler lets the close under way proceed whatever the sink answered.
//!
//! **Surfaces and services.** The router builds and drops module surfaces through a [`SurfaceMaker`] and carries out
//! the service ops through a [`ServiceSet`]. Both are `eitri-desk` traits because every host gives its own (here what
//! they act on, the grid, the terminal registry, the plugin pages, the chat panel, is the shell binary's), and this
//! module is also compiled alone by its conformance test, which hands it stubs. A surface the maker builds is
//! registered here ([`GtkHost::register_surface`]): the widget focus, map and unmap act on.
//!
//! **Starting a window.** `OpenWindow` builds the window (created, not shown) for the application
//! set with [`GtkHost::set_application`] and adopts it; `Present` shows it, through the compositor-aware
//! [`PairRaise`] when the shell sets one.
//!
//! **The window's mode.** `SetMode` toggles the window's mode through the function the shell sets
//! ([`GtkHost::set_mode_toggle`]), as `prefix F11` does, rather than setting the state it names: the shell owns the
//! window's real mode, which a bare F11, nvim's `g:neovide_fullscreen` and the compositor change too.
//!
//! **Focus, keys and the prefix's chrome.** A module's `Focus` goes through the shell's own grab when
//! one is set ([`GtkHost::set_focus`]), since the shell knows what a bare grab does not: a page still
//! to be built, the editor's grab that also tells the input method; whether the module is on screen
//! is the controller's to know before it asks. Without one it is the bare grab. A top-bar item's
//! `Focus` goes through [`GtkHost::set_top_bar`], which brings a hidden bar out first, and the rename entry's
//! through [`GtkHost::set_rename_entry`], which does the same and shows the entry so it can take them. A
//! `Chrome` op reads the window's model through [`GtkHost::set_chrome_model`] and redraws what changed on the
//! widgets [`GtkHost::set_chrome_widgets`] gives ([`chrome::reconcile`]), every flash and toast included.
//!
//! **The grid and the editor.** Each window's grid is reached through a [`GridPort`] and its editor
//! through [`EditorReads`] ([`GtkHost::set_grid`], [`GtkHost::set_editor`]): `Allocate` asks the grid
//! for a new allocation, `Restack` puts its float band in the op's order, and `Map`/`Unmap` leave
//! alone a host the grid's probe places by hand. The router answers the controller's reads from them.
//!
//! **Placing and the compositor.** `PlaceWindow` applies this process's placement role, optionally moving to an
//! output before focusing ([`monitor_move`]), through the [`MonitorPlacer`] the shell sets, never on the GTK thread;
//! `CompositorFocus` is the compositor's focus move past the window's edge, through [`PairRaise::move_focus`].
//!
//! **Lifetime.** The host holds its windows weakly. GTK drops its own reference when a window
//! closes, and only then does the window dispose and emit `destroy`, which is where the host
//! forgets the window and its surfaces; a strong reference here would keep every closed window.

// The conformance test compiles this module alone, without the binary's callers of some of it.
#![allow(dead_code)]

pub(crate) mod chrome;
pub(crate) mod keys;
pub(crate) mod onboarding;
pub(crate) mod prefix_panel;
pub(crate) mod project_chooser;
pub(crate) mod scheduler;

pub(crate) use eitri_desk::host_traits::{
    respawn_pane, BasePageMaker, BesidePort, EditorReads, GridPort, HintPort, LuaRunner, ProjectHost, RespawnPane,
    RestartRefused, ServiceSet, SurfaceMaker, NOT_CARRIED_OUT,
};
pub(crate) use eitri_desk::linked::peer;
pub(crate) use eitri_desk::router::{Router, Views, WindowPort};
pub(crate) use eitri_desk::{instance_pool, prefix_fit, source_book, wire_plan};

/// The window's op router over the GTK host, which is both its views and its window port.
pub(crate) type WmHost = Router<GtkHost, GtkHost>;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::{Rc, Weak};

use eitri_core::layout::{Direction, ModuleId, Rect};
use eitri_core::wm::chrome::ChromeModel;
use eitri_core::wm::{
    ActivationToken, CloseBy, CloseVerdict, FocusChain, FocusOwner, HostEvent, HostSink, Monitor, MonitorId, WinId,
    WindowSpec,
};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// What toggles a window's mode the way `prefix F11` does: the window's real mode is the shell's, which also hears
/// a bare F11, the editor's own setting and the compositor.
type ModeToggleFn = dyn Fn(WinId);

/// What gives a module the keys when the shell knows more than a bare grab: whether it took.
type FocusFn = dyn Fn(&ModuleId) -> bool;

/// What gives a top-bar item the keys: the first item showing from that index on, the bar brought out
/// first if it is hidden. Whether one took them.
type TopBarFn = dyn Fn(WinId, u32) -> bool;

/// What gives the top bar's rename entry the keys: the bar brought out first if it is hidden, the entry shown so it
/// can take them, its text left to the `Chrome` op that follows. Whether it took them.
type RenameEntryFn = dyn Fn(WinId) -> bool;

/// What gives the first-start screen the keys: the screen shown first if it is hidden, since the `Focus` op comes
/// before the `Chrome` op that fills it. Whether it took them.
type OnboardingFocusFn = dyn Fn(WinId) -> bool;

/// What reads a window's chrome model from the controller; nothing for a window it does not hold.
type ChromeModelFn = dyn Fn(WinId) -> Option<ChromeModel>;

/// The compositor-aware presentation of a window of a pair or one that takes files from outside, supplied by the binary
/// so the host can also run alone.
pub(crate) trait PairRaise {
    fn raise(&self, window: &gtk4::Window, token: Option<&str>);
    /// Has the compositor move the keys one window `direction`; `false` with no adapter to do it, which `on_none` has
    /// then said once. Never waits: the adapter's command runs on its worker.
    fn move_focus(&self, direction: Direction, on_none: &dyn Fn(&str)) -> bool;
}

/// Why `PlaceWindow` is refused where no window manager adapter can move a window.
pub(crate) const NO_OUTPUT_MOVE: &str = "this window manager cannot move a window to an output";

/// How this process's own window is moved to an output, per window manager. Each is carried out off the GTK thread:
/// a command runs on the adapter's worker, GNOME's call is asynchronous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MonitorMove {
    /// sway names the window by this process's pid: the command runs as it is.
    Command(Vec<String>),
    /// Hyprland names the window by this process's pid too, but its command is written in the form its config
    /// manager takes, which only the worker learns: it builds the command with
    /// `eitri_core::compositor::placement_argv`.
    Hyprland {
        pid: u32,
        connector: Option<String>,
        role: eitri_core::wm::PlacementRole,
    },
    /// No output move or compositor focus is needed.
    Unchanged,
    /// niri names a window by its id: the worker reads the window list for the first window of `pid`, then moves
    /// it with `eitri_core::compositor::move_to_output_argv`.
    ById { pid: u32, connector: String },
    /// GNOME: the Eitri extension moves the windows of the process that asks.
    Gnome { connector: String },
}

/// The move of the window of process `pid` to the output `connector` under `wm`; `Err` is why there is none.
pub(crate) fn monitor_move(
    wm: eitri_core::compositor::Wm,
    pid: u32,
    connector: Option<&str>,
    role: eitri_core::wm::PlacementRole,
) -> Result<MonitorMove, String> {
    use eitri_core::compositor::{placement_argv, valid_output_name, HyprForm, WindowRef, Wm};
    if connector.is_some_and(|connector| !valid_output_name(connector)) {
        return Err(format!("{:?} is not an output name", connector.unwrap_or_default()));
    }
    let own = WindowRef {
        pid,
        id: None,
        focused: false,
        visible: true,
        rect: None,
        ..Default::default()
    };
    if connector.is_none() && !matches!(wm, Wm::Sway | Wm::Hyprland) {
        return Ok(MonitorMove::Unchanged);
    }
    match wm {
        // sway has one form. For Hyprland the legacy build only shows that there is a command; the worker writes
        // the one it runs.
        Wm::Sway => placement_argv(wm, HyprForm::Legacy, &own, connector, role)
            .map(MonitorMove::Command)
            .ok_or_else(|| NO_OUTPUT_MOVE.to_string()),
        Wm::Hyprland => placement_argv(wm, HyprForm::Legacy, &own, connector, role)
            .map(|_| MonitorMove::Hyprland {
                pid,
                connector: connector.map(str::to_string),
                role,
            })
            .ok_or_else(|| NO_OUTPUT_MOVE.to_string()),
        Wm::Niri => Ok(MonitorMove::ById {
            pid,
            connector: connector.unwrap().to_string(),
        }),
        Wm::Gnome => Ok(MonitorMove::Gnome {
            connector: connector.unwrap().to_string(),
        }),
        Wm::None => Err(NO_OUTPUT_MOVE.to_string()),
    }
}

/// What carries out a [`MonitorMove`]: the window manager adapter the shell's binary has, which runs commands on its
/// worker thread and calls GNOME's extension.
pub(crate) trait MonitorPlacer {
    /// The window manager the moves are for.
    fn wm(&self) -> eitri_core::compositor::Wm;
    /// Starts `how`; never waits.
    fn place(&self, how: MonitorMove);
}

/// A window's chrome as the host draws it: its widgets, and the model last drawn on them.
struct ChromeDrawn {
    widgets: chrome::ChromeWidgets,
    last: RefCell<Option<ChromeModel>>,
}

/// The GTK host's state: each window it holds, the surfaces registered in them, and what the shell gives it to show
/// and focus them with. The router ([`WmHost`]) carries ops out through it as its [`Views`] and [`WindowPort`].
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
    mode_toggle: RefCell<Option<Rc<ModeToggleFn>>>,
    /// The application `OpenWindow` builds windows for; held weakly, as GTK holds its windows.
    application: RefCell<Option<glib::WeakRef<gtk4::Application>>>,
    focus_fn: RefCell<Option<Rc<FocusFn>>>,
    top_bar_fn: RefCell<Option<Rc<TopBarFn>>>,
    rename_entry_fn: RefCell<Option<Rc<RenameEntryFn>>>,
    onboarding_focus_fn: RefCell<Option<Rc<OnboardingFocusFn>>>,
    chrome_model: RefCell<Option<Rc<ChromeModelFn>>>,
    /// Each window's chrome widgets, with the model last drawn on them.
    chrome: RefCell<BTreeMap<WinId, Rc<ChromeDrawn>>>,
    pair_raise: RefCell<Option<Rc<dyn PairRaise>>>,
    placer: RefCell<Option<Rc<dyn MonitorPlacer>>>,
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
            mode_toggle: RefCell::new(None),
            application: RefCell::new(None),
            focus_fn: RefCell::new(None),
            top_bar_fn: RefCell::new(None),
            rename_entry_fn: RefCell::new(None),
            onboarding_focus_fn: RefCell::new(None),
            chrome_model: RefCell::new(None),
            chrome: RefCell::new(BTreeMap::new()),
            pair_raise: RefCell::new(None),
            placer: RefCell::new(None),
            grids: RefCell::new(BTreeMap::new()),
            editors: RefCell::new(BTreeMap::new()),
            focus_reports: Cell::new(0),
        })
    }

    /// The router over this host, as the window's driver applies ops to it and reads it. Each call makes a new router
    /// with no collaborators set: a window makes one, at its start.
    pub(crate) fn router(self: &Rc<GtkHost>) -> Rc<WmHost> {
        Router::new(self.clone(), self.clone())
    }

    /// Where the host reports. Held weakly: the sink (the driver) owns the router that owns the host.
    fn attach_sink(&self, sink: Weak<dyn HostSink>) {
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

    /// What `SetMode` toggles the window's mode through. Until it is set, a `SetMode` is not carried out.
    pub(crate) fn set_mode_toggle(&self, toggle: Rc<ModeToggleFn>) {
        *self.mode_toggle.borrow_mut() = Some(toggle);
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

    /// What the first-start screen's `Focus` goes through. Until it is set, such a `Focus` is not carried out.
    pub(crate) fn set_onboarding_focus(&self, focus: Rc<OnboardingFocusFn>) {
        *self.onboarding_focus_fn.borrow_mut() = Some(focus);
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

    /// Presents a paired window through its compositor adapter, including its first show.
    pub(crate) fn set_pair_raise(&self, raise: Rc<dyn PairRaise>) {
        *self.pair_raise.borrow_mut() = Some(raise);
    }

    /// What `PlaceWindow` moves this window through. Until it is set, every move is refused.
    pub(crate) fn set_monitor_placer(&self, placer: Rc<dyn MonitorPlacer>) {
        *self.placer.borrow_mut() = Some(placer);
    }

    /// `win`'s grid: what `Allocate` and `Restack` act on and what the grid reads answer from.
    pub(crate) fn set_grid(&self, win: WinId, grid: Rc<dyn GridPort>) {
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            grid.set_pointer_sink(win, sink);
        }
        self.grids.borrow_mut().insert(win, grid);
    }

    /// `win`'s editor: what the editor reads answer from.
    pub(crate) fn set_editor(&self, win: WinId, editor: Rc<dyn EditorReads>) {
        self.editors.borrow_mut().insert(win, editor);
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
}

/// What one of the shell's grabs of a chrome part answered (`None`: the shell gave no grab), as the views' result.
fn chrome_grab(took: Option<bool>, refused: &str) -> Result<(), String> {
    match took {
        None => Err(NOT_CARRIED_OUT.into()),
        Some(true) => Ok(()),
        Some(false) => Err(refused.into()),
    }
}

impl Views for GtkHost {
    fn attach(&self, sink: Weak<dyn HostSink>) {
        self.attach_sink(sink.clone());
        let grids: Vec<_> = self
            .grids
            .borrow()
            .iter()
            .map(|(win, grid)| (*win, grid.clone()))
            .collect();
        for (win, grid) in grids {
            grid.set_pointer_sink(win, sink.clone());
        }
    }

    fn has_surface(&self, win: WinId, module: &ModuleId) -> bool {
        self.surface(win, module).is_some()
    }

    fn grid(&self, win: WinId) -> Option<Rc<dyn GridPort>> {
        self.grids.borrow().get(&win).cloned()
    }

    fn editor(&self, win: WinId) -> Option<Rc<dyn EditorReads>> {
        self.editors.borrow().get(&win).cloned()
    }

    fn focus_module(&self, win: WinId, module: &ModuleId) -> Result<(), String> {
        // Cloned out first: the function grabs, and GTK notifies the focus change from inside it.
        let focus_fn = self.focus_fn.borrow().clone();
        let reports = self.focus_reports.get();
        if let Some(focus_fn) = focus_fn {
            if !focus_fn(module) {
                return Err("the shell did not give the module the keys".into());
            }
        } else {
            let Some(widget) = self.surface(win, module) else {
                return Err("the module has no surface in this window".into());
            };
            if !widget.is_mapped() {
                return Err("the module's surface is not on screen".into());
            }
            if !widget.grab_focus() {
                return Err("GTK did not move the keys to the module's surface".into());
            }
        }
        // A grab on the widget that already holds the keys changes nothing GTK notifies; report
        // anyway, so every focus that took is reported while it applies, as on any host.
        if self.focus_reports.get() == reports {
            self.report_focus(win);
        }
        Ok(())
    }

    fn focus_top_bar(&self, win: WinId, item: u32) -> Result<(), String> {
        // Cloned out first: the grab notifies the focus change from inside it.
        let top_bar = self.top_bar_fn.borrow().clone();
        chrome_grab(
            top_bar.map(|top_bar| top_bar(win, item)),
            "no top-bar item took the keys",
        )
    }

    fn focus_rename_entry(&self, win: WinId) -> Result<(), String> {
        // Cloned out first: the grab notifies the focus change from inside it.
        let rename = self.rename_entry_fn.borrow().clone();
        chrome_grab(
            rename.map(|rename| rename(win)),
            "the rename entry did not take the keys",
        )
    }

    fn focus_onboarding(&self, win: WinId) -> Result<(), String> {
        // Cloned out first: the grab notifies the focus change from inside it.
        let focus = self.onboarding_focus_fn.borrow().clone();
        chrome_grab(
            focus.map(|focus| focus(win)),
            "the first-start screen did not take the keys",
        )
    }

    fn grab_reported(&self, win: WinId, grab: &dyn Fn() -> bool) -> bool {
        let reports = self.focus_reports.get();
        if !grab() {
            return false;
        }
        if self.focus_reports.get() == reports {
            self.report_focus(win);
        }
        true
    }

    /// GTK leaves the window's focus widget on a widget made child-invisible, with no notify, so the keys would go
    /// on into a surface no one can see; clearing the focus first moves them, and the focus report says so while
    /// the op applies.
    fn release_keys(&self, win: WinId, module: &ModuleId) {
        let (Some(window), Some(widget)) = (self.window(win), self.surface(win, module)) else {
            return;
        };
        let holds_keys =
            GtkWindowExt::focus(&window).is_some_and(|focus| focus == widget || focus.is_ancestor(&widget));
        if holds_keys {
            GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>);
        }
    }

    fn show(&self, win: WinId, module: &ModuleId, shown: bool) {
        if let Some(widget) = self.surface(win, module) {
            widget.set_child_visible(shown);
        }
    }

    fn chrome(&self, win: WinId) -> Result<(), String> {
        self.draw_chrome(win).map_err(str::to_string)
    }
}

impl WindowPort for GtkHost {
    fn attach(&self, sink: Weak<dyn HostSink>) {
        self.attach_sink(sink);
    }

    fn is_open(&self, win: WinId) -> bool {
        self.window(win).is_some()
    }

    /// Builds `win`'s window, created and not shown, and adopts it. Undecorated: the window's own top
    /// bar is its only titlebar.
    fn open(&self, win: WinId, spec: &WindowSpec) -> Result<(), String> {
        if self.window(win).is_some() {
            return Err("the window is already open".into());
        }
        let app = self.application.borrow().as_ref().and_then(|app| app.upgrade());
        let Some(app) = app else {
            return Err(NOT_CARRIED_OUT.into());
        };
        let window = gtk4::ApplicationWindow::builder()
            .application(&app)
            .title(spec.title.as_str())
            .default_width(1280)
            .default_height(760)
            .decorated(false)
            .build();
        self.adopt_window(win, window.upcast_ref());
        Ok(())
    }

    /// Shows `win`, with the launcher's activation token when the controller has one; without one GTK
    /// uses the token it read from the environment at startup, if any.
    fn present(&self, win: WinId, activation: Option<&ActivationToken>) -> Result<(), String> {
        let Some(window) = self.window(win) else {
            return Err("no such window".into());
        };
        let raise = self.pair_raise.borrow().clone();
        if let Some(raise) = raise {
            raise.raise(&window, activation.map(|token| token.0.as_str()));
        } else {
            if let Some(token) = activation {
                window.set_startup_id(&token.0);
            }
            window.present();
        }
        crate::launch_env::window_up();
        Ok(())
    }

    fn close(&self, win: WinId) -> Result<(), String> {
        let Some(window) = self.window(win) else {
            return Err("no such window".into());
        };
        if let Some(noted) = self.asking.borrow_mut().get_mut(&win) {
            // Inside this window's own close request GTK ignores `close()`; the request's handler
            // lets the close under way proceed once the sink has answered.
            *noted = true;
            return Ok(());
        }
        if !window.is_realized() {
            // GTK ignores `close()` on a window that was never realized; nothing can have been
            // shown in it, so it is destroyed without a close request either way.
            window.destroy();
            return Ok(());
        }
        self.closing.borrow_mut().insert(win);
        window.close();
        self.closing.borrow_mut().remove(&win);
        Ok(())
    }

    /// The window's mode toggled as `prefix F11` toggles it, not set to the state the op names: the controller asks
    /// for the state its own toggle leads to, and a bare F11, nvim's `g:neovide_fullscreen` or the compositor may
    /// have changed the real mode since without the controller hearing, so applying that state could make the press
    /// do nothing where the toggle still changes the mode.
    fn toggle_mode(&self, win: WinId) -> Result<(), String> {
        // Cloned out first: the toggle calls into GTK, whose signals post to the sink.
        let toggle = self.mode_toggle.borrow().clone();
        let Some(toggle) = toggle else {
            return Err(NOT_CARRIED_OUT.into());
        };
        if self.window(win).is_none() {
            return Err("no such window".into());
        }
        toggle(win);
        Ok(())
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

    /// Only this process's own window can be moved: the window manager is told its pid (or GNOME's extension finds
    /// the caller's windows), so `win` must be one of this host's.
    fn place(&self, win: WinId, connector: Option<&str>, role: eitri_core::wm::PlacementRole) -> Result<(), String> {
        if self.window(win).is_none() {
            return Err("no such window".into());
        }
        let placer = self.placer.borrow().clone();
        let Some(placer) = placer else {
            return Err(NOT_CARRIED_OUT.into());
        };
        let how = monitor_move(placer.wm(), std::process::id(), connector, role)?;
        placer.place(how);
        Ok(())
    }

    /// The compositor's own tool moves the keys, on its worker. With none, the notice that says so is the refusal,
    /// shown once.
    fn compositor_focus(&self, direction: Direction) -> Result<(), Option<String>> {
        let raise = self.pair_raise.borrow().clone();
        let Some(raise) = raise else {
            return Err(Some(NOT_CARRIED_OUT.into()));
        };
        let notice = RefCell::new(None::<String>);
        if raise.move_focus(direction, &|text| *notice.borrow_mut() = Some(text.to_string())) {
            return Ok(());
        }
        Err(notice.into_inner())
    }

    /// The name of the window manager this window's moves go through, which `companion.wm` picks.
    fn compositor_name(&self) -> &'static str {
        let wm = self.placer.borrow().as_ref().map(|placer| placer.wm());
        eitri_core::compositor::mode::compositor_name(wm.unwrap_or(eitri_core::compositor::Wm::None))
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

#[cfg(test)]
mod monitor_move_tests {
    use super::*;
    use eitri_core::compositor::Wm;
    use eitri_core::wm::PlacementRole;

    #[test]
    fn placement_focus_follows_the_role_on_each_compositor() {
        let restored = PlacementRole::RestoredSecond { first_pid: 101 };
        for (role, focus_pid) in [(PlacementRole::NewSecond, 4242), (restored, 101)] {
            assert_eq!(
                monitor_move(Wm::Sway, 4242, Some("DP-2"), role),
                Ok(MonitorMove::Command(vec![
                    "swaymsg".into(),
                    format!("[pid=4242] move container to output \"DP-2\"; [pid={focus_pid}] focus")
                ]))
            );
            assert_eq!(
                monitor_move(Wm::Hyprland, 4242, Some("DP-2"), role),
                Ok(MonitorMove::Hyprland {
                    pid: 4242,
                    connector: Some("DP-2".into()),
                    role
                })
            );
        }
        assert_eq!(
            monitor_move(Wm::Sway, 4242, None, restored),
            Ok(MonitorMove::Command(vec!["swaymsg".into(), "[pid=101] focus".into()]))
        );
        assert_eq!(
            monitor_move(Wm::Hyprland, 4242, None, restored),
            Ok(MonitorMove::Hyprland {
                pid: 4242,
                connector: None,
                role: restored
            })
        );
        for role in [PlacementRole::Main, PlacementRole::NewSecond, restored] {
            assert_eq!(
                monitor_move(Wm::Niri, 4242, Some("DP-2"), role),
                Ok(MonitorMove::ById {
                    pid: 4242,
                    connector: "DP-2".into()
                })
            );
            assert_eq!(
                monitor_move(Wm::Gnome, 4242, Some("DP-2"), role),
                Ok(MonitorMove::Gnome {
                    connector: "DP-2".into()
                })
            );
            for wm in [Wm::Niri, Wm::Gnome, Wm::None] {
                assert_eq!(monitor_move(wm, 4242, None, role), Ok(MonitorMove::Unchanged));
            }
            assert!(monitor_move(Wm::None, 4242, Some("DP-2"), role).is_err());
            for wm in [Wm::Sway, Wm::Hyprland, Wm::Niri, Wm::Gnome] {
                assert!(monitor_move(wm, 4242, Some("DP-2; focus"), role).is_err());
            }
        }
    }

    #[test]
    fn a_new_module_window_is_focused_by_its_pid_once_mapped_and_moved_nowhere() {
        let role = PlacementRole::NewModule;
        assert_eq!(
            monitor_move(Wm::Sway, 4242, None, role),
            Ok(MonitorMove::Command(vec!["swaymsg".into(), "[pid=4242] focus".into()]))
        );
        assert_eq!(
            monitor_move(Wm::Hyprland, 4242, None, role),
            Ok(MonitorMove::Hyprland {
                pid: 4242,
                connector: None,
                role
            })
        );
        for wm in [Wm::Niri, Wm::Gnome, Wm::None] {
            assert_eq!(monitor_move(wm, 4242, None, role), Ok(MonitorMove::Unchanged), "{wm:?}");
        }
    }

    #[test]
    fn the_placement_command_is_chosen_per_compositor() {
        assert_eq!(
            monitor_move(Wm::Sway, 4242, Some("DP-2"), PlacementRole::Main),
            Ok(MonitorMove::Command(vec![
                "swaymsg".into(),
                "[pid=4242] move container to output \"DP-2\"".into()
            ]))
        );
        assert_eq!(
            monitor_move(Wm::Hyprland, 4242, Some("DP-2"), PlacementRole::Main),
            Ok(MonitorMove::Hyprland {
                pid: 4242,
                connector: Some("DP-2".into()),
                role: PlacementRole::Main
            }),
            "Hyprland's command is written on the worker, in the form its config manager takes"
        );
        assert_eq!(
            monitor_move(Wm::Niri, 4242, Some("HDMI-A-1"), PlacementRole::Main),
            Ok(MonitorMove::ById {
                pid: 4242,
                connector: "HDMI-A-1".into()
            }),
            "niri's command needs the window's id, read on the worker"
        );
        assert_eq!(
            monitor_move(Wm::Gnome, 4242, Some("DP-2"), PlacementRole::Main),
            Ok(MonitorMove::Gnome {
                connector: "DP-2".into()
            }),
            "GNOME's extension is asked, by the caller's own pid"
        );
        assert_eq!(
            monitor_move(Wm::None, 4242, Some("DP-2"), PlacementRole::Main),
            Err(NO_OUTPUT_MOVE.to_string())
        );
        for bad in ["", "DP-1\" ; exit", "two words", &"x".repeat(65)] {
            for wm in [Wm::Sway, Wm::Hyprland, Wm::Niri, Wm::Gnome] {
                assert!(
                    monitor_move(wm, 4242, Some(bad), PlacementRole::Main).is_err(),
                    "{wm:?} refuses {bad:?}, which names no output"
                );
            }
        }
    }
}

#[cfg(test)]
mod window_port_tests {
    use super::*;
    use eitri_core::wm::conformance::RecordingSink;
    use eitri_core::wm::Applied;
    use eitri_core::wm::{Host, HostOp, HostReads, OpTag};

    struct Compositor(Option<&'static str>);
    impl PairRaise for Compositor {
        fn raise(&self, _: &gtk4::Window, _: Option<&str>) {
            unreachable!()
        }
        fn move_focus(&self, _: Direction, on_none: &dyn Fn(&str)) -> bool {
            match self.0 {
                Some(notice) => {
                    on_none(notice);
                    false
                }
                None => true,
            }
        }
    }

    /// The GTK host with no window, the router over it, and where the router posts.
    fn router() -> (Rc<GtkHost>, Rc<WmHost>, Rc<RecordingSink>) {
        let host = GtkHost::new();
        let router = host.router();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        router.attach(Rc::downgrade(&erased));
        (host, router, sink)
    }

    #[test]
    fn the_off_key_hint_names_the_window_manager_this_window_moves_through() {
        struct Placer(eitri_core::compositor::Wm);
        impl MonitorPlacer for Placer {
            fn wm(&self) -> eitri_core::compositor::Wm {
                self.0
            }
            fn place(&self, _: MonitorMove) {
                unreachable!()
            }
        }
        let (host, router, _sink) = router();
        assert_eq!(router.compositor_name(), "the compositor");
        use eitri_core::compositor::Wm;
        for (wm, name) in [(Wm::Hyprland, "Hyprland"), (Wm::Sway, "sway"), (Wm::Niri, "niri")] {
            host.set_monitor_placer(Rc::new(Placer(wm)));
            assert_eq!(router.compositor_name(), name);
        }
    }

    #[test]
    fn a_focus_move_past_the_edge_is_the_compositors_and_says_once_when_it_has_none() {
        let (host, router, sink) = router();
        let op = HostOp::CompositorFocus {
            win: WinId::MAIN,
            direction: Direction::Right,
        };
        host.set_pair_raise(Rc::new(Compositor(None)));
        assert_eq!(router.apply(&op), Applied::Done);
        host.set_pair_raise(Rc::new(Compositor(Some("no compositor tool"))));
        assert_eq!(router.apply(&op), Applied::Refused);
        assert!(sink.events().into_iter().any(|(_, event)| matches!(
            event,
            HostEvent::Refused { what: OpTag::CompositorFocus, why, .. } if why == "no compositor tool"
        )));
    }
}
