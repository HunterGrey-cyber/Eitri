//! Coordinates global HINT sessions. The shell sees GTK and the WebView together, so it collects
//! targets, draws labels, captures window keys, lands, and cancels. The core `hint` module holds
//! the pure session logic. Key classification and label rendering are tested without a display;
//! the GTK integration needs GUI verification.
//!
//! GTK delivers capture-phase key events from the toplevel down to the focus widget, so a window
//! capture controller sees keys before the editor's `GLArea`, the `WebView`, or the top bar.
//! `gtk_widget_add_controller` prepends controllers, and dispatch stops at the first non-gesture
//! controller that handles the event.
//!
//! `gtk_widget_run_controllers` saves `l->next` before each call, while removing a controller
//! frees its link. Removing the next controller inside a handler would cause a use-after-free.
//! A gesture also does not stop dispatch like a handled key controller. `end` therefore detaches
//! controllers on an idle; until then, they ignore events if their session is no longer active.
//!
//! GDK 4 gives repeated presses no distinguishing flag. The key that lands or cancels a HINT,
//! and the key that starts it from `prefix f`, may still be held when the session ends.
//! The controller remembers the last press in [`Held`] and swallows repeats until release.
//! After the session ends, it stays attached only until that release.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;

use eitri_core::hint::{order_targets, HintSession, HintStep, LabelPlan, Landing, Slot, TargetOrder, WindowLayout};
use eitri_core::layout::ModuleId;
use neovide_editor::NeovideEditorPane;

use crate::agent_panel::{AgentPanelHandle, HintInbound};
use crate::module_grid::ModuleHosts;

/// How long `shell` waits for the panel's `hint_targets` before labelling the GTK targets alone.
/// A panel that never answers -- not loaded, wedged, replaced -- must not hold the
/// window in a HINT with no labels.
const PANEL_ANSWER_TIMEOUT: Duration = Duration::from_millis(300);

