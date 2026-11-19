//! The window: Neovide's own, with the panel's web view on its right. [`Host`] is everything the window
//! holds (which pane has the keys, the views, the editor's event proxy, the panel once assembled) and is the
//! assembly's [`HostActions`]; [`MacEmbedder`] is what Neovide calls while it lays the window out and draws.
//!
//! **Which pane has the keys is decided here, and AppKit's first responder follows it.** The panel's focus
//! indicator and nvim's cursor are both driven from [`Host::pane`], so a key must go where that says. AppKit
//! can move the first responder on its own (leaving native full screen put it back on the editor's view while
//! the panel was shown as focused), so the first responder is checked against the pane, and put back, when
//! the window becomes key, when it enters or leaves full screen, and before every key-down is dispatched.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use block2::RcBlock;
use eitri_core::keymap::companion::{strip_pieces, StripPiece};
use eitri_panel::panel_page::PanelPage;
use neovide::embed::{EmbedEvent, EmbedProxy, Embedder};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{msg_send, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSApplicationDidResignActiveNotification, NSAutoresizingMaskOptions, NSResponder, NSView, NSWindow,
    NSWindowDidBecomeKeyNotification, NSWindowDidChangeOcclusionStateNotification,
    NSWindowDidDeminiaturizeNotification, NSWindowDidEnterFullScreenNotification,
    NSWindowDidExitFullScreenNotification, NSWindowDidResignKeyNotification, NSWindowStyleMask,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSNotificationName, NSObjectProtocol, NSPoint, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::editor_tick::EditorTicker;
use super::menu;
use super::page::{repeat, PanelView};
use super::tick::PanelTicker;
use crate::assembly::{Assembly, HostActions};
use crate::keys::Pane;
use crate::layout::{
    first_window_content_pt, grows_first_window, min_content_width_pt, panel_frame_pt, panel_shown, reserved_px,
    MIN_CONTENT_HEIGHT_PT, PANEL_WIDTH_PT,
};

/// How often the host looks after what nothing announces, until it has been seen: Neovide replacing the main
/// menu (once, on the window's first focus; `menu::menu_settled`).
const HOST_TICK: Duration = Duration::from_millis(250);

/// How long a notice the panel cannot show stays in the window's subtitle.
const SUBTITLE_NOTICE: Duration = Duration::from_secs(4);

/// The subtitle: what the prefix waits for while it waits, else a notice still live at `now`, else what holds while
/// the editor has ended (`ended`, how to start nvim again), else nothing.
pub fn subtitle_or_notice(
    strip: String,
    notice: Option<&(String, Instant)>,
    ended: Option<String>,
    now: Instant,
) -> String {
    if !strip.is_empty() {
        return strip;
    }
    match notice {
        Some((text, until)) if now < *until => text.clone(),
        _ => ended.unwrap_or_default(),
    }
}

/// Who AppKit says has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Responder {
    Editor,
    /// The web view or a view inside it.
    Panel,
    Other,
    None,
}

/// Whether AppKit's first responder is the pane that has the keys.
pub fn responder_agrees(pane: Pane, responder: Responder) -> bool {
    matches!(
        (pane, responder),
        (Pane::Editor, Responder::Editor) | (Pane::Panel, Responder::Panel)
    )
}

/// Which pane a click at `x`/`y` (in the editor view's coordinates) lands in: `None` outside the content (the
/// title bar). The panel covers the editor from `panel_left` on while it is shown.
pub fn click_pane(x: f64, y: f64, width: f64, height: f64, panel_left: Option<f64>) -> Option<Pane> {
    #[allow(clippy::neg_cmp_op_on_partial_ord)] // a NaN coordinate must land nowhere
    if !(x >= 0.0 && x <= width && y >= 0.0 && y <= height) {
        return None;
    }
    match panel_left {
        Some(left) if x >= left => Some(Pane::Panel),
        _ => Some(Pane::Editor),
    }
}

/// The prefix's strip as one line of text, for the window's subtitle.
pub fn subtitle_text(pieces: &[StripPiece]) -> String {
    let mut text = String::new();
    for piece in pieces {
        match piece {
            StripPiece::Heading(heading) => {
                text.push_str(heading);
                text.push(' ');
            }
            StripPiece::Run { text: run, .. } => text.push_str(run),
            StripPiece::Dot => text.push_str(" · "),
        }
    }
    text.trim_end().to_owned()
}

/// Runs `f` on a later turn of the main run loop, when whatever AppKit or Neovide is in the middle of has
/// returned. Only on the main thread, which the marker proves: `f` runs there.
pub fn on_next_turn(mtm: MainThreadMarker, f: impl FnOnce() + 'static) {
    after(mtm, Duration::ZERO, f);
}

/// Runs `f` once on the main run loop, `delay` from now (in its common modes). Only on the main thread.
pub fn after(_mtm: MainThreadMarker, delay: Duration, f: impl FnOnce() + 'static) {
    let f = Cell::new(Some(f));
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        if let Some(f) = f.take() {
            f();
        }
    });
    // SAFETY: a one-shot timer added to the main run loop, which retains it (and the block) until it fires.
    unsafe {
        let timer = NSTimer::timerWithTimeInterval_repeats_block(delay.as_secs_f64(), false, &block);
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
    }
}

