//! The global `f` HINT's coordinator (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md).
//! `shell` owns the session because only it sees GTK and the WebView at once. Pure logic lives in
//! `neovibe_core::hint`; this file is GTK: collecting targets, drawing labels, capturing keys at the
//! window, landing, and cancelling. The two small decisions that need no display -- what a key
//! means while HINT is up, and how a label is drawn for a typed prefix -- are split out as plain
//! functions so they are unit-tested; everything else here is owed to a GUI pass
//! (`shell/MANUAL_VERIFICATION.md`, "Global f HINT").
//!
//! Three facts about GTK 4.22 that this file is built on, each read out of GTK's own source
//! (tag `4.22.5`, the version installed here) rather than assumed:
//!
//! - Key events are delivered capture-phase from the toplevel down to the focus widget
//!   (`gtkmain.c::gtk_propagate_event_internal`), so a capture controller on the window sees every
//!   key before the editor's `GLArea`, the `WebView` or the top bar's own controller.
//! - `gtk_widget_add_controller` **prepends** (`gtkwidget.c`), and `gtk_widget_run_controllers`
//!   stops at the first non-gesture controller that handles the event. The `<Control><Shift>f`
//!   accelerator is not dispatched by the window's `gtk-application-shortcuts` controller itself:
//!   that one is `GTK_SHORTCUT_SCOPE_GLOBAL`, whose own `handle_event` returns FALSE
//!   (`gtkshortcutcontroller.c::gtk_shortcut_controller_handle_event`). It is registered with the
//!   window's shortcut manager instead (`gtk_shortcut_controller_root`), and the accelerator fires
//!   from the manager's `gtk-shortcut-manager-capture` controller, which every `GtkWindow` gets at
//!   widget init (`gtkshortcutmanager.c::gtk_shortcut_manager_create_controllers`, called from
//!   `gtkwidget.c`'s instance init). That is the controller `GTK_DEBUG=keybindings` names. It was
//!   added long before the HINT's key controller, so the HINT's runs first: a second `Ctrl+Shift+F`
//!   is handled here once, and never also toggles through the `app.hint` accelerator.
//! - `gtk_widget_run_controllers` saves `l->next` before running a controller, and
//!   `gtk_widget_remove_controller` frees that link outright. Removing the *next* controller from
//!   inside a handler is therefore a use-after-free, and a gesture does not end the loop the way a
//!   handled key controller does. So the controllers are never removed from inside their own
//!   dispatch: `end` detaches them on an idle, and until then each one checks that its session is
//!   still the active one and otherwise lets the event through untouched.
//!
//! One more fact, about keyboards rather than GTK: a key held down auto-repeats, and GDK 4 gives a
//! repeated press no flag. The key that lands a HINT (or cancels it) is usually still down when the
//! HINT ends, and so is `Ctrl+Shift+F` when the accelerator starts one. So the key controller
//! remembers the last key pressed ([`Held`]) and swallows its repeats until it is released; after
//! the session ends it stays attached for exactly that long, and no longer (see `end`).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;

use neovibe_core::hint::{order_targets, HintSession, HintStep, Landing, LabelPlan, Slot, TargetOrder, WindowLayout};
use neovide_editor::NeovideEditorPane;

use crate::agent_panel::{AgentPanelHandle, HintInbound};

/// How long `shell` waits for the panel's `hint_targets` before labelling the GTK targets alone
/// (spec §3.3). A panel that never answers -- not loaded, wedged, replaced -- must not hold the
/// window in a HINT with no labels.
const PANEL_ANSWER_TIMEOUT: Duration = Duration::from_millis(300);

/// What one key press means while a HINT is up. The window's capture controller swallows the
/// press whatever this says (spec §4 invariant 6): nothing typed during HINT reaches nvim or the
/// panel's own key handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintKey {
    /// `Esc`, or `Ctrl+Shift+F` again (the trigger is a toggle).
    Cancel,
    Backspace,
    /// A plain character, offered to `HintSession::key`. Letters outside the alphabet are
    /// `Ignored` there, not here, so there is one place that decides what a label letter is.
    /// Always lower case, and the Latin letter of the physical key when a non-Latin layout typed
    /// it (see `classify_key`).
    Letter(char),
    /// Anything else, including a bare modifier and any chord: swallowed, no effect.
    Nothing,
}

