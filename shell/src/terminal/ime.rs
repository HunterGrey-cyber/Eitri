//! The terminal's input method (bottom-terminal phase 2): what fcitx5/rime says, and what of it
//! reaches the shell.
//!
//! The pattern is `neovide-editor/src/keyboard.rs`'s (`74010ed`, finished there, candidate placement
//! included): a `GtkIMMulticontext` whose client widget is the pane, attached to the pane's key
//! controller with `set_im_context`, so GTK runs the input method on every key BEFORE `key-pressed`
//! and a key it consumes (a composition in progress) never reaches the terminal's own key path.
//! Composed text arrives through `commit`; the pane draws the unfinished composition itself
//! (`preedit-changed`, `neovibe_terminal::layout_preedit`) and tells the input method where it is
//! (`set_cursor_location`). The GTK wiring is `pane.rs`'s; the decisions are here, pure, and tested
//! with synthetic signals.
//!
//! **A commit reaches the shell only while the terminal holds the keys, and never out of the pane's
//! own reset.** neovibe's keys are taken before the input method sees them -- the `Ctrl+a` prefix,
//! `Ctrl+h/j/k/l`, HINT and the app accelerators are all capture phase -- so a composition can be
//! cut off by anything that moves the keys elsewhere: `Ctrl+k`, `Ctrl+a t`, a HINT label, a click in
//! another module, alt-tab. **GTK then focuses the input method out itself, from inside the focus
//! change, before `pane_focus` hears that the keys left** (GTK 4.22.5): the key controller owns its
//! input method's focus since `set_im_context` (`gtkeventcontrollerkey.c:142-177`), and
//! `gtk_window_root_set_focus` clears the old widget's `has_focus` and runs that crossing before it
//! notifies `focus-widget` (`gtkwindow.c:2252-2270`), as `_gtk_window_set_is_active` clears
//! `is_active` and crosses before it notifies `is-active` (`:6161-6186`). fcitx5-gtk 5.1.7 commits its
//! composition right there, synchronously (`fcitximcontext.cpp:796-807`: committing on focus-out is
//! the client's job, `ClientUnfocusCommit`), and rime's default composition carries nothing that
//! keeps it back, so `nihao` then `Ctrl+k` commits `ni hao`. So [`ImeGate::commit`] is told what GTK
//! says at that moment ([`GtkFocus`]: the widget's own `has_focus` and its window's `is_active`, one
//! of them already false -- which one depends on how the keys left), and not only what `pane_focus`
//! last reported; any of the three saying no drops the commit. That text is
//! half-typed pinyin, or a candidate the owner never chose: garbage to a shell. **Dropping it is
//! neovibe's choice, not fcitx5's convention:** in his other GTK4 apps the same click away inserts
//! the raw `ni hao` (the plan's owner decision 1). The same holds while the pane resets the input
//! method for a reason of its own: a paste or a `Ctrl+a` literal cutting into a composition discards
//! it, then goes to the shell (fcitx5-gtk commits out of `reset` too, `fcitximcontext.cpp:1119`).
//!
//! **What asking GTK covers:** every focus-out GTK itself starts, which is every way the keys leave
//! (neovibe's own moves, a click, alt-tab). Not one the fcitx5 server starts: its `NotifyFocusOut`
//! (fcitx5 5.1.22, `dbusfrontend.cpp:626-634`) makes fcitx5-gtk commit if it still believes it has
//! focus (`fcitximcontext.cpp:708-715`). That reaches the shell only if the server focuses the
//! terminal's input context out while GTK says the terminal has the keys, or if a stale one meets
//! pinyin typed within a D-Bus round trip of the keys coming back. GTK cannot see either, so neither
//! can the gate; neither is expected in practice.
//!
//! **What a commit becomes:** its text, byte for byte and never bracketed (`keys::normalize_commit`):
//! a committed string is what the owner typed. **With an input method attached -- any GTK module,
//! fcitx5 running or not -- that is every printable key it does not compose**; see the test
//! `a_printable_key_arriving_as_a_commit_keeps_its_text_and_loses_what_only_a_key_says` for why, and
//! for what that costs a program that asked for more than text. And a key the input method binds
//! never reaches the shell at all (the plan's owner decision 11).
//!
//! **The caret is the input method's.** fcitx5-rime puts it at the composition's FIRST cell by
//! default on Linux (`PreeditCursorPositionAtBeginning`, so its candidate window stays put while he
//! types), and the pane follows it: the beam and the candidate window sit there, as in his other
//! apps. The pango attributes that come with a preedit (rime's `HighLight` on the segment being
//! converted) are not drawn.

