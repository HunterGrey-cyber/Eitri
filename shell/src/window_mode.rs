//! Window modes: windowed, fullscreen (the top bar stays, the window buttons go), and immersive
//! (the top bar goes too, and `Ctrl+k` shows it for as long as it has focus). Spec:
//! docs/superpowers/specs/2026-09-19-window-modes-design.md §2.
//!
//! The mode also follows `g:neovide_fullscreen` both ways (§2.4): a `:let`, a mapping or `init.lua`
//! setting it moves the window, and a window that changed for a reason nvim did not see (`F11`, the
//! compositor) writes it back, so the variable always says what the window is doing.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use neovide_editor::NeovideEditorPane;

use crate::chrome::TopBar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowMode {
    Windowed,
    Fullscreen,
    /// `from_fullscreen` is where `Ctrl+Shift+F11` goes back to: it undoes itself (§2.2).
    Immersive { from_fullscreen: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeEvent {
    /// `F11`: fullscreen on or off. From immersive it is off, straight to windowed.
    ToggleFullscreen,
    /// `Ctrl+Shift+F11`.
    ToggleImmersive,
    /// `g:neovide_fullscreen` changed in nvim.
    Setting(bool),
    /// The window's own fullscreen state changed for a reason this module did not cause.
    Window(bool),
}

impl WindowMode {
    /// The table in spec §2.2, cell for cell.
    pub(crate) fn next(self, event: ModeEvent) -> WindowMode {
        use ModeEvent::*;
        use WindowMode::*;
        match (self, event) {
            (Windowed, ToggleFullscreen) => Fullscreen,
            (Fullscreen | Immersive { .. }, ToggleFullscreen) => Windowed,
            (Windowed, ToggleImmersive) => Immersive { from_fullscreen: false },
            (Fullscreen, ToggleImmersive) => Immersive { from_fullscreen: true },
            (Immersive { from_fullscreen: true }, ToggleImmersive) => Fullscreen,
            (Immersive { from_fullscreen: false }, ToggleImmersive) => Windowed,
            (Windowed, Setting(true) | Window(true)) => Fullscreen,
            (Fullscreen | Immersive { .. }, Setting(false) | Window(false)) => Windowed,
            (mode, Setting(_) | Window(_)) => mode,
        }
    }

    pub(crate) fn is_fullscreen(self) -> bool {
        !matches!(self, WindowMode::Windowed)
    }

    pub(crate) fn shows_top_bar(self) -> bool {
        !matches!(self, WindowMode::Immersive { .. })
    }

    /// Minimize/maximize/close are only for a window that is not fullscreen (§2.1).
    pub(crate) fn shows_window_controls(self) -> bool {
        matches!(self, WindowMode::Windowed)
    }
}

/// What to write to `g:neovide_fullscreen` so it matches `mode`, given what it holds as far as this
/// window knows (`var_model`). `None` when it already matches: never writing an equal value is what
/// keeps the write and nvim's report of it from bouncing (§5 invariant 2).
pub(crate) fn setting_to_write(var_model: bool, mode: WindowMode) -> Option<bool> {
    (var_model != mode.is_fullscreen()).then_some(mode.is_fullscreen())
}

/// The live window's mode. One per window; `install` wires it and returns it.
pub(crate) struct WindowModes {
    window: gtk4::ApplicationWindow,
    top_bar: gtk4::Widget,
    controls: gtk4::Widget,
    /// `None` when a Lua plugin replaced the editor in the main slot: there is no nvim to sync
    /// with, and `F11` still works (§2.4).
    editor: Option<Rc<NeovideEditorPane>>,
    /// Moves focus back to the pane that last had it; used when the top bar hides while it holds
    /// focus, since GTK would otherwise leave the keys on an invisible button.
    return_to_pane: Rc<dyn Fn()>,
    mode: Cell<WindowMode>,
    /// What `g:neovide_fullscreen` holds, as far as this window knows: the last value read back
    /// from nvim or written to it. Neovide's own default is `false`.
    var_model: Cell<bool>,
    /// Immersive only: the top bar is showing because `Ctrl+k` asked for it (§2.5).
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

        // App-level, so GTK takes them before nvim or the panel sees the key (spec §2.3; the same
        // capture-phase shortcut manager as HINT's `Ctrl+Shift+F`).
        for (name, accel, event) in [
            ("fullscreen", "F11", ModeEvent::ToggleFullscreen),
            ("immersive", "<Control><Shift>F11", ModeEvent::ToggleImmersive),
        ] {
            let action = gtk4::gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(&this);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.apply(event);
                }
            });
            app.add_action(&action);
            app.set_accels_for_action(&format!("app.{name}"), &[accel]);
        }

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

        // A revealed top bar hides again the moment focus leaves it (§2.5).
        let weak = Rc::downgrade(&this);
        window.connect_notify_local(Some("focus-widget"), move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            if this.revealed.get() && !this.focus_in_top_bar() {
                this.revealed.set(false);
                this.sync_widgets();
            }
        });

        this
    }

    fn focus_in_top_bar(&self) -> bool {
        gtk4::prelude::GtkWindowExt::focus(&self.window)
            .is_some_and(|w| w == self.top_bar || w.is_ancestor(&self.top_bar))
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
        let show_bar = mode.shows_top_bar() || self.revealed.get();
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
}

