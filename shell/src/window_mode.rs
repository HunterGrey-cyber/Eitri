//! Window modes: windowed, fullscreen (the top bar stays, the window buttons go),
//! and immersive (the top bar also hides; `Ctrl+k` shows it while it has focus).
//!
//! The mode follows `g:neovide_fullscreen` in both directions. A `:let`, mapping or
//! `init.lua` change moves the window. Changes from `F11` or the compositor are
//! written back so the variable reflects the window's fullscreen state.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use neovide_editor::NeovideEditorPane;

use crate::chrome::TopBar;

use eitri_core::wm::modes::{
    classify_window_notify, is_our_echo, setting_to_write, ModeEvent, WindowMode, WindowNotify,
};

/// The live window's mode. One per window; `install` wires it and returns it.
pub(crate) struct WindowModes {
    window: gtk4::ApplicationWindow,
    top_bar: gtk4::Widget,
    controls: gtk4::Widget,
    /// `None` when there is no nvim to sync with; `F11` still works.
    /// Every window has an editor, which a Lua `main` panel hides rather than replaces,
    /// so `main.rs` always passes `Some`.
    editor: Option<Rc<NeovideEditorPane>>,
    /// Moves focus back to the pane that last had it; used when the top bar hides while it holds
    /// focus, since GTK would otherwise leave the keys on an invisible button.
    return_to_pane: Rc<dyn Fn()>,
    mode: Cell<WindowMode>,
    /// What `g:neovide_fullscreen` holds, as far as this window knows: the last value read back
    /// from nvim or written to it. Neovide's own default is `false`.
    var_model: Cell<bool>,
    /// Immersive only: the top bar is showing because `Ctrl+k` requested it.
    revealed: Cell<bool>,
    /// The fullscreen state last ASKED of the compositor and not yet confirmed. The request, never
    /// `window.is_fullscreen()`, is what a second toggle compares against -- see
    /// `classify_window_notify` for why the confirmation cannot be trusted as "where we are".
    requested: Cell<Option<bool>>,
    /// The `g:neovide_fullscreen` value we last wrote and have not yet seen reported back
    /// (`is_our_echo`).
    pending_echo: Cell<Option<bool>>,
}