/// `same_key` is every keyval the same physical key produces at level 0 in each of the keyboard's
/// layouts (`gdk::Display::map_keycode`), which is where a Latin letter is found for a key typed on
/// a non-Latin layout. The labels are Latin letters and the user types them by where they sit on the
/// keyboard: with CapsLock on (`A`), Shift held, or a Cyrillic layout active (`ф` on the key that is
/// `a`), no label could be typed at all before this (whole-branch review), and a HINT only Esc
/// could leave looked broken.
pub(crate) fn classify_key(key: Key, state: ModifierType, same_key: &[Key]) -> HintKey {
    let ctrl = state.contains(ModifierType::CONTROL_MASK);
    let shift = state.contains(ModifierType::SHIFT_MASK);
    if key == Key::Escape {
        return HintKey::Cancel;
    }
    // With Shift held the keyval arrives as `F`; accept both spellings of the same chord.
    if ctrl && shift && (key == Key::F || key == Key::f) {
        return HintKey::Cancel;
    }
    if state.intersects(ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK) {
        return HintKey::Nothing;
    }
    if key == Key::BackSpace {
        return HintKey::Backspace;
    }
    match key.to_lower().to_unicode() {
        Some(ch) if ch.is_control() || ch.is_whitespace() => HintKey::Nothing,
        Some(ch) if ch.is_ascii() => HintKey::Letter(ch),
        Some(ch) => HintKey::Letter(
            same_key
                .iter()
                .find_map(|k| k.to_lower().to_unicode().filter(char::is_ascii_lowercase))
                .unwrap_or(ch),
        ),
        None => HintKey::Nothing,
    }
}

/// The keyvals `keycode` produces at level 0 across the keyboard's layouts, for `classify_key`.
fn same_key_in_every_layout(widget: Option<gtk4::Widget>, keycode: u32) -> Vec<Key> {
    widget
        .and_then(|w| w.display().map_keycode(keycode))
        .map(|entries| entries.into_iter().filter(|(k, _)| k.level() == 0).map(|(_, key)| key).collect())
        .unwrap_or_default()
}

/// How one GTK label shows after `typed`: Pango markup for its text, and whether it can still be
/// reached. A reachable label dims the prefix already typed; an unreachable one recedes whole
/// (`.hint-label.hint-off`). The same two effects the panel draws (`.hint-typed`/`.hint-off`).
pub(crate) fn label_markup(label: &str, typed: &str) -> (String, bool) {
    if typed.is_empty() {
        return (glib::markup_escape_text(label).to_string(), true);
    }
    match label.strip_prefix(typed) {
        Some(rest) => (
            format!(
                "<span alpha=\"45%\">{}</span>{}",
                glib::markup_escape_text(typed),
                glib::markup_escape_text(rest)
            ),
            true,
        ),
        None => (glib::markup_escape_text(label).to_string(), false),
    }
}

/// The key the HINT's key controller last saw go down and not yet come up. Its auto-repeat is not a
/// new key: without this, holding the landing letter a moment too long sends its repeats to the
/// pane the HINT just focused (an `a` reaching nvim enters append mode -- an action, against spec
/// §4 invariants 1 and 6), and holding `Ctrl+Shift+F` toggles the HINT on and off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    Nothing,
    /// The `F` of the `Ctrl+Shift+F` that started this HINT. The controller never saw its press
    /// (the accelerator did), so it is known by key, not by keycode.
    Trigger,
    /// A key this controller saw pressed, by hardware keycode.
    Key(u32),
}

impl Held {
    fn matches(self, keycode: u32, key: Key) -> bool {
        match self {
            Held::Nothing => false,
            Held::Trigger => key == Key::f || key == Key::F,
            Held::Key(code) => code == keycode,
        }
    }

    /// A press: `true` when it is the held key repeating, which reaches nobody. Any other press is
    /// a fresh key, and becomes the held one.
    pub(crate) fn press(&mut self, keycode: u32, key: Key) -> bool {
        if self.matches(keycode, key) {
            return true;
        }
        *self = Held::Key(keycode);
        false
    }

    /// A release: the held key coming up ends the hold.
    pub(crate) fn release(&mut self, keycode: u32, key: Key) {
        if self.matches(keycode, key) {
            *self = Held::Nothing;
        }
    }
}