/// What the window holds. One per process: Eitri hosts one window.
pub struct Host {
    mtm: MainThreadMarker,
    panel_view: PanelView,
    window: RefCell<Option<Retained<NSWindow>>>,
    /// winit's view, the window's content view: the editor.
    editor_view: RefCell<Option<Retained<NSView>>>,
    proxy: RefCell<Option<EmbedProxy>>,
    assembly: RefCell<Option<Rc<Assembly>>>,
    pane: Cell<Pane>,
    hidden_by_user: Cell<bool>,
    subtitle: RefCell<String>,
    /// A notice for the subtitle, with when it goes: what a toast says while the panel is not on screen.
    notice: RefCell<Option<(String, Instant)>>,
    frames: Cell<u64>,
    /// The key monitor, the notification observers and the timers, kept for the window's life.
    monitor: RefCell<Option<Retained<AnyObject>>>,
    pub(super) observers: RefCell<Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    pub(super) timers: RefCell<Vec<Retained<NSTimer>>>,
    /// The panel's tick, from the attach until the shutdown.
    panel_tick: RefCell<Option<Rc<PanelTicker>>>,
    /// The editor's poll, from the moment nvim listens until the shutdown.
    pub(super) editor_tick: RefCell<Option<Rc<EditorTicker>>>,
    /// Starting, quitting and shutting down (`app.rs`).
    pub life: super::app::Lifecycle,
    /// The Edit menu's and the Quit item's targets, made with the window.
    edit_target: RefCell<Option<Retained<menu::EditTarget>>>,
    quit_target: RefCell<Option<Retained<menu::QuitTarget>>>,
    /// Each reason the first responder had to be put back, with how often.
    reconciles: RefCell<Vec<(String, u32)>>,
    /// Windows Neovide made after the first, left without a panel.
    extra_windows: Cell<u32>,
    /// The panel's page has loaded at least once, and the web view has since been first responder once
    /// (`settle_input_method`).
    page_loaded: Cell<bool>,
    input_method_settled: Cell<bool>,
    /// What the turns and the page's refused focus requests did, with when (ms since the host was made), for the
    /// acceptance build's state.
    input_method_log: RefCell<Vec<String>>,
    made_at: Instant,
    /// Turns that failed since the last that held, each tried again a little later, up to `INPUT_METHOD_RETRIES`.
    input_method_retries: Cell<u8>,
}

/// How many times a failed turn through the web view is tried again, and how far apart.
const INPUT_METHOD_RETRIES: u8 = 3;
const INPUT_METHOD_RETRY_AFTER: Duration = Duration::from_millis(300);

impl Host {
    /// Whether the panel has a timer armed, including its fallback steady timer.
    #[cfg(any(test, debug_assertions, feature = "test-support"))]
    pub fn panel_timer_armed(&self) -> bool {
        self.panel_tick.borrow().as_ref().is_some_and(|tick| tick.timer_armed())
    }