use gtk4::gdk::Rectangle;
use terminal_input::NormalizedInput;

use super::keys::normalize_commit;

/// The composition the input method is showing: its text and its caret, in characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Preedit {
    pub(crate) text: String,
    pub(crate) caret: usize,
}

/// What becomes of a `commit`.
#[derive(Debug, PartialEq)]
pub(crate) enum Commit {
    /// To the shell, through the pane's one door (`pane::submit`).
    Send(NormalizedInput),
    /// Nothing reaches the shell; the reason, for the log.
    Dropped(&'static str),
}

/// What GTK itself says at the moment of a commit: the terminal widget's own `has_focus`, and its
/// window's `is_active`. **Both, because GTK clears them at different moments** (4.22.5): a focus move
/// inside the window clears `has_focus` BEFORE the crossing that focuses the input method out
/// (`gtk_window_root_set_focus`, `gtkwindow.c:2252-2254`), but alt-tab clears `is_active` before its
/// crossing and `has_focus` only AFTER it (`_gtk_window_set_is_active`, `:6161`, `:6176-6177`). So
/// `has_focus` alone lets the half-typed pinyin through on alt-tab, and `is_active` alone lets it
/// through on `Ctrl+k`; each has its own test. `Default` is "neither": what the pane reports when
/// its widget is gone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GtkFocus {
    pub(crate) widget_has_focus: bool,
    pub(crate) window_is_active: bool,
}

/// What the pane must do after a focus report. GTK's key controller has already focused the input
/// method in or out by then; this is what is left for the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusChange {
    /// Point the input method at the cursor, before the first key of a composition.
    In,
    /// `reset`: whatever survives a focus-out in the client (a dead-key sequence) is discarded.
    Out,
    /// Nothing: the same answer as last time.
    Unchanged,
}

/// Holds what the input method has said and decides what reaches the shell.
#[derive(Debug, Default)]
pub(crate) struct ImeGate {
    /// The terminal holds the keys, as `pane_focus` last reported it ("has the keys": focus AND an
    /// active window). It hears of a change AFTER GTK's own focus-out, so a commit is also checked
    /// against GTK's answer at that moment ([`Self::commit`]'s [`GtkFocus`]).
    focused: bool,
    /// Inside the pane's own `IMContext::reset` call ([`Self::begin_reset`]).
    resetting: bool,
    preedit: Option<Preedit>,
}

impl ImeGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `pane_focus`'s report for the terminal. Losing the keys discards the composition here too, so
    /// it is off the screen even if the input method never says it ended.
    pub(crate) fn set_focused(&mut self, focused: bool) -> FocusChange {
        if focused == self.focused {
            return FocusChange::Unchanged;
        }
        self.focused = focused;
        if focused {
            FocusChange::In
        } else {
            self.preedit = None;
            FocusChange::Out
        }
    }

    /// The input method committed `text`. `gtk` is GTK's own answer at this moment ([`GtkFocus`]),
    /// half of which is already "no" when the commit comes out of GTK's focus-out of the input
    /// method, before `pane_focus` reports (the module doc). That, or the last report saying no,
    /// drops the commit. Any commit ends the composition.
    pub(crate) fn commit(&mut self, text: &str, gtk: GtkFocus) -> Commit {
        self.preedit = None;
        if !self.focused || !gtk.widget_has_focus || !gtk.window_is_active {
            return Commit::Dropped("the terminal does not hold the keys");
        }
        if self.resetting {
            return Commit::Dropped("the composition was discarded");
        }
        if text.is_empty() {
            return Commit::Dropped("it was empty");
        }
        Commit::Send(normalize_commit(text))
    }

    /// `preedit-start`, `-changed` or `-end`, with GTK's `preedit_string()`: the text and its caret
    /// in characters. An empty text is no composition. Ignored unless the terminal holds the keys
    /// and the pane is not resetting.
    pub(crate) fn preedit_changed(&mut self, text: &str, caret: i32) {
        if !self.focused || self.resetting || text.is_empty() {
            self.preedit = None;
            return;
        }
        let chars = text.chars().count();
        self.preedit = Some(Preedit {
            text: text.to_string(),
            caret: usize::try_from(caret).unwrap_or(0).min(chars),
        });
    }

    pub(crate) fn preedit(&self) -> Option<&Preedit> {
        self.preedit.as_ref()
    }

    pub(crate) fn composing(&self) -> bool {
        self.preedit.is_some()
    }

    /// The pane is about to call `IMContext::reset` to discard a composition: until
    /// [`Self::end_reset`], a commit is dropped and a preedit is ignored.
    pub(crate) fn begin_reset(&mut self) {
        self.resetting = true;
        self.preedit = None;
    }

    pub(crate) fn end_reset(&mut self) {
        self.resetting = false;
    }
}

