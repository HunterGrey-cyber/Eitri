//! Window mode transitions and synchronization with the editor and compositor.
//!
//! This module's `WindowMode` enum is the shell's mode. The distinct `super::WindowMode`
//! struct in `window.rs` holds the controller's per-window mode flags.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMode {
    Windowed,
    Fullscreen,
    /// `from_fullscreen` is where `prefix F11` goes back to: it undoes itself.
    Immersive {
        from_fullscreen: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeEvent {
    /// `F11`: fullscreen on or off. From immersive it is off, straight to windowed.
    ToggleFullscreen,
    /// `prefix F11`.
    ToggleImmersive,
    /// `g:neovide_fullscreen` changed in nvim.
    Setting(bool),
    /// The window's own fullscreen state changed for a reason this module did not cause.
    Window(bool),
}

impl WindowMode {
    /// Toggles undo themselves, while external changes only alter a disagreeing mode.
    pub fn next(self, event: ModeEvent) -> WindowMode {
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

    pub fn is_fullscreen(self) -> bool {
        !matches!(self, WindowMode::Windowed)
    }

    pub fn shows_top_bar(self) -> bool {
        !matches!(self, WindowMode::Immersive { .. })
    }

    /// Minimize/maximize/close are only for a window that is not fullscreen.
    pub fn shows_window_controls(self) -> bool {
        matches!(self, WindowMode::Windowed)
    }
}

/// What to write to `g:neovide_fullscreen` so it matches `mode`, given what it holds as far as this
/// window knows (`var_model`). `None` when it already matches: never writing an equal value is what
/// keeps the write and nvim's report of it from bouncing.
pub fn setting_to_write(var_model: bool, mode: WindowMode) -> Option<bool> {
    (var_model != mode.is_fullscreen()).then_some(mode.is_fullscreen())
}

/// What a `notify::fullscreened` means, given the state we last ASKED the compositor for.
///
/// A fullscreen report confirms a compositor round trip, so it can answer a request two toggles
/// old. Compare it with the outstanding request before treating it as an external change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowNotify {
    /// The answer to our own outstanding request. The mode already says this; stop waiting.
    Confirmed,
    /// A confirmation that does not answer our latest request -- an older request's answer still in
    /// flight, or a refusal. Ignored: acting on it would undo a decision the user has already made.
    Stale,
    /// Nothing of ours is outstanding and the window disagrees with the mode: the compositor (or a
    /// desktop shortcut) did this, and the mode follows it.
    External,
}

pub fn classify_window_notify(requested: Option<bool>, mode_is_fullscreen: bool, is: bool) -> WindowNotify {
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
pub fn is_our_echo(pending_echo: Option<bool>, value: bool) -> bool {
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

    /// With two quick toggles the second press must still be issued, and the
    /// first request's confirmation must not undo it.
    #[test]
    fn a_confirmation_answering_an_older_request_does_not_move_the_mode() {
        // press 1: requested true. press 2 compares against the REQUEST, so it is issued.
        assert_eq!(classify_window_notify(Some(false), false, true), WindowNotify::Stale);
        // ...and once the compositor catches up with the latest request, it is just confirmed.
        assert_eq!(
            classify_window_notify(Some(false), false, false),
            WindowNotify::Confirmed
        );
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
        assert!(
            !is_our_echo(Some(true), false),
            "the user set it the other way while ours was in flight"
        );
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