struct Active {
    session_id: u64,
    order: TargetOrder,
    /// `None` while waiting for the panel's answer.
    plan: Option<LabelPlan>,
    session: Option<HintSession>,
    /// The GTK labels, each with the global label it shows, in `before` then `after` order.
    labels: Vec<(gtk4::Label, String)>,
    restore_focus: Option<gtk4::Widget>,
    keys: gtk4::EventControllerKey,
    held: Rc<Cell<Held>>,
    clicks: gtk4::GestureClick,
    scrolls: gtk4::EventControllerScroll,
    timeout: Option<glib::SourceId>,
}

pub(crate) struct HintCoordinator {
    window: gtk4::ApplicationWindow,
    /// Wraps the window's root; labels are its overlay children.
    overlay: gtk4::Overlay,
    top_items: Vec<gtk4::Widget>,
    editor: Rc<NeovideEditorPane>,
    editor_widget: gtk4::Widget,
    /// The main slot's widget. The editor unless a Lua plugin replaced it.
    main_widget: gtk4::Widget,
    /// The side slot's widget. The agent panel unless a Lua plugin replaced it.
    side_widget: gtk4::Widget,
    agent_widget: gtk4::Widget,
    bottom: Option<gtk4::Widget>,
    agent: AgentPanelHandle,
    next_session: Cell<u64>,
    active: RefCell<Option<Active>>,
    /// An ended session's key controller, still swallowing the repeats of a key held across the
    /// end. Detached on that key's release, on the next fresh key, on a new HINT, or when the
    /// window stops being active.
    draining: RefCell<Option<gtk4::EventControllerKey>>,
}

pub(crate) struct HintWidgets {
    pub(crate) window: gtk4::ApplicationWindow,
    pub(crate) overlay: gtk4::Overlay,
    pub(crate) top_items: Vec<gtk4::Widget>,
    pub(crate) editor: Rc<NeovideEditorPane>,
    pub(crate) main_widget: gtk4::Widget,
    pub(crate) side_widget: gtk4::Widget,
    pub(crate) agent_widget: gtk4::Widget,
    pub(crate) bottom: Option<gtk4::Widget>,
    pub(crate) agent: AgentPanelHandle,
}

/// Shown, and on screen with a non-zero size: spec §2.2's "看得见" for a GTK widget.
fn visible(widget: &gtk4::Widget) -> bool {
    widget.is_mapped() && widget.width() > 0 && widget.height() > 0
}

impl HintCoordinator {
    pub(crate) fn new(w: HintWidgets) -> Rc<Self> {
        let editor_widget: gtk4::Widget = w.editor.widget().clone().upcast();
        let this = Rc::new(HintCoordinator {
            window: w.window,
            overlay: w.overlay,
            top_items: w.top_items,
            editor: w.editor,
            editor_widget,
            main_widget: w.main_widget,
            side_widget: w.side_widget,
            agent_widget: w.agent_widget,
            bottom: w.bottom,
            agent: w.agent,
            next_session: Cell::new(0),
            active: RefCell::new(None),
            draining: RefCell::new(None),
        });
        // Alt-tab away cancels (spec §2.5). Connected once, for the window's lifetime; a no-op
        // when no HINT is up.
        let weak = Rc::downgrade(&this);
        this.window.connect_notify_local(Some("is-active"), move |window, _| {
            if !window.is_active() {
                if let Some(this) = weak.upgrade() {
                    this.cancel();
                    // A release that happens in another window never reaches this one.
                    this.stop_draining();
                }
            }
        });
        this
    }

    fn is_active(&self) -> bool {
        self.active.borrow().is_some()
    }

    fn active_id(&self) -> Option<u64> {
        self.active.borrow().as_ref().map(|a| a.session_id)
    }

    fn layout(&self) -> WindowLayout {
        WindowLayout {
            top_visible: self.top_items.iter().map(visible).collect(),
            main_visible: visible(&self.main_widget),
            main_is_editor: self.main_widget == self.editor_widget,
            side_visible: visible(&self.side_widget),
            side_is_agent_panel: self.side_widget == self.agent_widget,
            bottom_visible: self.bottom.as_ref().is_some_and(visible),
        }
    }

    fn slot_widget(&self, slot: Slot) -> Option<gtk4::Widget> {
        match slot {
            Slot::Top(i) => self.top_items.get(i).cloned(),
            Slot::Editor => Some(self.editor_widget.clone()),
            Slot::Main => Some(self.main_widget.clone()),
            Slot::Side => Some(self.side_widget.clone()),
            Slot::Bottom => self.bottom.clone(),
        }
    }