    /// The host and its web view, before the window exists: the panel can be assembled with [`Host::page`]
    /// and attached right away; the view joins the window in `window_created`.
    pub fn new(mtm: MainThreadMarker) -> Rc<Host> {
        let panel_view = PanelView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_WIDTH_PT, 0.0)),
        );
        let host = Rc::new(Host {
            mtm,
            panel_view,
            window: RefCell::new(None),
            editor_view: RefCell::new(None),
            proxy: RefCell::new(None),
            assembly: RefCell::new(None),
            pane: Cell::new(Pane::Editor),
            hidden_by_user: Cell::new(false),
            subtitle: RefCell::new(String::new()),
            notice: RefCell::new(None),
            frames: Cell::new(0),
            monitor: RefCell::new(None),
            observers: RefCell::new(Vec::new()),
            timers: RefCell::new(Vec::new()),
            panel_tick: RefCell::new(None),
            editor_tick: RefCell::new(None),
            life: super::app::Lifecycle::default(),
            edit_target: RefCell::new(None),
            quit_target: RefCell::new(None),
            reconciles: RefCell::new(Vec::new()),
            extra_windows: Cell::new(0),
            page_loaded: Cell::new(false),
            input_method_settled: Cell::new(false),
            input_method_log: RefCell::new(Vec::new()),
            made_at: Instant::now(),
            input_method_retries: Cell::new(0),
        });
        host.observe_termination();
        HOST_FOR_LATER.with(|h| *h.borrow_mut() = Rc::downgrade(&host));
        // The page asked for the keys while the editor holds them and was refused: AppKit has already taken
        // them from the editor's view, so they go back.
        let weak = Rc::downgrade(&host);
        host.panel_view.on_loaded(move || {
            if let Some(host) = weak.upgrade() {
                host.page_loaded.set(true);
                // Out of WebKit's callback: the move of the first responder makes WebKit call back in.
                let later = host.clone();
                on_next_turn(host.mtm, move || later.settle_input_method());
            }
        });
        let weak = Rc::downgrade(&host);
        host.panel_view.gate().on_refused(move || {
            if let Some(host) = weak.upgrade() {
                host.note_input_method("the page's focus refused");
                host.reconcile("the page asked for the keys");
                // The page's refused focus leaves the editor unable to compose (`settle_input_method`).
                host.input_method_settled.set(false);
                host.input_method_retries.set(0);
                host.settle_input_method();
            }
        });
        host
    }

    /// The page the panel is assembled with.
    pub fn page(&self) -> Rc<PanelPage> {
        self.panel_view.page()
    }

    /// The assembled panel: its reports reach it, the first document loads, the panel's tick starts, and the
    /// editor's link starts once nvim listens.
    pub fn attach(self: &Rc<Self>, assembly: Rc<Assembly>) {
        *self.assembly.borrow_mut() = Some(assembly.clone());
        #[cfg(feature = "acceptance")]
        {
            self.panel_view
                .report_to(super::acceptance::recording_events(assembly.panel.clone()));
            assembly.panel.load_document(eitri_panel::agent_panel::ViewId::BASE);
        }
        #[cfg(not(feature = "acceptance"))]
        self.panel_view.connect(&assembly.panel);
        *self.panel_tick.borrow_mut() = Some(PanelTicker::start(self.mtm, assembly.panel.clone()));
        self.report_pane_focus();
        self.wait_for_the_editor(assembly);
    }

    pub fn assembly(&self) -> Option<Rc<Assembly>> {
        self.assembly.borrow().clone()
    }

    /// Which pane has the keys.
    pub fn pane(&self) -> Pane {
        self.pane.get()
    }

    fn window(&self) -> Option<Retained<NSWindow>> {
        self.window.borrow().clone()
    }

    /// The window, once Neovide has made it.
    pub fn ns_window(&self) -> Option<Retained<NSWindow>> {
        self.window()
    }

    fn editor_view(&self) -> Option<Retained<NSView>> {
        self.editor_view.borrow().clone()
    }

    fn web_view(&self) -> &NSView {
        self.panel_view.view()
    }

    pub(crate) fn is_our_window(&self, window: Option<&NSWindow>) -> bool {
        match (window, self.window.borrow().as_deref()) {
            (Some(window), Some(ours)) => std::ptr::eq(window, ours),
            _ => false,
        }
    }

    pub(crate) fn mtm(&self) -> MainThreadMarker {
        self.mtm
    }

    /// The window exists: the web view goes in on the right, the window gets its smallest size, and the key
    /// monitor, the window's observers and the host tick start.
    fn window_created(self: &Rc<Self>, window: &winit::window::Window, proxy: EmbedProxy) {
        // The panel, the key monitor and the observers belong to the first window. Another (Neovide's New
        // Window, from its Dock menu) is left as Neovide makes it, without them.
        if self.window.borrow().is_some() {
            self.extra_windows.set(self.extra_windows.get() + 1);
            crate::log_line!(
                "[mac] Neovide made another window; it has no panel and the keys of the first are unchanged"
            );
            return;
        }
        let Some(editor_view) = ns_view_of(window) else {
            crate::log_error!("[mac] the window has no AppKit view; no panel");
            return;
        };
        let Some(ns_window) = editor_view.window() else {
            crate::log_error!("[mac] the editor's view is in no window; no panel");
            return;
        };
        let bounds = editor_view.bounds();
        let web = self.web_view();
        // Against the right edge even when the window is narrower than the panel (the origin is then negative):
        // the autoresizing below keeps that right margin as the window grows, where an origin held at 0 would
        // have the growth added to its left and push the panel past the window's edge.
        web.setFrame(panel_frame(bounds.size));
        editor_view.addSubview(web);
        web.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        web.setTranslatesAutoresizingMaskIntoConstraints(true);
        ns_window.setContentMinSize(NSSize::new(min_content_width_pt(), MIN_CONTENT_HEIGHT_PT));
        if bounds.size.width < min_content_width_pt() {
            // A first start (Neovide's default is 500 by 500 pixels) or a saved size too small for the panel: the
            // window grows to fit it, once Neovide has finished making it, unless it is by then full screen.
            let window = ns_window.clone();
            on_next_turn(self.mtm, move || {
                grow_if_too_narrow(&window);
            });
        }
        *self.window.borrow_mut() = Some(ns_window.clone());
        *self.editor_view.borrow_mut() = Some(editor_view);
        *self.proxy.borrow_mut() = Some(proxy);
        self.show_panel(panel_shown(bounds.size.width, self.hidden_by_user.get()), false);

        let weak = Rc::downgrade(self);
        *self.edit_target.borrow_mut() = Some(menu::EditTarget::new(self.mtm, move || {
            weak.upgrade()
                .is_some_and(|host| host.pane.get() == Pane::Panel && host.responder() == Responder::Panel)
        }));
        let weak = Rc::downgrade(self);
        *self.quit_target.borrow_mut() = Some(menu::QuitTarget::new(self.mtm, move || {
            weak.upgrade()
                .is_none_or(|host| host.quit_requested(crate::quit::QuitSource::Menu))
        }));
        *self.monitor.borrow_mut() = super::keys::install(Rc::downgrade(self));
        self.observe(&ns_window);
        // The window may already be key: no notification will say so.
        self.report_pane_focus();
        let weak = Rc::downgrade(self);
        if !self.keep_edit_menu() {
            let tick = repeat(self.mtm, HOST_TICK, move || match weak.upgrade() {
                // Stops itself once the menu is settled: a frame still runs a pass.
                Some(host) => !host.keep_edit_menu(),
                None => false,
            });
            self.timers.borrow_mut().push(tick);
        }
        #[cfg(feature = "acceptance")]
        super::acceptance::start(Rc::downgrade(self));
    }

    /// One pass over the main menu; whether it is settled (`menu::menu_settled`).
    fn keep_edit_menu(&self) -> bool {
        let (Some(target), Some(quit)) = (self.edit_target.borrow().clone(), self.quit_target.borrow().clone()) else {
            return false;
        };
        let pass = menu::keep_main_menu(self.mtm, &target, &quit);
        if pass.retargeted_quit > 0 {
            self.life.quit_item_retargeted(pass.retargeted_quit);
            crate::log_line!("[mac] the menu's Quit asks first while an agent is at work");
        }
        if pass.inserted_edit_menu {
            self.life.menu_inserted();
            crate::log_line!("[mac] the Edit menu is in the main menu");
        }
        if pass.removed_new_window > 0 {
            self.life.new_window_removed(pass.removed_new_window);
            crate::log_line!(
                "[mac] removed {} New Window item(s) from the main menu",
                pass.removed_new_window
            );
        }
        menu::menu_settled(self.life.quit_items_retargeted(), &pass)
    }

    /// Removes what the window installed into AppKit (the key monitor, the observers) and stops its timers, so
    /// nothing acts on the host while the panel shuts down. Idempotent.
    pub fn teardown(&self) {
        if let Some(monitor) = self.monitor.borrow_mut().take() {
            // SAFETY: the object `addLocalMonitorForEventsMatchingMask:handler:` returned, removed once.
            unsafe { objc2_app_kit::NSEvent::removeMonitor(&monitor) };
        }
        let center = NSNotificationCenter::defaultCenter();
        let observers: Vec<_> = self.observers.borrow_mut().drain(..).collect();
        for token in observers {
            // SAFETY: a token `addObserverForName:object:queue:usingBlock:` returned, removed once.
            unsafe { center.removeObserver(token.as_ref()) };
        }
        let timers: Vec<_> = self.timers.borrow_mut().drain(..).collect();
        for timer in timers {
            timer.invalidate();
        }
        let tick = self.panel_tick.borrow_mut().take();
        if let Some(tick) = tick {
            tick.stop();
        }
        let editor_tick = self.editor_tick.borrow_mut().take();
        if let Some(editor_tick) = editor_tick {
            editor_tick.stop();
        }
    }

    /// The editor's poll: `None` before nvim listens and after the shutdown, else whether it runs on its timer
    /// (`true`) or waits on its sockets (`false`).
    pub fn editor_ticking(&self) -> Option<bool> {
        let tick = self.editor_tick.borrow();
        let tick = tick.as_ref()?;
        (tick.is_ticking() || tick.is_watching()).then(|| tick.is_ticking())
    }

    /// The sources the editor's poll waits on now (none while it ticks on its timer or once it is stopped).
    pub fn editor_watch_sources(&self) -> Vec<dispatch2::DispatchRetained<dispatch2::DispatchSource>> {
        self.editor_tick
            .borrow()
            .as_ref()
            .map(|tick| tick.watch_sources())
            .unwrap_or_default()
    }

    /// Lets go of the editor's poll without stopping it: what a ticker dropped by any other path would do.
    pub fn drop_editor_tick_without_stopping(&self) {
        self.editor_tick.borrow_mut().take();
    }

    /// Whether the panel's tick is driven (from the attach until the shutdown).
    pub fn panel_ticking(&self) -> bool {
        self.panel_tick.borrow().as_ref().is_some_and(|tick| tick.is_running())
    }

    /// Each reason the first responder had to be put back, with how often.
    pub fn reconciles(&self) -> Vec<(String, u32)> {
        self.reconciles.borrow().clone()
    }

    /// Windows Neovide made after the first.
    pub fn extra_windows(&self) -> u32 {
        self.extra_windows.get()
    }

    fn count_reconcile(&self, why: &str) {
        let mut counts = self.reconciles.borrow_mut();
        match counts.iter_mut().find(|(reason, _)| reason == why) {
            Some((_, n)) => *n += 1,
            None => counts.push((why.to_owned(), 1)),
        }
    }

    /// The window's notifications that move or end the keys.
    fn observe(self: &Rc<Self>, window: &NSWindow) {
        let center = NSNotificationCenter::defaultCenter();
        let on = |name: &NSNotificationName, object: Option<&AnyObject>, f: fn(&Host)| {
            let weak: Weak<Host> = Rc::downgrade(self);
            let block = RcBlock::new(move |_note: NonNull<NSNotification>| {
                if let Some(host) = weak.upgrade() {
                    f(&host);
                }
            });
            // SAFETY: the observer runs on the posting (main) thread, no queue given; the token is kept for the
            // window's life, which is the process's.
            let token = unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), object, None, &block) };
            self.observers.borrow_mut().push(token);
        };
        let window: &AnyObject = window.as_ref();
        // SAFETY: AppKit's notification names, static strings it defines.
        unsafe {
            on(NSWindowDidBecomeKeyNotification, Some(window), |host| {
                host.reconcile("the window became key");
                host.report_pane_focus();
                host.settle_input_method();
            });
            on(NSWindowDidResignKeyNotification, Some(window), |host| {
                host.cancel_keys();
                host.report_pane_focus();
            });
            on(
                NSWindowDidEnterFullScreenNotification,
                Some(window),
                Host::after_full_screen,
            );
            on(
                NSWindowDidExitFullScreenNotification,
                Some(window),
                Host::after_full_screen,
            );
            on(NSApplicationDidResignActiveNotification, None, Host::cancel_keys);
            // The page counts as seen only while the window is on screen; back on screen, it is seen.
            on(
                NSWindowDidDeminiaturizeNotification,
                Some(window),
                Host::page_back_on_screen,
            );
            on(
                NSWindowDidChangeOcclusionStateNotification,
                Some(window),
                Host::page_back_on_screen,
            );
        }
    }

    fn page_back_on_screen(&self) {
        let Some(assembly) = self.assembly() else { return };
        if self.page().is_visible() {
            assembly.panel.page_shown(eitri_panel::agent_panel::ViewId::BASE);
        }
    }

    fn after_full_screen(&self) {
        self.reconcile("the window entered or left full screen");
        // AppKit settles the window's responders over the transition's last turns too.
        if let Some(host) = HOST_FOR_LATER.with(|h| h.borrow().upgrade()) {
            on_next_turn(self.mtm, move || host.reconcile("the full-screen transition settled"));
        }
    }

    /// Ends what the prefix waits for: the window lost the keys or a click arrived.
    pub(crate) fn cancel_keys(&self) {
        if let Some(assembly) = self.assembly() {
            assembly.keys.borrow_mut().cancel();
        }
        self.update_subtitle();
    }

    /// Shows what the prefix waits for as the window's subtitle, cleared when it waits for nothing.
    pub(crate) fn update_subtitle(&self) {
        let (Some(assembly), Some(window)) = (self.assembly(), self.window()) else {
            return;
        };
        let waiting = assembly.keys.borrow().waiting();
        let strip = subtitle_text(&strip_pieces(waiting, &assembly.config.keymap));
        let ended = self
            .life
            .editor_ended()
            .then(|| assembly.editor_ended_text(self.life.ended_by_crash()));
        let text = subtitle_or_notice(strip, self.notice.borrow().as_ref(), ended, Instant::now());
        if *self.subtitle.borrow() != text {
            window.setSubtitle(&NSString::from_str(&text));
            *self.subtitle.borrow_mut() = text;
        }
    }

    /// Who AppKit says has the keys.
    pub fn responder(&self) -> Responder {
        let (Some(window), Some(editor)) = (self.window(), self.editor_view()) else {
            return Responder::None;
        };
        let Some(responder) = window.firstResponder() else {
            return Responder::None;
        };
        let responder: &NSResponder = &responder;
        let editor: &NSResponder = &editor;
        let web: &NSView = self.web_view();
        if std::ptr::eq(responder, editor) {
            return Responder::Editor;
        }
        match responder.downcast_ref::<NSView>() {
            Some(view) if std::ptr::eq(view, web) || view.isDescendantOf(web) => Responder::Panel,
            _ => Responder::Other,
        }
    }

    /// Puts AppKit's first responder back on the pane that has the keys, when they disagree.
    pub(crate) fn reconcile(&self, why: &str) {
        let responder = self.responder();
        let pane = self.pane.get();
        if responder_agrees(pane, responder) {
            return;
        }
        let (Some(window), Some(editor)) = (self.window(), self.editor_view()) else {
            return;
        };
        crate::log_line!(
            "[mac] {why}: the first responder was {responder:?} while the {pane:?} has the keys; given back"
        );
        self.count_reconcile(why);
        match pane {
            Pane::Editor => {
                self.panel_view.gate().set_editor_has_keys(true);
                if !window.makeFirstResponder(Some(&editor)) {
                    crate::log_line!("[mac] AppKit refused the editor the keys back");
                }
            }
            Pane::Panel if self.web_view().isHidden() => {
                self.focus_pane(Pane::Editor);
            }
            Pane::Panel => {
                self.panel_view.gate().set_editor_has_keys(false);
                if !window.makeFirstResponder(Some(self.web_view())) {
                    self.focus_pane(Pane::Editor);
                }
            }
        }
    }

    fn note_input_method(&self, what: &str) {
        let at = self.made_at.elapsed().as_millis();
        self.input_method_log.borrow_mut().push(format!("{at} ms {what}"));
    }

    /// The input method's state for the acceptance build: the page loaded, the turn settled, what happened when.
    pub(crate) fn input_method_state(&self) -> (bool, bool, Vec<String>) {
        (
            self.page_loaded.get(),
            self.input_method_settled.get(),
            self.input_method_log.borrow().clone(),
        )
    }

    /// The editor composes with an input method (Pinyin) only while the page's own focus requests have been met: once
    /// the page has focused its composer with the editor holding the keys (it does at every load, and the gate
    /// refuses it), Pinyin hands winit its letters uncomposed, the editor's view first responder and its input context
    /// current all the while. Measured on macOS 26.7 with the product's page (the acceptance item `pinyin-switch`):
    /// blurring the composer, reactivating the editor's input context, or resigning and retaking first responder do
    /// not mend it; one turn of the keys through the web view does, and holds through input source changes. So after
    /// a load and after each refused focus, once the window is key (the panel shown for the turn alone if it is hidden), the
    /// web view takes the keys and gives them straight back to the editor: the pane stays the editor's, and neither nvim nor the panel is
    /// told of a move. With the keys in the panel, the web view has them and nothing is needed.
    fn settle_input_method(&self) {
        if self.input_method_settled.get() || !self.page_loaded.get() {
            return;
        }
        let (Some(window), Some(editor)) = (self.window(), self.editor_view()) else {
            return;
        };
        if self.pane.get() == Pane::Panel && self.responder() == Responder::Panel {
            self.input_method_settled.set(true);
            return;
        }
        if !window.isKeyWindow() || self.pane.get() != Pane::Editor {
            return;
        }
        // A hidden view cannot take the keys, and a refusal while the panel is hidden breaks the editor the same way
        // (measured: `pinyin-hidden`): the view is shown for the turn alone and hidden again before this returns, so
        // AppKit, which draws after the run loop's turn, never draws it, and no key arrives in between.
        let web = self.web_view();
        let was_hidden = web.isHidden();
        if was_hidden {
            // Not reported shown: the open tab's news would count as seen.
            self.panel_view.show_unreported(true);
            web.setHidden(false);
        }
        let gate = self.panel_view.gate();
        gate.set_editor_has_keys(false);
        let took = window.makeFirstResponder(Some(web));
        gate.set_editor_has_keys(true);
        let back = window.makeFirstResponder(Some(&editor));
        if was_hidden {
            web.setHidden(true);
            self.panel_view.show_unreported(false);
        }
        // Settled only by a whole turn: a view that kept the keys goes back to the pane's, and the turn is tried again.
        self.input_method_settled.set(took && back);
        crate::log_line!("[mac] the input method's turn through the web view: taken {took}, given back {back}");
        self.note_input_method(&format!("turn: taken {took}, given back {back}"));
        if !back {
            self.reconcile("the input method's turn could not give the editor the keys back");
        }
        if took && back {
            self.input_method_retries.set(0);
        } else if self.input_method_retries.get() < INPUT_METHOD_RETRIES {
            // Nothing else would try again: the editor's own keys find the first responder agreeing with the pane.
            self.input_method_retries.set(self.input_method_retries.get() + 1);
            if let Some(host) = HOST_FOR_LATER.with(|h| h.borrow().upgrade()) {
                after(self.mtm, INPUT_METHOD_RETRY_AFTER, move || host.settle_input_method());
            }
        }
    }

    /// Moves the keys to `pane`: the composition in the pane that loses them ends, AppKit's first responder
    /// moves, nvim hears about it (FocusLost/FocusGained, its cursor), and the panel learns whether it has
    /// keyboard focus. `true` when `pane` has the keys afterwards.
    pub fn focus_pane(&self, pane: Pane) -> bool {
        let (Some(window), Some(editor)) = (self.window(), self.editor_view()) else {
            crate::log_line!("[mac] no window yet: the keys stay with the {:?}", self.pane.get());
            return self.pane.get() == pane;
        };
        let gate = self.panel_view.gate();
        match pane {
            // An ended editor takes no keys while the panel can have them: Neovide would keep them for the next nvim.
            Pane::Editor if self.life.editor_ended() && !self.web_view().isHidden() => {
                crate::log_line!(
                    "[mac] the editor has ended: the keys stay with the {:?}",
                    self.pane.get()
                );
                return self.pane.get() == pane;
            }
            Pane::Editor => {
                self.end_page_composition();
                gate.set_editor_has_keys(true);
                if !window.makeFirstResponder(Some(&editor)) {
                    // The pane stays as it was: it must name whoever AppKit gives the keys to. The page's composition
                    // was not interrupted after all.
                    self.panel_view.keep_composition();
                    crate::log_line!("[mac] AppKit refused the editor the keys");
                    gate.set_editor_has_keys(self.pane.get() == Pane::Editor);
                    return false;
                }
            }
            Pane::Panel => {
                if self.web_view().isHidden() {
                    crate::log_line!("[mac] the panel is hidden: the keys stay with the editor");
                    return false;
                }
                self.end_editor_composition();
                // Before the move: the web view asks the gate when AppKit makes it first responder.
                gate.set_editor_has_keys(false);
                if !window.makeFirstResponder(Some(self.web_view())) {
                    crate::log_line!("[mac] AppKit refused the panel the keys");
                    gate.set_editor_has_keys(self.pane.get() == Pane::Editor);
                    return false;
                }
            }
        }
        if pane == Pane::Panel {
            // The web view has been first responder: what `settle_input_method` does is done.
            self.input_method_settled.set(true);
        }
        if self.pane.replace(pane) != pane {
            if let Some(proxy) = self.proxy.borrow().as_ref() {
                proxy.send(EmbedEvent::PaneFocus {
                    editor: pane == Pane::Editor,
                });
            }
        }
        self.report_pane_focus();
        true
    }

    /// Tells the panel where the keys are (`Assembly::report_keys`): whether it has keyboard focus, and whether
    /// the editor has the keys, which the typing cadence follows.
    fn report_pane_focus(&self) {
        let Some(assembly) = self.assembly() else { return };
        let key = self.window().is_some_and(|w| w.isKeyWindow());
        assembly.report_keys(self.pane.get(), key);
    }

    /// Whether the editor's text input has a composition open.
    pub(crate) fn editor_marked_text(&self) -> bool {
        // SAFETY: winit's view implements NSTextInputClient, whose `hasMarkedText` takes nothing.
        self.editor_view()
            .is_some_and(|view| unsafe { msg_send![&*view, hasMarkedText] })
    }

    /// The editor view's input context as AppKit holds it: the keyboard input source it has selected, and whether it
    /// is the current (active) context. For the acceptance build's state.
    pub(crate) fn editor_input_context(&self) -> (Option<String>, bool) {
        let Some(context) = self.editor_view().and_then(|view| view.inputContext()) else {
            return (None, false);
        };
        let source = context.selectedKeyboardInputSource().map(|s| s.to_string());
        let current = objc2_app_kit::NSTextInputContext::currentInputContext(self.mtm)
            .is_some_and(|c| std::ptr::eq(&*c, &*context));
        (source, current)
    }

    /// The acceptance build's probe of the editor's input context: `discard` (`discardMarkedText`), `reactivate`
    /// (`deactivate` then `activate`), `resign` (the window's first responder to none and back to the editor).
    #[cfg(feature = "acceptance")]
    pub(crate) fn probe_editor_input_context(&self, op: &str) -> bool {
        let (Some(view), Some(window)) = (self.editor_view(), self.window()) else {
            return false;
        };
        let Some(context) = view.inputContext() else {
            return false;
        };
        match op {
            "discard" => context.discardMarkedText(),
            "reactivate" => {
                context.deactivate();
                context.activate();
            }
            "resign" => {
                window.makeFirstResponder(None);
                window.makeFirstResponder(Some(&view));
            }
            "pane_focus" => {
                if let Some(proxy) = self.proxy.borrow().as_ref() {
                    proxy.send(EmbedEvent::PaneFocus { editor: false });
                    proxy.send(EmbedEvent::PaneFocus { editor: true });
                }
            }
            "web_cycle_gated" => {
                let gate = self.panel_view.gate();
                gate.set_editor_has_keys(false);
                let took = window.makeFirstResponder(Some(self.web_view()));
                gate.set_editor_has_keys(true);
                window.makeFirstResponder(Some(&view));
                crate::log_line!("[mac] probe: the web view took the keys: {took}");
            }
            "web_context" => {
                if let Some(web) = self.web_view().inputContext() {
                    web.activate();
                    web.deactivate();
                }
                context.activate();
            }
            _ => return false,
        }
        true
    }

    /// Cancels a composition open in the editor. Left alone, winit's view keeps its marked text after it
    /// loses the keys, and the next composition starts from it.
    pub(crate) fn end_editor_composition(&self) {
        let Some(view) = self.editor_view() else { return };
        if !self.editor_marked_text() {
            return;
        }
        // SAFETY: NSTextInputClient's `unmarkText`, which winit implements: it drops the marked text and tells
        // nvim the preedit is empty.
        let _: () = unsafe { msg_send![&*view, unmarkText] };
        // The input method closes its own session (and its candidate window) too.
        if let Some(context) = view.inputContext() {
            context.discardMarkedText();
        }
        crate::log_line!("[mac] the editor's composition was cancelled");
    }

    /// Asks the input method to drop a composition open in the page. WebKit keeps the composition in the web
    /// process; what it does with the marked text when the view loses the keys is the page's.
    fn end_page_composition(&self) {
        // The page puts its composer back as it was before the composition; the input method drops its session.
        self.panel_view.cancel_composition();
        if let Some(context) = self.web_view().inputContext() {
            context.discardMarkedText();
        }
    }

    /// Whether the panel's view is hidden now (by the zoom or a window too narrow for it).
    pub(crate) fn panel_view_hidden(&self) -> bool {
        self.web_view().isHidden()
    }

    /// Where a click in the window lands, from its location in window coordinates.
    pub(crate) fn click_target(&self, location_in_window: NSPoint) -> Option<Pane> {
        let editor = self.editor_view()?;
        let point = editor.convertPoint_fromView(location_in_window, None);
        let bounds = editor.bounds();
        let web = self.web_view();
        let panel_left = (!web.isHidden()).then(|| web.frame().origin.x);
        click_pane(point.x, point.y, bounds.size.width, bounds.size.height, panel_left)
    }

    /// Shows or hides the web view. Hiding it while it has the keys gives them to the editor: at once when
    /// `now`, else on the next turn (Neovide is laying the window out).
    fn show_panel(&self, shown: bool, now: bool) {
        let web = self.web_view();
        if web.isHidden() != shown {
            return;
        }
        if shown {
            if let Some(editor) = self.editor_view() {
                // Placed again from the window as it is now, whatever the resizes while hidden made of its frame.
                web.setFrame(panel_frame(editor.bounds().size));
            }
        }
        web.setHidden(!shown);
        crate::log_line!("[mac] panel {}", if shown { "shown" } else { "hidden" });
        if shown && !self.input_method_settled.get() {
            // Neovide is laying the window out: the turn through the web view waits for the next turn of the loop.
            if let Some(host) = HOST_FOR_LATER.with(|h| h.borrow().upgrade()) {
                on_next_turn(self.mtm, move || host.settle_input_method());
            }
        }
        if shown || self.pane.get() != Pane::Panel {
            return;
        }
        if now {
            self.focus_pane(Pane::Editor);
        } else if let Some(host) = HOST_FOR_LATER.with(|h| h.borrow().upgrade()) {
            // Only if it still holds then: a resize may have shown the panel again meanwhile.
            on_next_turn(self.mtm, move || {
                if host.web_view().isHidden() && host.pane.get() == Pane::Panel {
                    host.focus_pane(Pane::Editor);
                }
            });
        }
    }

    fn content_width_pt(&self) -> f64 {
        self.editor_view().map(|v| v.bounds().size.width).unwrap_or(0.0)
    }

    pub fn hidden_by_user(&self) -> bool {
        self.hidden_by_user.get()
    }

    pub fn frames(&self) -> u64 {
        self.frames.get()
    }

    pub fn subtitle(&self) -> String {
        self.subtitle.borrow().clone()
    }

    pub fn panel_view(&self) -> &PanelView {
        &self.panel_view
    }

    pub fn is_full_screen(&self) -> bool {
        self.window()
            .is_some_and(|w| w.styleMask().contains(NSWindowStyleMask::FullScreen))
    }

    pub fn is_key(&self) -> bool {
        self.window().is_some_and(|w| w.isKeyWindow())
    }
}