impl WindowModes {
    pub(crate) fn install(
        app: &gtk4::Application,
        window: &gtk4::ApplicationWindow,
        top_bar: &TopBar,
        editor: Option<Rc<NeovideEditorPane>>,
        return_to_pane: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let this = Rc::new(WindowModes {
            window: window.clone(),
            top_bar: top_bar.widget.clone(),
            controls: top_bar.controls.clone(),
            editor,
            return_to_pane,
            mode: Cell::new(WindowMode::Windowed),
            var_model: Cell::new(false),
            revealed: Cell::new(false),
            requested: Cell::new(None),
            pending_echo: Cell::new(None),
        });

        // App-level, so GTK takes it before nvim or the panel sees the key. Immersive has no
        // accelerator: it is `prefix F11`, which calls `toggle_immersive`.
        let action = gtk4::gio::SimpleAction::new("fullscreen", None);
        let weak = Rc::downgrade(&this);
        action.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.apply(ModeEvent::ToggleFullscreen);
            }
        });
        app.add_action(&action);
        let accels: Vec<String> = eitri_core::keymap::root::chords_of("fullscreen")
            .iter()
            .map(|root| crate::gtk_host::keys::gtk_accel(&root.chord()))
            .collect();
        app.set_accels_for_action("app.fullscreen", &accels.iter().map(String::as_str).collect::<Vec<_>>());

        // The compositor answering. Which of the three things it can mean is `classify_window_notify`'s
        // job; the one that moves the mode is a change nothing here asked for.
        let weak = Rc::downgrade(&this);
        window.connect_notify_local(Some("fullscreened"), move |window, _| {
            let Some(this) = weak.upgrade() else { return };
            let is = window.is_fullscreen();
            match classify_window_notify(this.requested.get(), this.mode.get().is_fullscreen(), is) {
                WindowNotify::Confirmed => this.requested.set(None),
                WindowNotify::Stale => {}
                WindowNotify::External => this.apply(ModeEvent::Window(is)),
            }
        });

        if let Some(editor) = &this.editor {
            let weak = Rc::downgrade(&this);
            editor.on_fullscreen_setting(move |value| {
                let Some(this) = weak.upgrade() else { return };
                this.var_model.set(value);
                // Our own write coming back is not a decision: acting on it would let a window
                // drive itself from its own past state.
                if is_our_echo(this.pending_echo.get(), value) {
                    this.pending_echo.set(None);
                    return;
                }
                this.pending_echo.set(None);
                this.apply(ModeEvent::Setting(value));
            });
        }

        // A revealed top bar hides again when focus leaves it.
        let weak = Rc::downgrade(&this);
        window.connect_notify_local(Some("focus-widget"), move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            this.conceal_top_bar();
        });

        this
    }

    fn focus_in_top_bar(&self) -> bool {
        gtk4::prelude::GtkWindowExt::focus(&self.window)
            .is_some_and(|w| w == self.top_bar || w.is_ancestor(&self.top_bar))
    }

    /// `prefix F11`: immersive on or off.
    pub(crate) fn toggle_immersive(&self) {
        self.apply(ModeEvent::ToggleImmersive);
    }

    fn apply(&self, event: ModeEvent) {
        let next = self.mode.get().next(event);
        self.mode.set(next);
        if !matches!(next, WindowMode::Immersive { .. }) {
            self.revealed.set(false);
        }
        // Against the last REQUEST, falling back to the compositor's answer only when nothing of
        // ours is outstanding. Comparing against `is_fullscreen()` dropped the second of two quick
        // toggles, and the first request's confirmation then undid the user's own last press.
        let want = next.is_fullscreen();
        let current = self.requested.get().unwrap_or_else(|| self.window.is_fullscreen());
        if current != want {
            self.requested.set(Some(want));
            if want {
                self.window.fullscreen();
            } else {
                self.window.unfullscreen();
            }
        }
        self.sync_widgets();
        if let (Some(editor), Some(value)) = (&self.editor, setting_to_write(self.var_model.get(), next)) {
            editor.set_fullscreen_setting(value);
            self.var_model.set(value);
            self.pending_echo.set(Some(value));
        }
        println!("[window_mode] {event:?} -> {next:?}");
    }

    fn sync_widgets(&self) {
        let mode = self.mode.get();
        // A bar that hangs in no window (a chat or page window builds one for its hooks but never shows it) stays
        // hidden whatever the mode or a reveal asks.
        let show_bar = self.top_bar.parent().is_some() && (mode.shows_top_bar() || self.revealed.get());
        if !show_bar && self.focus_in_top_bar() {
            (self.return_to_pane)();
        }
        self.top_bar.set_visible(show_bar);
        self.controls.set_visible(mode.shows_window_controls());
    }

    /// Shows the top bar in immersive mode so `Ctrl+k` can focus it; a no-op otherwise, where it is
    /// already showing. Call BEFORE `grab_focus`: GTK will focus a hidden widget without complaint.
    pub(crate) fn reveal_top_bar(&self) {
        if matches!(self.mode.get(), WindowMode::Immersive { .. }) {
            self.revealed.set(true);
            self.sync_widgets();
        }
    }

    /// Hides a revealed top bar again unless the keys are in it: what the keys leaving the bar do, and
    /// what a reveal made without moving the keys there (the workspace chooser's strip) needs once
    /// it is no longer wanted, since no focus change will come to hide it.
    pub(crate) fn conceal_top_bar(&self) {
        if self.revealed.get() && !self.focus_in_top_bar() {
            self.revealed.set(false);
            self.sync_widgets();
        }
    }
}