    fn stop_draining(&self) {
        if let Some(keys) = self.draining.borrow_mut().take() {
            detach_later(&self.window, keys);
        }
    }

    /// Start a HINT, or cancel the one that is up: `f` on the top bar, and the panel's
    /// `hint_request`.
    pub(crate) fn toggle(self: &Rc<Self>) {
        self.toggle_with(Held::Nothing);
    }

    /// The same, from the `Ctrl+Shift+F` accelerator, whose `F` is still down.
    pub(crate) fn toggle_from_chord(self: &Rc<Self>) {
        self.toggle_with(Held::Trigger);
    }

    fn toggle_with(self: &Rc<Self>, held: Held) {
        if self.is_active() {
            self.cancel();
            return;
        }
        self.stop_draining();
        let session_id = self.next_session.get() + 1;
        self.next_session.set(session_id);

        let order = order_targets(&self.layout());
        let panel_asked = order.ask_panel;
        let restore_focus = gtk4::prelude::GtkWindowExt::focus(&self.window);

        let held = Rc::new(Cell::new(held));
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        let held_on_press = held.clone();
        keys.connect_key_pressed(move |keys, key, keycode, state| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            let mut h = held_on_press.get();
            let repeat = h.press(keycode, key);
            held_on_press.set(h);
            if repeat {
                // Live or draining, a held key's repeat reaches nobody.
                return glib::Propagation::Stop;
            }
            if this.active_id() != Some(session_id) {
                // A fresh key after this session ended: nothing is held any more. Detached on an
                // idle (see the module doc), and the key goes on to whatever it was meant for.
                if this.draining.borrow().as_ref() == Some(keys) {
                    this.draining.borrow_mut().take();
                }
                detach_later(&this.window, keys.clone());
                return glib::Propagation::Proceed;
            }
            this.on_key(classify_key(key, state, &same_key_in_every_layout(keys.widget(), keycode)));
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(self);
        let held_on_release = held.clone();
        keys.connect_key_released(move |keys, key, keycode, _| {
            let mut h = held_on_release.get();
            h.release(keycode, key);
            held_on_release.set(h);
            let Some(this) = weak.upgrade() else { return };
            if h == Held::Nothing && this.draining.borrow().as_ref() == Some(keys) {
                this.stop_draining();
            }
        });

        let clicks = gtk4::GestureClick::new();
        clicks.set_button(0);
        clicks.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        clicks.connect_pressed(move |gesture, _, _, _| {
            let Some(this) = weak.upgrade() else { return };
            if this.active_id() != Some(session_id) {
                return;
            }
            // Claimed, so the click does not also land on (and focus) whatever it hit: a cancel
            // puts focus back where it was, as if nothing had happened (spec §2.5).
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            this.cancel();
        });

        // A scroll cancels too. The labels sit at the positions measured when the HINT started;
        // scrolled content moves under them, and a label left over a different row would land on
        // the row it was drawn for, not the one it now covers. Unlike a click the scroll is not
        // swallowed: the user wanted to scroll, and with the labels gone nothing is stale.
        let scrolls = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        scrolls.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        scrolls.connect_scroll(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                if this.active_id() == Some(session_id) {
                    this.cancel();
                }
            }
            glib::Propagation::Proceed
        });

        self.window.add_controller(keys.clone());
        self.window.add_controller(clicks.clone());
        self.window.add_controller(scrolls.clone());

        let timeout = if panel_asked {
            let weak: Weak<Self> = Rc::downgrade(self);
            Some(glib::timeout_add_local_once(PANEL_ANSWER_TIMEOUT, move || {
                let Some(this) = weak.upgrade() else { return };
                // This source has fired: forget its id so `end` does not remove it a second time.
                if let Some(active) = this.active.borrow_mut().as_mut() {
                    if active.session_id == session_id {
                        active.timeout = None;
                    }
                }
                this.on_targets(session_id, 0);
            }))
        } else {
            None
        };

        *self.active.borrow_mut() = Some(Active {
            session_id,
            order,
            plan: None,
            session: None,
            labels: Vec::new(),
            restore_focus,
            keys,
            held,
            clicks,
            scrolls,
            timeout,
        });