thread_local! {
    /// The host, for work deferred to a later turn from places that hold only `&Host`.
    static HOST_FOR_LATER: RefCell<Weak<Host>> = const { RefCell::new(Weak::new()) };
}

/// The host, for work deferred to a later turn.
pub(super) fn host_for_later() -> Option<Rc<Host>> {
    HOST_FOR_LATER.with(|h| h.borrow().upgrade())
}

impl HostActions for Host {
    fn focus(&self, pane: Pane) -> bool {
        self.focus_pane(pane)
    }

    fn toast(&self, text: &str) {
        match self.assembly() {
            Some(assembly) if self.panel_visible() => assembly.panel.show_notice(text),
            // The panel's own notice would not be seen: the window's subtitle says it, for a few seconds.
            Some(_) => {
                crate::log_line!("[mac] notice: {text}");
                *self.notice.borrow_mut() = Some((text.to_owned(), Instant::now() + SUBTITLE_NOTICE));
                self.update_subtitle();
                // The notice goes from the subtitle when it is due; nothing else would redraw it then.
                let weak = HOST_FOR_LATER.with(|h| h.borrow().clone());
                after(self.mtm, SUBTITLE_NOTICE, move || {
                    if let Some(host) = weak.upgrade() {
                        host.update_subtitle();
                    }
                });
            }
            None => crate::log_line!("[mac] {text}"),
        }
    }

