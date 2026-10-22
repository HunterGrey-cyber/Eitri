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
//! [`LuaRunner`] the shell sets (`init.lua` and the events it emits are the binary's). `Focus` goes
//! through the shell's own focus function when one is set ([`GtkHost::set_focus`]), since the shell
//! knows what a bare grab does not: whether the layout shows the module, a page still to be built,
//! the editor's grab that also tells the input method. Without one it is the bare grab.
//!
//! **Lifetime.** The host holds its windows weakly. GTK drops its own reference when a window
//! closes, and only then does the window dispose and emit `destroy`, which is where the host
//! forgets the window and its surfaces; a strong reference here would keep every closed window.

// Nothing runs the driver's shell verbs on this host yet, so part of the host is still unused.
#![allow(dead_code)]

pub(crate) mod keys;
pub(crate) mod scheduler;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::{Rc, Weak};

use eitri_core::keymap::Action;
use eitri_core::layout::{ModuleId, Rect};
use eitri_core::wm::{
    ActivationToken, Applied, ChatOp, CloseBy, CloseVerdict, DropWhy, FocusChain, FocusOwner, FocusTo, Host, HostEvent,
    HostOp, HostSink, LuaOp, Monitor, MonitorId, OpTag, PluginOp, SurfaceKind, TermOp, WinId, WindowSpec,
};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// What a shell verb runs on: the window, the action and the keycode that carried it (zero when no
/// key did).
type ShellVerbArm = dyn Fn(WinId, &Action, u32);

/// What gives a module the keys when the shell knows more than a bare grab: whether it took.
type FocusFn = dyn Fn(&ModuleId) -> bool;

/// The answer to every op this shell does not carry out through the host yet; its own code still
/// does those, so a controller that emits one learns it went nowhere.
pub(crate) const NOT_CARRIED_OUT: &str = "not carried out by this shell yet";

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
    fn terminal(&self, op: &TermOp) -> Result<(), String>;
    fn plugin(&self, op: &PluginOp) -> Result<(), String>;
    fn chat(&self, op: &ChatOp) -> Result<(), String>;
}

/// What the `Lua` ops run on: the window's Lua kernel. `Err` says why an op was not carried out.
pub(crate) trait LuaRunner {
    fn run(&self, op: &LuaOp) -> Result<(), String>;
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

    /// Posts where `win`'s keys are now: the registered surfaces around the focus widget, innermost
    /// first, the innermost one owning the keys.
    fn report_focus(&self, win: WinId) {
        let Some(window) = self.window(win) else {
            return;
        };
        let surfaces: Vec<(ModuleId, gtk4::Widget)> = self
            .surfaces
            .borrow()
            .iter()
            .filter(|((w, _), _)| *w == win)
            .map(|((_, m), widget)| (m.clone(), widget.clone()))
            .collect();
        let mut chain = Vec::new();
        let mut at = GtkWindowExt::focus(&window);
        while let Some(widget) = at {
            if let Some((module, _)) = surfaces.iter().find(|(_, s)| *s == widget) {
                chain.push(module.clone());
            }
            at = widget.parent();
        }
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
        let FocusTo::Module(module) = to else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
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
            HostOp::Map { win, module } => self.set_mapped(op, *win, module, true),
            HostOp::Unmap { win, module } => self.set_mapped(op, *win, module, false),
            HostOp::CloseWindow { win } => self.close_window(op, *win),
            HostOp::CreateSurface { win, module, kind } => self.create_surface(op, *win, module, kind),
            HostOp::DropSurface { win, module, why } => self.drop_surface(op, *win, module, why),
            HostOp::Terminal(call) => self.service(op, |s| s.terminal(call)),
            HostOp::Plugin(call) => self.service(op, |s| s.plugin(call)),
            HostOp::Chat(call) => self.service(op, |s| s.chat(call)),
            HostOp::OpenWindow { win, spec } => self.open_window(op, *win, spec),
            HostOp::Present { win, activation } => self.present(op, *win, activation.as_ref()),
            HostOp::Lua(call) => self.lua(op, call),
            HostOp::SetMode { .. }
            | HostOp::MoveToMonitor { .. }
            | HostOp::Quit
            | HostOp::PrepareSurface { .. }
            | HostOp::Allocate { .. }
            | HostOp::Restack { .. }
            | HostOp::Chrome { .. }
            | HostOp::Hint(..)
            | HostOp::Editor(..)
            | HostOp::Cleanup(..) => self.refuse(op, NOT_CARRIED_OUT),
        }
    }

    fn run_shell_verb(&self, win: WinId, action: &Action, keycode: u32) {
        // Cloned out first: the arm may set a new arm, or apply ops, from inside.
        let arm = self.shell_verb_arm.borrow().clone();
        match arm {
            Some(arm) => arm(win, action, keycode),
            None => eprintln!("[wm] no shell verb arm is set: {action:?} in {win:?} dropped"),
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