/// What a `notify::fullscreened` means, given the state we last ASKED the compositor for.
///
/// `gtk_window_is_fullscreen` reports the compositor's confirmation, never the request: on a mapped
/// window `gtk_window_fullscreen` only builds a `GdkToplevelLayout` and asks (GTK 4.22.5,
/// `gtkwindow.c`), and `priv->fullscreen` is assigned in `surface_state_changed`, a round trip
/// later. So a confirmation can arrive that answers a request two toggles old.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowNotify {
    /// The answer to our own outstanding request. The mode already says this; stop waiting.
    Confirmed,
    /// A confirmation that does not answer our latest request -- an older request's answer still in
    /// flight, or a refusal. Ignored: acting on it would undo a decision the user has already made.
    Stale,
    /// Nothing of ours is outstanding and the window disagrees with the mode: the compositor (or a
    /// desktop shortcut) did this, and the mode follows it.
    External,
}

pub(crate) fn classify_window_notify(requested: Option<bool>, mode_is_fullscreen: bool, is: bool) -> WindowNotify {
    match requested {
        Some(want) if want == is => WindowNotify::Confirmed,
        Some(_) => WindowNotify::Stale,
        None if is != mode_is_fullscreen => WindowNotify::External,
        None => WindowNotify::Confirmed,
    }
}

/// Whether a `g:neovide_fullscreen` report is this window's own write coming back.
///
/// Every write we make is reported to us again, because it reaches nvim and nvim's watcher reports
/// it like any `:let`. Treating that echo as a fresh external decision is how a window would act on
/// its own past self.
pub(crate) fn is_our_echo(pending_echo: Option<bool>, value: bool) -> bool {
    pending_echo == Some(value)
}

#[cfg(test)]
mod tests {
    use super::ModeEvent::*;
    use super::WindowMode::*;
    use super::*;

    const IMM_W: WindowMode = Immersive { from_fullscreen: false };
    const IMM_F: WindowMode = Immersive { from_fullscreen: true };

    #[test]
    fn f11_is_fullscreen_on_and_off_and_leaves_immersive_for_windowed() {
        assert_eq!(Windowed.next(ToggleFullscreen), Fullscreen);
        assert_eq!(Fullscreen.next(ToggleFullscreen), Windowed);
        assert_eq!(IMM_W.next(ToggleFullscreen), Windowed);
        assert_eq!(IMM_F.next(ToggleFullscreen), Windowed);
    }

    #[test]
    fn ctrl_shift_f11_enters_immersive_and_undoes_itself() {
        assert_eq!(Windowed.next(ToggleImmersive), IMM_W);
        assert_eq!(Fullscreen.next(ToggleImmersive), IMM_F);
        assert_eq!(IMM_W.next(ToggleImmersive), Windowed);
        assert_eq!(IMM_F.next(ToggleImmersive), Fullscreen);
    }

    #[test]
    fn the_variable_and_the_compositor_move_the_mode_only_when_they_disagree_with_it() {
        for external in [Setting as fn(bool) -> ModeEvent, Window] {
            assert_eq!(Windowed.next(external(true)), Fullscreen);
            assert_eq!(Windowed.next(external(false)), Windowed);
            assert_eq!(Fullscreen.next(external(true)), Fullscreen);
            assert_eq!(Fullscreen.next(external(false)), Windowed);
            assert_eq!(IMM_W.next(external(true)), IMM_W);
            assert_eq!(IMM_F.next(external(false)), Windowed);
        }
    }

    #[test]
    fn what_each_mode_shows() {
        assert!(Windowed.shows_top_bar() && Windowed.shows_window_controls() && !Windowed.is_fullscreen());
        assert!(Fullscreen.shows_top_bar() && !Fullscreen.shows_window_controls() && Fullscreen.is_fullscreen());
        assert!(!IMM_W.shows_top_bar() && !IMM_W.shows_window_controls() && IMM_W.is_fullscreen());
    }

    /// The double-toggle the review reproduced: the second press must still be issued, and the
    /// first request's confirmation must not undo it.
    #[test]
    fn a_confirmation_answering_an_older_request_does_not_move_the_mode() {
        // press 1: requested true. press 2 compares against the REQUEST, so it is issued.
        assert_eq!(classify_window_notify(Some(false), false, true), WindowNotify::Stale);
        // ...and once the compositor catches up with the latest request, it is just confirmed.
        assert_eq!(classify_window_notify(Some(false), false, false), WindowNotify::Confirmed);
    }

    #[test]
    fn a_change_nobody_here_asked_for_moves_the_mode() {
        assert_eq!(classify_window_notify(None, true, false), WindowNotify::External);
        assert_eq!(classify_window_notify(None, false, true), WindowNotify::External);
        // Nothing outstanding and nothing to correct.
        assert_eq!(classify_window_notify(None, true, true), WindowNotify::Confirmed);
    }

    /// A window must not act on its own write coming back through nvim's watcher.
    #[test]
    fn our_own_write_is_recognised_and_a_real_let_is_not() {
        assert!(is_our_echo(Some(true), true));
        assert!(!is_our_echo(Some(true), false), "the user set it the other way while ours was in flight");
        assert!(!is_our_echo(None, true), "nothing of ours outstanding: a real :let");
    }

    #[test]
    fn the_variable_is_written_only_when_it_differs() {
        assert_eq!(setting_to_write(false, Fullscreen), Some(true));
        assert_eq!(setting_to_write(true, Fullscreen), None);
        assert_eq!(setting_to_write(true, IMM_W), None);
        assert_eq!(setting_to_write(true, Windowed), Some(false));
        assert_eq!(setting_to_write(false, Windowed), None);
    }
}