        if panel_asked {
            self.agent.hint_collect(session_id);
        } else {
            self.on_targets(session_id, 0);
        }
    }

    /// The panel's two messages. A `hint_targets` for any session but the active one is ignored
    /// (spec §4 invariant 5), as is a second answer after the timeout already planned without it.
    ///
    /// A `hint_request` only ever starts a HINT, never cancels one. The panel's `f` cannot reach the
    /// panel while a HINT's key controller is attached, so a request that arrives with one up was
    /// posted before it started: a held `f`'s repeat, or `ff` typed faster than this main loop
    /// handled the first. Toggling on it tore down the HINT the first `f` had just opened
    /// (whole-branch review). The panel also stops sending them itself (`App.tsx::requestHint`).
    pub(crate) fn on_panel(self: &Rc<Self>, message: HintInbound) {
        match message {
            HintInbound::Request if self.is_active() => {}
            HintInbound::Request => self.toggle(),
            HintInbound::Targets { session_id, count } => self.on_targets(session_id, count),
        }
    }

    fn on_targets(self: &Rc<Self>, session_id: u64, count: usize) {
        let (plan, order) = {
            let mut guard = self.active.borrow_mut();
            let Some(active) = guard.as_mut() else { return };
            if active.session_id != session_id || active.plan.is_some() {
                return;
            }
            let plan = active.order.plan(count);
            active.session = Some(HintSession::new(plan.all()));
            active.plan = Some(plan.clone());
            if let Some(source) = active.timeout.take() {
                source.remove();
            }
            (plan, active.order.clone())
        };

        if plan.all().is_empty() {
            // Nothing on screen to label: a HINT that nothing can end but Esc is worse than none.
            self.cancel();
            return;
        }
        if order.ask_panel && !plan.panel.is_empty() {
            self.agent.hint_show(session_id, &plan.panel);
        }
        let mut labels = Vec::new();
        for (slot, label) in order.before.iter().zip(&plan.before).chain(order.after.iter().zip(&plan.after)) {
            let Some(widget) = self.slot_widget(*slot) else { continue };
            let Some(point) = widget.compute_point(&self.overlay, &graphene::Point::new(0.0, 0.0)) else {
                // Not in the overlay's tree after all; the label can still be typed, just not seen.
                eprintln!("[hint] no position for a target; its label {label} is not drawn");
                continue;
            };
            let gtk_label = gtk4::Label::new(None);
            gtk_label.add_css_class("hint-label");
            gtk_label.set_can_target(false);
            gtk_label.set_can_focus(false);
            gtk_label.set_halign(gtk4::Align::Start);
            gtk_label.set_valign(gtk4::Align::Start);
            gtk_label.set_margin_start(point.x().max(0.0).round() as i32);
            gtk_label.set_margin_top(point.y().max(0.0).round() as i32);
            gtk_label.set_markup(&label_markup(label, "").0);
            self.overlay.add_overlay(&gtk_label);
            labels.push((gtk_label, label.clone()));
        }
        if let Some(active) = self.active.borrow_mut().as_mut() {
            if active.session_id == session_id {
                active.labels = labels;
                return;
            }
        }
        // The session ended while labels were being built (cannot happen on one thread today, but
        // a stray label outliving its session is exactly invariant 3's failure).
        for (label, _) in labels {
            self.overlay.remove_overlay(&label);
        }
    }

    fn on_key(self: &Rc<Self>, key: HintKey) {
        match key {
            HintKey::Cancel => self.cancel(),
            HintKey::Nothing => {}
            HintKey::Backspace => {
                let typed = {
                    let mut guard = self.active.borrow_mut();
                    let Some(session) = guard.as_mut().and_then(|a| a.session.as_mut()) else { return };
                    session.backspace().to_string()
                };
                self.show_prefix(&typed);
            }
            HintKey::Letter(ch) => {
                let step = {
                    let mut guard = self.active.borrow_mut();
                    // Before the plan exists every key is swallowed and ignored.
                    let Some(session) = guard.as_mut().and_then(|a| a.session.as_mut()) else { return };
                    session.key(ch)
                };
                match step {
                    HintStep::Narrowed { typed } => self.show_prefix(&typed),
                    HintStep::Landed { index } => self.land(index),
                    HintStep::Ignored => {}
                }
            }
        }
    }

    fn show_prefix(&self, typed: &str) {
        let guard = self.active.borrow();
        let Some(active) = guard.as_ref() else { return };
        if active.order.ask_panel {
            self.agent.hint_prefix(active.session_id, typed);
        }
        for (label, text) in &active.labels {
            let (markup, reachable) = label_markup(text, typed);
            label.set_markup(&markup);
            if reachable {
                label.remove_css_class("hint-off");
            } else {
                label.add_css_class("hint-off");
            }
        }
    }

    /// Moves focus to label `index` and does nothing else (spec §4 invariant 1).
    fn land(self: &Rc<Self>, index: usize) {
        let (session_id, landing, panel_asked) = {
            let guard = self.active.borrow();
            let Some(active) = guard.as_ref() else { return };
            let Some(landing) = active.plan.as_ref().and_then(|p| active.order.landing(p, index)) else { return };
            (active.session_id, landing, active.order.ask_panel)
        };
        match landing {
            Landing::Panel(i) => {
                self.agent_widget.grab_focus();
                // `hint_land` ends the panel's half itself (spec §3.3).
                self.agent.hint_land(session_id, i);
            }
            Landing::Gtk(slot) => {
                match (slot, self.slot_widget(slot)) {
                    (Slot::Editor, _) => self.editor.grab_focus(),
                    (_, Some(w)) => {
                        w.grab_focus();
                    }
                    (_, None) => {}
                }
                // The panel may be showing labels too; a GTK landing must clear them.
                if panel_asked {
                    self.agent.hint_end(session_id);
                }
            }
        }
        self.end(false);
    }

    /// Every cancel path (spec §2.5): labels gone, focus back where it was.
    fn cancel(&self) {
        let Some((session_id, panel_asked)) =
            self.active.borrow().as_ref().map(|a| (a.session_id, a.order.ask_panel))
        else {
            return;
        };
        if panel_asked {
            self.agent.hint_end(session_id);
        }
        self.end(true);
    }

    fn end(&self, restore: bool) {
        // Taken out first, so nothing below runs with `active` borrowed and a re-entrant callback
        // (a focus change, an `is-active` notify) sees no session rather than a borrow panic.
        let Some(mut active) = self.active.borrow_mut().take() else { return };
        if let Some(source) = active.timeout.take() {
            source.remove();
        }
        for (label, _) in active.labels.drain(..) {
            self.overlay.remove_overlay(&label);
        }
        // Detached on an idle, never from inside their own dispatch -- see the module doc. The key
        // controller stays while a key is still held across the end (it usually is: the letter
        // that landed, or the `Esc` that cancelled), so that key's repeats reach nobody.
        detach_later(&self.window, active.clicks);
        detach_later(&self.window, active.scrolls);
        if active.held.get() == Held::Nothing {
            detach_later(&self.window, active.keys);
        } else {
            self.stop_draining();
            *self.draining.borrow_mut() = Some(active.keys);
        }
        if restore {
            match active.restore_focus {
                Some(w) if w == self.editor_widget => self.editor.grab_focus(),
                Some(w) => {
                    w.grab_focus();
                }
                None => {}
            }
        }
    }
}