    fn show_editor(&self) {
        NSApplication::sharedApplication(self.mtm).activate();
        if let Some(window) = self.window() {
            window.makeKeyAndOrderFront(None);
        }
        self.focus_pane(Pane::Editor);
    }

    fn set_panel_hidden(&self, hidden: bool) {
        self.hidden_by_user.set(hidden);
        self.show_panel(panel_shown(self.content_width_pt(), hidden), true);
        if let Some(proxy) = self.proxy.borrow().as_ref() {
            proxy.send(EmbedEvent::Layout);
        }
    }

    fn panel_visible(&self) -> bool {
        panel_shown(self.content_width_pt(), self.hidden_by_user.get())
    }

    fn editor_ended(&self) -> bool {
        self.life.editor_ended()
    }

    fn restart_editor(&self) -> bool {
        let Some(host) = host_for_later() else { return false };
        match host.begin_restart() {
            super::app::Restart::NotEnded => return false,
            super::app::Restart::PathTaken => {
                self.toast(super::app::SOCKET_STILL_TAKEN);
                return true;
            }
            super::app::Restart::Started => {}
        }
        let sent = self
            .proxy
            .borrow()
            .as_ref()
            .is_some_and(|proxy| proxy.send(EmbedEvent::RestartEditor));
        if !sent {
            crate::log_error!("[mac] the restart did not reach Neovide (no event loop): nvim is not started again");
        }
        // The keys go to the editor now: typed before the new nvim reads them, they wait for it.
        self.focus_pane(Pane::Editor);
        self.update_subtitle();
        true
    }
}