/// What one key press means while a HINT is up. The window's capture controller swallows the
/// press whatever this says: nothing typed during HINT reaches nvim or the
/// panel's own key handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintKey {
    /// `Esc`.
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
/// `a`), no label could be typed at all before this, and a HINT only Esc
/// could leave looked broken.
pub(crate) fn classify_key(key: Key, state: ModifierType, same_key: &[Key]) -> HintKey {
    if key == Key::Escape {
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
        .map(|entries| {
            entries
                .into_iter()
                .filter(|(k, _)| k.level() == 0)
                .map(|(_, key)| key)
                .collect()
        })
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

/// Tracks the last key the HINT controller saw pressed but not released. Auto-repeat must
/// not count as a new key: repeats of a landing letter could reach the newly focused pane
/// and act on it (`a` enters append mode in nvim). Holding the trigger key (`prefix f`)
/// must not toggle the HINT again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    Nothing,
    /// A key this controller saw pressed, by hardware keycode.
    Key(u32),
}

impl Held {
    fn matches(self, keycode: u32, _key: Key) -> bool {
        match self {
            Held::Nothing => false,
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
    /// The strip entries as they were when the HINT started: their labels sit where these were.
    strips: Vec<(ModuleId, gtk4::Widget)>,
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
    top_items: TopItems,
    /// `None` in a window with no editor: the editor arm of a focus restore then matches nothing.
    editor: Option<Rc<NeovideEditorPane>>,
    editor_widget: Option<gtk4::Widget>,
    agent_widget: gtk4::Widget,
    /// [`HintWidgets::modules`].
    modules: ModuleHosts,
    /// Lands on a module the way that module takes focus (the editor tells the input method).
    focus_module: Rc<dyn Fn(&ModuleId) -> bool>,
    /// [`HintWidgets::strips`].
    strips: StripEntries,
    /// [`HintWidgets::activate_strip`].
    activate_strip: Rc<dyn Fn(&ModuleId)>,
    agent: AgentPanelHandle,
    next_session: Cell<u64>,
    active: RefCell<Option<Active>>,
    /// An ended session's key controller, still swallowing the repeats of a key held across the
    /// end. Detached on that key's release, on the next fresh key, on a new HINT, or when the
    /// window stops being active.
    draining: RefCell<Option<gtk4::EventControllerKey>>,
    /// `select.number`'s digits ([`HintCoordinator::show_numbers`]): labels only, with no session.
    numbers: RefCell<Vec<gtk4::Label>>,
}

pub(crate) struct HintWidgets {
    pub(crate) window: gtk4::ApplicationWindow,
    pub(crate) overlay: gtk4::Overlay,
    pub(crate) top_items: TopItems,
    pub(crate) editor: Option<Rc<NeovideEditorPane>>,
    /// The panel's `WebView` itself, which a panel landing grabs -- never its module host, a
    /// `WebHost` that refuses a grab (`web_host`'s module doc).
    pub(crate) agent_widget: gtk4::Widget,
    /// Every module and its host **in the layout's tree order** (`ModuleGrid::hosts_in_tree_order`):
    /// HINT feeds it straight into `WindowLayout::modules`, which orders its labels by it. The alias
    /// promises no order of its own -- `ModuleGrid::live_hosts` is add order -- so a builder that
    /// "tidied" `main.rs`'s closure into `live_hosts()` would reorder every label, and nothing
    /// headless would notice (found in review).
    pub(crate) modules: ModuleHosts,
    pub(crate) focus_module: Rc<dyn Fn(&ModuleId) -> bool>,
    /// Every container strip's entries that name a module now, with their label widgets
    /// (`ModuleGrid::strip_entries`): labelled after every module, so the modules keep the labels
    /// they had before strips existed.
    pub(crate) strips: StripEntries,
    /// What landing on a strip entry does: the same as a click on it.
    pub(crate) activate_strip: Rc<dyn Fn(&ModuleId)>,
    pub(crate) agent: AgentPanelHandle,
}

/// The top bar's items in order: the project, the workspace, every tray chip, then the bar's own. A chip made for a
/// page loaded into the running window is entered after the other chips; every read clones what it needs out.
pub(crate) type TopItems = Rc<RefCell<Vec<gtk4::Widget>>>;

/// Strip entries with their modules, read when a HINT starts.
pub(crate) type StripEntries = Rc<dyn Fn() -> Vec<(ModuleId, gtk4::Widget)>>;

/// `order` with the strip entries' slots moved behind every other target. The entries are passed
/// to the plan as top-bar slots numbered from `top_items` on (`Slot::Top(top_items + k)` is the
/// k-th entry), which `order_targets` puts first; their labels go last instead, so a window with no
/// tabbed container is labelled exactly as before and a strip never takes a module's label.
pub(crate) fn strips_after_modules(mut order: TargetOrder, top_items: usize) -> TargetOrder {
    let is_strip = |slot: &Slot| matches!(slot, Slot::Top(i) if *i >= top_items);
    let strips: Vec<Slot> = order.before.iter().filter(|s| is_strip(s)).cloned().collect();
    order.before.retain(|s| !is_strip(s));
    order.after.extend(strips);
    order
}

/// Shown, and on screen with a non-zero size: "visible" for a GTK widget.
fn visible(widget: &gtk4::Widget) -> bool {
    widget.is_mapped() && widget.width() > 0 && widget.height() > 0
}

impl HintCoordinator {
    pub(crate) fn new(w: HintWidgets) -> Rc<Self> {
        let editor_widget: Option<gtk4::Widget> = w.editor.as_ref().map(|editor| editor.widget().clone().upcast());
        let this = Rc::new(HintCoordinator {
            window: w.window,
            overlay: w.overlay,
            top_items: w.top_items,
            editor: w.editor,
            editor_widget,
            agent_widget: w.agent_widget,
            modules: w.modules,
            focus_module: w.focus_module,
            strips: w.strips,
            activate_strip: w.activate_strip,
            agent: w.agent,
            next_session: Cell::new(0),
            active: RefCell::new(None),
            draining: RefCell::new(None),
            numbers: RefCell::new(Vec::new()),
        });
        // Alt-tab away cancels. Connected once, for the window's lifetime; a no-op
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

    /// The window as HINT plans it; `strips`' entries follow the top bar's items as top slots.
    fn layout(&self, strips: &[(ModuleId, gtk4::Widget)]) -> WindowLayout {
        WindowLayout {
            top_visible: self
                .top_items
                .borrow()
                .iter()
                .chain(strips.iter().map(|(_, w)| w))
                .map(visible)
                .collect(),
            modules: (self.modules)()
                .iter()
                .map(|(id, host)| (id.clone(), visible(host)))
                .collect(),
        }
    }

    /// The strip entry a top slot past the top bar's items stands for, in the HINT that is up.
    fn strip_entry(&self, i: usize) -> Option<(ModuleId, gtk4::Widget)> {
        let k = i.checked_sub(self.top_items.borrow().len())?;
        self.active.borrow().as_ref()?.strips.get(k).cloned()
    }

    fn slot_widget(&self, slot: &Slot) -> Option<gtk4::Widget> {
        match slot {
            Slot::Top(i) if *i >= self.top_items.borrow().len() => self.strip_entry(*i).map(|(_, w)| w),
            Slot::Top(i) => self.top_items.borrow().get(*i).cloned(),
            Slot::Module(id) => (self.modules)()
                .into_iter()
                .find(|(m, _)| m == id)
                .map(|(_, host)| host),
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

    /// The same, from `prefix f` (or whichever key the keymap binds to `hint`): that key, `keycode`,
    /// is still down, and its auto-repeat must reach neither a label nor the pane landed on.
    pub(crate) fn toggle_from_prefix(self: &Rc<Self>, keycode: u32) {
        self.toggle_with(Held::Key(keycode));
    }

    fn toggle_with(self: &Rc<Self>, held: Held) {
        if self.is_active() {
            self.cancel();
            return;
        }
        self.stop_draining();
        let session_id = self.next_session.get() + 1;
        self.next_session.set(session_id);
        // A HINT installs a window-level CAPTURE key controller, so while one is open every key in
        // the window reaches nobody else. That is the design, and it is also indistinguishable from
        // the window being broken -- so a session that fails to end looks exactly like "I cannot
        // type any more". Until 2026-09-20 neither the start nor the end said anything, and one
        // such report could not be explained from the log at all. One line each way now.
        println!("[hint] session {session_id} started (held={held:?})");

        let strips = (self.strips)();
        let order = strips_after_modules(order_targets(&self.layout(&strips)), self.top_items.borrow().len());
        let panel_asked = order.ask_panel;
        let restore_focus = gtk4::prelude::GtkWindowExt::focus(&self.window);

        let held = Rc::new(Cell::new(held));
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        let held_on_press = held.clone();
        keys.connect_key_pressed(move |keys, key, keycode, state| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
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
            this.on_key(classify_key(
                key,
                state,
                &same_key_in_every_layout(keys.widget(), keycode),
            ));
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
            // puts focus back where it was, as if nothing had happened.
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
            strips,
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

    /// The panel's two messages. A `hint_targets` for any session but the active one is ignored,
    /// as is a second answer after the timeout already planned without it.
    ///
    /// A `hint_request` only ever starts a HINT, never cancels one. The panel's `f` cannot reach the
    /// panel while a HINT's key controller is attached, so a request that arrives with one up was
    /// posted before it started: a held `f`'s repeat, or `ff` typed faster than this main loop
    /// handled the first. Toggling on it tore down the HINT the first `f` had just opened
    /// The panel also stops sending them itself (`App.tsx::requestHint`).
    pub(crate) fn on_panel(self: &Rc<Self>, message: HintInbound) {
        match message {
            HintInbound::Request if self.is_active() => {}
            HintInbound::Request => self.toggle(),
            HintInbound::Targets { session_id, count } => self.on_targets(session_id, count),
        }
    }

    /// `select.number`: each digit drawn large in the centre of its module, in the HINT labels'
    /// style. Draw-only: no session starts and no controller is added, so the digit typed next goes
    /// to the prefix, which is waiting for it, and nothing here can hold the keys if the wait ends
    /// some way nobody expected -- the worst a missed [`HintCoordinator::hide_numbers`] leaves is a
    /// stale digit on screen. A module that is not mapped gets no digit drawn; its number can still
    /// be typed. Replaces any digits already up. The number of digits drawn.
    ///
    /// `hosts` is every module's host, not HINT's own [`HintWidgets::modules`]: HINT leaves out a
    /// chat that shows a notice instead of a page (it has no labels to collect), but that notice is
    /// still a module on screen whose digit can be typed, so it gets its digit drawn too.
    pub(crate) fn show_numbers(&self, plan: &[(char, ModuleId)], hosts: &[(ModuleId, gtk4::Widget)]) -> usize {
        self.hide_numbers();
        let mut labels = Vec::new();
        for (digit, id) in plan {
            let Some((_, host)) = hosts.iter().find(|(m, _)| m == id) else {
                continue;
            };
            if !visible(host) {
                continue;
            }
            let Some(origin) = host.compute_point(&self.overlay, &graphene::Point::new(0.0, 0.0)) else {
                continue;
            };
            let label = gtk4::Label::new(Some(&digit.to_string()));
            label.add_css_class("hint-label");
            label.add_css_class("hint-number");
            label.set_can_target(false);
            label.set_can_focus(false);
            label.set_halign(gtk4::Align::Start);
            label.set_valign(gtk4::Align::Start);
            // Measured once it is in the overlay, so its CSS (the large size) is what is measured.
            self.overlay.add_overlay(&label);
            let (_, w, ..) = label.measure(gtk4::Orientation::Horizontal, -1);
            let (_, h, ..) = label.measure(gtk4::Orientation::Vertical, -1);
            let x = origin.x().round() as i32 + (host.width() - w) / 2;
            let y = origin.y().round() as i32 + (host.height() - h) / 2;
            label.set_margin_start(x.max(0));
            label.set_margin_top(y.max(0));
            labels.push(label);
        }
        let drawn = labels.len();
        *self.numbers.borrow_mut() = labels;
        drawn
    }

    /// Takes `select.number`'s digits down; nothing if none are up.
    pub(crate) fn hide_numbers(&self) {
        let labels = std::mem::take(&mut *self.numbers.borrow_mut());
        for label in labels {
            self.overlay.remove_overlay(&label);
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
        for (slot, label) in order
            .before
            .iter()
            .zip(&plan.before)
            .chain(order.after.iter().zip(&plan.after))
        {
            let Some(widget) = self.slot_widget(slot) else {
                continue;
            };
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
                    let Some(session) = guard.as_mut().and_then(|a| a.session.as_mut()) else {
                        return;
                    };
                    session.backspace().to_string()
                };
                self.show_prefix(&typed);
            }
            HintKey::Letter(ch) => {
                let step = {
                    let mut guard = self.active.borrow_mut();
                    // Before the plan exists every key is swallowed and ignored.
                    let Some(session) = guard.as_mut().and_then(|a| a.session.as_mut()) else {
                        return;
                    };
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

    /// Moves focus to label `index` and does nothing else.
    fn land(self: &Rc<Self>, index: usize) {
        let (session_id, landing, panel_asked) = {
            let guard = self.active.borrow();
            let Some(active) = guard.as_ref() else { return };
            let Some(landing) = active.plan.as_ref().and_then(|p| active.order.landing(p, index)) else {
                return;
            };
            (active.session_id, landing, active.order.ask_panel)
        };
        match landing {
            Landing::Panel(i) => {
                self.agent_widget.grab_focus();
                // `hint_land` ends the panel's half itself.
                self.agent.hint_land(session_id, i);
            }
            Landing::Gtk(slot) => {
                match &slot {
                    Slot::Module(id) => {
                        (self.focus_module)(id);
                    }
                    // A strip entry: as if it were clicked, which chooses its tab.
                    Slot::Top(i) if *i >= self.top_items.borrow().len() => {
                        if let Some((id, _)) = self.strip_entry(*i) {
                            (self.activate_strip)(&id);
                        }
                    }
                    Slot::Top(_) => {
                        if let Some(w) = self.slot_widget(&slot) {
                            w.grab_focus();
                        }
                    }
                }
                // The panel may be showing labels too; a GTK landing must clear them.
                if panel_asked {
                    self.agent.hint_end(session_id);
                }
            }
        }
        self.end(false);
    }

    /// Every cancel path: labels gone, focus back where it was.
    fn cancel(&self) {
        let Some((session_id, panel_asked)) = self.active.borrow().as_ref().map(|a| (a.session_id, a.order.ask_panel))
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
        let Some(mut active) = self.active.borrow_mut().take() else {
            return;
        };
        println!(
            "[hint] session {} ended (restore_focus={restore}, labels={})",
            active.session_id,
            active.labels.len(),
        );
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
                Some(w) if Some(&w) == self.editor_widget.as_ref() => {
                    if let Some(editor) = &self.editor {
                        editor.grab_focus();
                    }
                }
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

    /// `Esc` cancels; `Ctrl+Shift+F` is no longer the HINT's trigger, so it is a chord
    /// like any other and does nothing.
    #[test]
    fn escape_cancels_and_ctrl_shift_f_is_just_a_chord() {
        assert_eq!(classify_key(Key::Escape, NONE, &[]), HintKey::Cancel);
        let chord = ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK;
        assert_eq!(classify_key(Key::F, chord, &[]), HintKey::Nothing);
        assert_eq!(classify_key(Key::f, chord, &[]), HintKey::Nothing);
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
        assert_eq!(
            classify_key(Key::A, ModifierType::SHIFT_MASK, &[]),
            HintKey::Letter('a')
        );
    }

    /// On a Cyrillic layout the key where `a` sits types `ф`: its Latin letter is found in the
    /// keyboard's other layout. With no Latin letter anywhere it stays itself, and `HintSession`
    /// ignores it.
    #[test]
    fn a_non_latin_layout_types_the_latin_letter_on_the_same_key() {
        let same_key = [Key::Cyrillic_ef, Key::a];
        assert_eq!(classify_key(Key::Cyrillic_ef, NONE, &same_key), HintKey::Letter('a'));
        assert_eq!(
            classify_key(Key::Cyrillic_EF, ModifierType::LOCK_MASK, &same_key),
            HintKey::Letter('a')
        );
        assert_eq!(
            classify_key(Key::Cyrillic_ef, NONE, &[Key::Cyrillic_ef]),
            HintKey::Letter('ф')
        );
    }

    /// A non-label key does nothing: not a cancel, not a letter.
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
        assert_eq!(
            label_markup("as", "a"),
            ("<span alpha=\"45%\">a</span>s".to_string(), true)
        );
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

    /// Strip entries are labelled after every module (and after the chat's own labels): the
    /// modules keep the labels they had before a container was tabbed. Landing on one is the same
    /// slot it was planned as, so the entry is found by its place among the strips.
    #[test]
    fn strip_entries_are_labelled_after_the_modules() {
        // Two top-bar items, then two strip entries (the second not on screen).
        let layout = WindowLayout {
            top_visible: vec![true, true, true, false],
            modules: vec![(ModuleId::editor(), true), (ModuleId::terminal(), true)],
        };
        let order = strips_after_modules(order_targets(&layout), 2);
        assert_eq!(
            order.before,
            [
                Slot::Top(0),
                Slot::Top(1),
                Slot::Module(ModuleId::editor()),
                Slot::Module(ModuleId::terminal())
            ]
        );
        assert_eq!(order.after, [Slot::Top(2)], "the entry on screen, last");
        let plan = order.plan(0);
        assert_eq!(order.landing(&plan, 4), Some(Landing::Gtk(Slot::Top(2))));

        // With the chat on screen its labels sit between; the strips still come after it all.
        let layout = WindowLayout {
            top_visible: vec![true, true],
            modules: vec![
                (ModuleId::editor(), true),
                (ModuleId::agent(), true),
                (ModuleId::terminal(), true),
            ],
        };
        let order = strips_after_modules(order_targets(&layout), 1);
        assert_eq!(order.before, [Slot::Top(0), Slot::Module(ModuleId::editor())]);
        assert_eq!(order.after, [Slot::Module(ModuleId::terminal()), Slot::Top(1)]);

        // No strips: the order is exactly what it was.
        let layout = WindowLayout {
            top_visible: vec![true],
            modules: vec![(ModuleId::editor(), true)],
        };
        assert_eq!(strips_after_modules(order_targets(&layout), 1), order_targets(&layout));
    }

    /// `prefix f`: the prefix saw the press, so the HINT starts already holding that key by
    /// keycode. Its repeats reach nobody; after it comes up, it is a key again.
    #[test]
    fn a_hint_from_the_prefix_holds_the_key_that_started_it() {
        let mut held = Held::Key(F);
        assert!(held.press(F, Key::f));
        held.release(F, Key::f);
        assert!(!held.press(F, Key::f), "pressed again after release, it is a key again");
    }
}