/// Removes `controller` from `window` on an idle, never from inside a dispatch (see the module
/// doc), and only if it is still attached: two paths may both ask.
fn detach_later(window: &gtk4::ApplicationWindow, controller: impl IsA<gtk4::EventController>) {
    let window = window.clone();
    glib::idle_add_local_once(move || {
        if controller.widget().as_ref() == Some(window.upcast_ref::<gtk4::Widget>()) {
            window.remove_controller(&controller);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifierType = ModifierType::empty();

    #[test]
    fn escape_and_the_trigger_chord_cancel() {
        assert_eq!(classify_key(Key::Escape, NONE, &[]), HintKey::Cancel);
        let chord = ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK;
        assert_eq!(classify_key(Key::F, chord, &[]), HintKey::Cancel);
        assert_eq!(classify_key(Key::f, chord, &[]), HintKey::Cancel);
    }

    #[test]
    fn a_plain_letter_is_offered_to_the_session() {
        assert_eq!(classify_key(Key::a, NONE, &[]), HintKey::Letter('a'));
        // `f` is not in the alphabet; `HintSession` ignores it, this does not decide that.
        assert_eq!(classify_key(Key::f, NONE, &[]), HintKey::Letter('f'));
        assert_eq!(classify_key(Key::semicolon, NONE, &[]), HintKey::Letter(';'));
        assert_eq!(classify_key(Key::BackSpace, NONE, &[]), HintKey::Backspace);
    }

    /// CapsLock and Shift type `A`; the label is `a`.
    #[test]
    fn capslock_and_shift_still_type_the_label() {
        assert_eq!(classify_key(Key::A, ModifierType::LOCK_MASK, &[]), HintKey::Letter('a'));
        assert_eq!(classify_key(Key::A, ModifierType::SHIFT_MASK, &[]), HintKey::Letter('a'));
    }

    /// On a Cyrillic layout the key where `a` sits types `ф`: its Latin letter is found in the
    /// keyboard's other layout. With no Latin letter anywhere it stays itself, and `HintSession`
    /// ignores it.
    #[test]
    fn a_non_latin_layout_types_the_latin_letter_on_the_same_key() {
        let same_key = [Key::Cyrillic_ef, Key::a];
        assert_eq!(classify_key(Key::Cyrillic_ef, NONE, &same_key), HintKey::Letter('a'));
        assert_eq!(classify_key(Key::Cyrillic_EF, ModifierType::LOCK_MASK, &same_key), HintKey::Letter('a'));
        assert_eq!(classify_key(Key::Cyrillic_ef, NONE, &[Key::Cyrillic_ef]), HintKey::Letter('ф'));
    }

    /// A non-label key does nothing: not a cancel, not a letter (spec §2.5).
    #[test]
    fn chords_modifiers_and_controls_do_nothing() {
        assert_eq!(classify_key(Key::f, ModifierType::CONTROL_MASK, &[]), HintKey::Nothing);
        assert_eq!(classify_key(Key::a, ModifierType::ALT_MASK, &[]), HintKey::Nothing);
        assert_eq!(classify_key(Key::Shift_L, NONE, &[]), HintKey::Nothing);
        assert_eq!(classify_key(Key::Return, NONE, &[]), HintKey::Nothing);
        assert_eq!(classify_key(Key::Tab, NONE, &[]), HintKey::Nothing);
        assert_eq!(classify_key(Key::space, NONE, &[]), HintKey::Nothing);
        assert_eq!(classify_key(Key::Left, NONE, &[]), HintKey::Nothing);
    }

    #[test]
    fn a_label_dims_its_typed_prefix_and_an_unreachable_one_is_off() {
        assert_eq!(label_markup("as", ""), ("as".to_string(), true));
        assert_eq!(label_markup("as", "a"), ("<span alpha=\"45%\">a</span>s".to_string(), true));
        assert_eq!(label_markup("ds", "a"), ("ds".to_string(), false));
    }

    const A: u32 = 38;
    const F: u32 = 41;

    /// Holding the landing letter: its repeats are swallowed until it comes up, and pressing it
    /// again after that is a fresh key.
    #[test]
    fn a_held_keys_repeats_are_not_fresh_keys() {
        let mut held = Held::Nothing;
        assert!(!held.press(A, Key::a), "the first press is a key");
        assert!(held.press(A, Key::a), "a repeat is not");
        assert!(held.press(A, Key::a));
        held.release(A, Key::a);
        assert_eq!(held, Held::Nothing);
        assert!(!held.press(A, Key::a), "pressed again after release, it is a key again");
    }

    /// Another key going down ends the hold, and a release of some other key does not.
    #[test]
    fn another_key_is_fresh_and_takes_over_the_hold() {
        let mut held = Held::Nothing;
        held.press(A, Key::a);
        held.release(F, Key::f);
        assert_eq!(held, Held::Key(A));
        assert!(!held.press(F, Key::f));
        assert_eq!(held, Held::Key(F));
    }

    /// Holding `Ctrl+Shift+F`: the accelerator saw the press, so the HINT starts holding the
    /// trigger by key. Its repeats do not cancel; after `F` comes up, the chord cancels.
    #[test]
    fn the_triggers_repeats_do_not_toggle_the_hint() {
        let mut held = Held::Trigger;
        assert!(held.press(F, Key::F));
        assert!(held.press(F, Key::f), "Shift released first: still the same key");
        held.release(F, Key::F);
        assert!(!held.press(F, Key::F), "a second chord after release is a key");
    }
}