/// Grows a window too narrow for the panel to the first window's size, centred, unless it is full screen or on its
/// way there (a size set under the transition is the one AppKit comes back to) or already wide enough
/// (`layout::grows_first_window`). The width is read now, not when the need was first seen.
fn grow_if_too_narrow(window: &NSWindow) {
    let width = window.contentLayoutRect().size.width;
    let full_screen = window.styleMask().contains(NSWindowStyleMask::FullScreen);
    if !grows_first_window(width, full_screen) {
        crate::log_line!("[mac] the window is not grown: {width} pt wide, full screen {full_screen}");
        return;
    }
    let visible = window.screen().map(|screen| screen.visibleFrame().size);
    let (width, height) = first_window_content_pt(
        visible.map_or(f64::NAN, |s| s.width),
        visible.map_or(f64::NAN, |s| s.height),
    );
    crate::log_line!("[mac] the window is too narrow for the panel; grown to {width} x {height} pt");
    window.setContentSize(NSSize::new(width, height));
    window.center();
}

/// The web view's frame in the editor's view (`layout::panel_frame_pt`).
fn panel_frame(editor: NSSize) -> NSRect {
    let (x, y, width, height) = panel_frame_pt(editor.width, editor.height);
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

/// winit's view for `window`, retained.
fn ns_view_of(window: &winit::window::Window) -> Option<Retained<NSView>> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None;
    };
    // SAFETY: winit's AppKit handle names its live NSView, valid for the window's life; retaining it keeps
    // it for as long as the host uses it.
    unsafe { Retained::retain(appkit.ns_view.as_ptr().cast::<NSView>()) }
}