/// The rectangle `IMContext::set_cursor_location` wants for one terminal cell: widget-local LOGICAL
/// pixels, where `cell` is `TerminalMetrics::cell_rect`'s DEVICE pixels -- the metrics are built at
/// the GLArea's device-pixel allocation and its scale. The same one division by GTK's integer
/// `scale_factor` the editor makes (`neovide-editor/src/keyboard.rs`, `im_cursor_rect_for_editor`),
/// and the same rule that a cell is never reported smaller than one pixel. The pane paints from its
/// own top-left, so there is no origin to add.
pub(crate) fn im_cursor_rect(cell: skia_safe::Rect, scale_factor: i32) -> Rectangle {
    let scale = scale_factor.max(1) as f32;
    Rectangle::new(
        (cell.left / scale).round() as i32,
        (cell.top / scale).round() as i32,
        (cell.width() / scale).ceil().max(1.0) as i32,
        (cell.height() / scale).ceil().max(1.0) as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::TermMode;

    /// GTK's answer while the owner types into the terminal: the widget has focus, the window is
    /// active.
    const TYPING: GtkFocus = GtkFocus {
        widget_has_focus: true,
        window_is_active: true,
    };

    fn focused() -> ImeGate {
        let mut gate = ImeGate::new();
        assert_eq!(gate.set_focused(true), FocusChange::In);
        gate
    }

    fn sent(commit: Commit) -> NormalizedInput {
        match commit {
            Commit::Send(input) => input,
            Commit::Dropped(why) => panic!("expected the commit to reach the shell, dropped: {why}"),
        }
    }

    /// The P4 pass's own sequence (`docs/canonical/neovibe_feasibility_status.md`, "P4 结论(续)"):
    /// rime's preedit went `n`, `ni`, `ni h`, `ni ha`, `ni hao`, and Space committed 你好. The bytes
    /// at the PTY are the GUI checklist's oracle, `e4 bd a0 e5 a5 bd` -- checked with bracketed paste
    /// ON, as his zsh runs, because a commit is typing and must not arrive bracketed.
    #[test]
    fn nihao_then_space_commits_the_bytes_the_gui_pass_reads_at_the_pty() {
        let mut gate = focused();
        for (text, caret) in [("n", 1), ("ni", 2), ("ni h", 4), ("ni ha", 5), ("ni hao", 6)] {
            gate.preedit_changed(text, caret);
        }
        assert_eq!(
            gate.preedit(),
            Some(&Preedit {
                text: "ni hao".to_string(),
                caret: 6
            })
        );
        let input = sent(gate.commit("\u{4f60}\u{597d}", TYPING));
        assert_eq!(
            terminal_input::encode(&input, TermMode::BRACKETED_PASTE),
            vec![0xe4, 0xbd, 0xa0, 0xe5, 0xa5, 0xbd]
        );
        assert!(!gate.composing(), "a commit ends the composition");
    }

    /// GTK's own order (4.22.5, `gtk_window_root_set_focus`): the widget's `has_focus` is cleared,
    /// the key controller's crossing focuses the input method out -- fcitx5-gtk commits its
    /// composition right there -- and only then does `pane_focus` hear that the keys left. So the
    /// commit arrives while the gate still says "focused", and only GTK's own answer can drop it.
    /// This is `nihao` then `Ctrl+k` (or `Ctrl+a t`, a HINT label, a click elsewhere), the
    /// checklist's items 3-4; a gate that trusts only `pane_focus`'s report sends `ni hao` to the
    /// shell and fails here.
    #[test]
    fn a_commit_out_of_gtks_own_focus_out_is_dropped_before_pane_focus_reports() {
        let mut gate = focused();
        gate.preedit_changed("ni hao", 0);
        let focus_moved = GtkFocus {
            widget_has_focus: false,
            window_is_active: true,
        };
        assert!(matches!(gate.commit("ni hao", focus_moved), Commit::Dropped(_)));
        assert_eq!(gate.preedit(), None, "the composition ended with it");
        assert_eq!(gate.set_focused(false), FocusChange::Out, "the report comes after");
    }

    /// Alt-tab, in GTK's order (4.22.5, `_gtk_window_set_is_active`): `is_active` is cleared, the
    /// activation crossing focuses the input method out -- fcitx5-gtk commits there -- and the
    /// widget's `has_focus` is cleared only after that. So at the commit the widget still says it
    /// has focus, and only the window's answer can drop it. A gate (or a call site) that asks
    /// `has_focus` alone sends `ni hao` to the shell and fails here.
    #[test]
    fn alt_tab_mid_composition_commits_nothing_though_the_widget_still_has_focus() {
        let mut gate = focused();
        gate.preedit_changed("ni hao", 0);
        let alt_tab = GtkFocus {
            widget_has_focus: true,
            window_is_active: false,
        };
        assert!(matches!(gate.commit("ni hao", alt_tab), Commit::Dropped(_)));
        assert_eq!(gate.preedit(), None, "the composition ended with it");
        assert_eq!(gate.set_focused(false), FocusChange::Out, "the report comes after");
    }

    /// The other half: once `pane_focus` has reported the keys gone, a commit that comes later still
    /// (an input method that commits asynchronously) is dropped on that report alone, and a late
    /// preedit is not drawn. Back in, typing works.
    #[test]
    fn leaving_the_terminal_mid_composition_commits_nothing() {
        let mut gate = focused();
        gate.preedit_changed("ni hao", 6);
        assert_eq!(gate.set_focused(false), FocusChange::Out);
        assert_eq!(gate.preedit(), None, "the preedit is not drawn once the keys have left");
        assert!(matches!(gate.commit("ni hao", TYPING), Commit::Dropped(_)));
        assert!(matches!(gate.commit("\u{4f60}\u{597d}", TYPING), Commit::Dropped(_)));
        gate.preedit_changed("ni hao", 6);
        assert_eq!(gate.preedit(), None, "a late preedit while unfocused is not drawn");
        assert_eq!(gate.set_focused(true), FocusChange::In);
        assert_eq!(
            sent(gate.commit("a", TYPING)),
            normalize_commit("a"),
            "back in, typing works"
        );
    }

    /// A paste or a `Ctrl+a` literal cuts into a composition: the pane discards it with a reset, and
    /// what the input method commits out of that reset never reaches the shell.
    #[test]
    fn a_commit_out_of_the_panes_own_reset_is_dropped() {
        let mut gate = focused();
        gate.preedit_changed("ni", 2);
        assert!(gate.composing());
        gate.begin_reset();
        assert!(!gate.composing());
        assert!(matches!(gate.commit("ni", TYPING), Commit::Dropped(_)));
        gate.preedit_changed("ni", 2);
        assert_eq!(gate.preedit(), None, "a preedit during the reset is not drawn");
        gate.end_reset();
        assert_eq!(sent(gate.commit("a", TYPING)), normalize_commit("a"));
    }

    #[test]
    fn a_focus_change_is_reported_once() {
        let mut gate = ImeGate::new();
        assert_eq!(
            gate.set_focused(false),
            FocusChange::Unchanged,
            "unfocused from the start"
        );
        assert_eq!(gate.set_focused(true), FocusChange::In);
        assert_eq!(gate.set_focused(true), FocusChange::Unchanged);
        assert_eq!(gate.set_focused(false), FocusChange::Out);
        assert_eq!(gate.set_focused(false), FocusChange::Unchanged);
    }

    #[test]
    fn an_empty_commit_sends_nothing_and_an_empty_preedit_is_no_composition() {
        let mut gate = focused();
        assert!(matches!(gate.commit("", TYPING), Commit::Dropped(_)));
        gate.preedit_changed("ni", 2);
        gate.preedit_changed("", 0);
        assert!(!gate.composing(), "preedit-end carries an empty string");
    }

    #[test]
    fn the_caret_is_clamped_to_the_preedit() {
        let mut gate = focused();
        gate.preedit_changed("ab", 9);
        assert_eq!(gate.preedit().map(|p| p.caret), Some(2));
        gate.preedit_changed("ab", -1);
        assert_eq!(gate.preedit().map(|p| p.caret), Some(0));
    }

    /// With an input method attached -- any GTK module, fcitx5 running or not -- every printable key it
    /// does not compose arrives as a commit, never as `key-pressed`. fcitx5-gtk 5.1.7 (his
    /// `GTK_IM_MODULE`) hands a key its server does not take, and every key while no server runs, to
    /// GTK's simple context (`fcitximcontext.cpp:485-525`, sync mode or not), which commits any
    /// printable character (`gtkimcontextsimple.c:745-750`, GTK 4.22.5); GTK's Wayland context commits
    /// it itself (`gtkimcontextwayland.c:621-645`), and so does the multicontext with no module at all,
    /// `GTK_IM_MODULE=none` (`gtkimmulticontext.c:376-407`) -- what the editor saw in `db3709d`. Enter,
    /// Tab, Backspace, Escape, the arrows, the function keys and `Ctrl`/`Alt` chords still take the key
    /// path, except a key the input method binds, which never reaches the shell (`Ctrl+Shift+U`, his
    /// fcitx5 hotkeys, `F4` with rime on: the plan's owner decision 11). A commit is its text, so the
    /// text is right and what only a key can say is lost. Each loss is pinned here, so this test fails
    /// the day one stops being true and the record must change: the release a program asked for (nvim's
    /// kitty flags 3 include event types; the key path sends one for `a`, a commit has none), a keypad
    /// digit's own code under disambiguation, and, under flag 8 (every key as an escape), the escape
    /// for a letter and for Space. nvim and his zsh tolerate all of it; a program that needs releases
    /// or keypad codes does not get them.
    #[test]
    fn a_printable_key_arriving_as_a_commit_keeps_its_text_and_loses_what_only_a_key_says() {
        use crate::terminal::keys::{normalize_key, RawKey};
        use gtk4::gdk::{Key, ModifierType};
        use terminal_input::encode;
        let key = |keyval: Key, pressed: bool| {
            normalize_key(RawKey {
                keyval,
                unmodified_keyval: None,
                state: ModifierType::empty(),
                consumed: ModifierType::empty(),
                pressed,
                repeat: false,
            })
            .unwrap()
        };
        let commit = |text: &str| sent(focused().commit(text, TYPING));
        let nvim = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_EVENT_TYPES;
        for mode in [TermMode::empty(), TermMode::BRACKETED_PASTE, nvim] {
            assert_eq!(encode(&commit("a"), mode), encode(&key(Key::a, true), mode), "{mode:?}");
        }
        // Lost: the release. The key path sends one under event types; a commit has none to send.
        assert_eq!(encode(&key(Key::a, false), nvim), b"\x1b[97;1:3u");
        // Lost: the keypad digit's own code under disambiguation.
        let disambiguate = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(encode(&key(Key::KP_1, true), disambiguate), b"\x1b[57400u");
        assert_eq!(encode(&commit("1"), disambiguate), b"1");
        // Lost under flag 8, every key as an escape: a letter and a space arrive as their text.
        let every_key = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC;
        assert_eq!(encode(&key(Key::a, true), every_key), b"\x1b[97u");
        assert_eq!(encode(&commit("a"), every_key), b"a");
        assert_eq!(encode(&key(Key::space, true), every_key), b"\x1b[32u");
        assert_eq!(encode(&commit(" "), every_key), b" ");
    }

    #[test]
    fn the_input_method_is_pointed_at_one_cell_in_logical_pixels() {
        let rect = im_cursor_rect(skia_safe::Rect::from_xywh(90.0, 36.0, 9.0, 18.0), 1);
        assert_eq!((rect.x(), rect.y(), rect.width(), rect.height()), (90, 36, 9, 18));
        // Scale 2: the metrics were built at twice the pixels, and GTK wants the logical ones.
        let rect = im_cursor_rect(skia_safe::Rect::from_xywh(180.0, 72.0, 18.0, 36.0), 2);
        assert_eq!((rect.x(), rect.y(), rect.width(), rect.height()), (90, 36, 9, 18));
        let rect = im_cursor_rect(skia_safe::Rect::from_xywh(0.0, 0.0, 0.4, 0.4), 1);
        assert_eq!((rect.width(), rect.height()), (1, 1), "never smaller than a pixel");
        let rect = im_cursor_rect(skia_safe::Rect::from_xywh(10.0, 10.0, 9.0, 18.0), 0);
        assert_eq!(rect.x(), 10, "a scale of 0 (unrealized) is read as 1");
    }
}