/// What Neovide calls while it lays the window out and draws.
pub struct MacEmbedder(Rc<Host>);

impl MacEmbedder {
    pub fn new(host: Rc<Host>) -> MacEmbedder {
        MacEmbedder(host)
    }
}

impl Embedder for MacEmbedder {
    fn reserved_right(&self, inner_width_px: u32, scale_factor: f64) -> u32 {
        let content_width_pt = f64::from(inner_width_px) / scale_factor;
        let hidden_by_user = self.0.hidden_by_user.get();
        self.0.show_panel(panel_shown(content_width_pt, hidden_by_user), false);
        reserved_px(content_width_pt, scale_factor, hidden_by_user)
    }

    fn editor_has_keys(&self) -> bool {
        self.0.pane.get() == Pane::Editor
    }

    fn window_created(&self, window: &winit::window::Window, proxy: EmbedProxy) {
        self.0.window_created(window, proxy);
    }

    fn frame_drawn(&self, _render: Duration) {
        self.0.frames.set(self.0.frames.get() + 1);
        self.0.keep_edit_menu();
    }

    fn close_requested(&self) -> bool {
        self.0.quit_requested(crate::quit::QuitSource::CloseButton)
    }

    fn editor_ended(&self) -> bool {
        self.0.nvim_ended()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_responder_agrees_only_with_its_own_pane() {
        assert!(responder_agrees(Pane::Editor, Responder::Editor));
        assert!(responder_agrees(Pane::Panel, Responder::Panel));
        for responder in [Responder::Other, Responder::None, Responder::Panel] {
            assert!(!responder_agrees(Pane::Editor, responder), "{responder:?}");
        }
        for responder in [Responder::Other, Responder::None, Responder::Editor] {
            assert!(!responder_agrees(Pane::Panel, responder), "{responder:?}");
        }
    }

    #[test]
    fn a_click_lands_in_the_pane_under_it_and_nowhere_outside_the_content() {
        let panel = Some(800.0);
        assert_eq!(click_pane(10.0, 10.0, 1240.0, 700.0, panel), Some(Pane::Editor));
        assert_eq!(click_pane(799.9, 10.0, 1240.0, 700.0, panel), Some(Pane::Editor));
        assert_eq!(click_pane(800.0, 10.0, 1240.0, 700.0, panel), Some(Pane::Panel));
        assert_eq!(
            click_pane(1000.0, 10.0, 1240.0, 700.0, None),
            Some(Pane::Editor),
            "a hidden panel"
        );
        assert_eq!(click_pane(1000.0, 720.0, 1240.0, 700.0, panel), None, "the title bar");
        assert_eq!(click_pane(-1.0, 10.0, 1240.0, 700.0, panel), None);
        assert_eq!(click_pane(f64::NAN, 10.0, 1240.0, 700.0, panel), None);
    }

    #[test]
    fn a_notice_shows_in_the_subtitle_until_it_ends_or_the_prefix_waits() {
        let now = Instant::now();
        let notice = ("too narrow".to_owned(), now + Duration::from_secs(4));
        assert_eq!(
            subtitle_or_notice(String::new(), Some(&notice), None, now),
            "too narrow"
        );
        assert_eq!(
            subtitle_or_notice("c new tab".into(), Some(&notice), None, now),
            "c new tab"
        );
        assert_eq!(
            subtitle_or_notice(String::new(), Some(&notice), None, now + Duration::from_secs(4)),
            ""
        );
        assert_eq!(subtitle_or_notice(String::new(), None, None, now), "");
    }

    /// While the editor has ended the subtitle says how to start it again, under the prefix's strip and a live notice.
    #[test]
    fn the_ended_editor_shows_in_the_subtitle_when_nothing_else_does() {
        let now = Instant::now();
        let ended = || Some("nvim exited".to_owned());
        assert_eq!(subtitle_or_notice(String::new(), None, ended(), now), "nvim exited");
        let notice = ("too narrow".to_owned(), now + Duration::from_secs(4));
        assert_eq!(
            subtitle_or_notice(String::new(), Some(&notice), ended(), now),
            "too narrow"
        );
        assert_eq!(
            subtitle_or_notice(String::new(), Some(&notice), ended(), now + Duration::from_secs(4)),
            "nvim exited"
        );
        assert_eq!(subtitle_or_notice("c new tab".into(), None, ended(), now), "c new tab");
    }

    #[test]
    fn the_subtitle_reads_like_the_strip() {
        assert_eq!(subtitle_text(&[]), "");
        let pieces = [
            StripPiece::Run {
                text: "c new tab".into(),
                dimmed: false,
            },
            StripPiece::Dot,
            StripPiece::Run {
                text: "n next tab".into(),
                dimmed: true,
            },
        ];
        assert_eq!(subtitle_text(&pieces), "c new tab · n next tab");
        assert_eq!(
            subtitle_text(&[StripPiece::Heading("not in a companion window".into())]),
            "not in a companion window"
        );
    }
}
