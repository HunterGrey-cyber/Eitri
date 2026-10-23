//! The terminal's input method: composition, focus gating, and text sent to the shell.
//!
//! A `GtkIMMulticontext` uses the pane as its client widget and is attached to the key controller
//! with `set_im_context`. GTK runs it before `key-pressed`, so keys consumed by composition never
//! reach the terminal's key handler. Composed text arrives through `commit`; `preedit-changed`
//! updates the composition drawn by the pane, and `set_cursor_location` positions the candidates.
//! GTK wiring lives in `pane.rs`; the pure logic here is tested with synthetic signals.
//!
//! A commit reaches the shell only while the terminal has the keys and is not resetting its input
//! method. The prefix, navigation keys, HINT, and app accelerators run in capture phase and can move
//! focus before the input method sees a key. GTK focuses the input method out during the focus
//! change, before notifying focus listeners. fcitx5-gtk can synchronously commit unfinished text
//! there, so `nihao` followed by `Ctrl+k` can produce a commit of `ni hao`.
//!
//! [`ImeGate::commit`] checks [`GtkFocus`], the widget's `has_focus` and its window's `is_active`,
//! as well as the last focus report. Any false value drops the commit, preventing unfinished pinyin
//! or an unchosen candidate from reaching the shell. A reset can also emit a commit, so a paste
//! or literal `Ctrl+a` discards the composition during reset before sending its own input.
//!
//! GTK state catches focus changes initiated by GTK, including navigation, clicks, and switching
//! applications. It cannot detect a focus-out initiated by the fcitx5 server while GTK still reports
//! focus. Such a commit can pass the gate, including a stale focus-out arriving just after focus
//! returns and new composition begins within one D-Bus exchange.
//!
//! Single-line commits keep their bytes unchanged and are not bracketed (`keys::normalize_commit`).
//! A commit containing a line break is treated as a paste (`keys::normalize_paste`): bracketed when
//! the program requests it, otherwise with line breaks sent as Enter, like `Ctrl+Shift+V`.
//! This also covers fcitx5's `Ctrl+;` clipboard history, whose unbracketed lines could execute in zsh.
//!
//! With an input method attached, printable keys it does not compose arrive as text commits even
//! if fcitx5 is not running. Text commits lose information available only in key events; keys bound
//! by the input method never reach the shell.
//!
//! The pane follows the input method's caret. fcitx5-rime defaults to the first composition cell on
//! Linux (`PreeditCursorPositionAtBeginning`), keeping the beam and candidate window there while
//! typing. Pango preedit attributes, including rime's `HighLight` on the converted segment, are not drawn.

use gtk4::gdk::Rectangle;
use terminal_input::NormalizedInput;

use super::keys::{normalize_commit, normalize_paste};

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
    /// The terminal holds the keys, as the focus report last said ("has the keys": focus AND an
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

    /// The focus report for the terminal. Losing the keys discards the composition here too, so
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
    /// method, before the focus report (the module doc). That, or the last report saying no,
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
        // A line break makes it a paste: a multi-line commit is
        // text not typed line by line -- fcitx5's clipboard history -- and a shell runs
        // each unbracketed line as it arrives.
        if text.contains(['\n', '\r']) {
            return Commit::Send(normalize_paste(text));
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
    use super::super::keys::normalize_paste;
    use super::*;
    use alacritty_terminal::term::TermMode;

    /// GTK's answer while the user types into the terminal: the widget has focus, the window is
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

    /// A commit with a line break, such as fcitx5 clipboard history from `Ctrl+;`, is a paste.
    /// Like `Ctrl+Shift+V`, it is bracketed when requested by the program; otherwise its line
    /// breaks become Enter. Treating it as typing could execute each line in the shell.
    /// A single-line commit keeps its bytes and is never bracketed.
    #[test]
    fn a_multi_line_commit_is_a_paste_and_a_single_line_one_is_typing() {
        let mut gate = focused();
        let input = sent(gate.commit("echo a\necho b", TYPING));
        assert_eq!(input, normalize_paste("echo a\necho b"));
        assert_eq!(
            terminal_input::encode(&input, TermMode::BRACKETED_PASTE),
            b"\x1b[200~echo a\necho b\x1b[201~".to_vec()
        );
        assert_eq!(
            terminal_input::encode(&input, TermMode::empty()),
            b"echo a\recho b".to_vec()
        );
        assert_eq!(
            sent(gate.commit("a\rb", TYPING)),
            normalize_paste("a\rb"),
            "a lone CR is a line break too"
        );
        let one_line = sent(gate.commit("\u{4f60}\u{597d} world", TYPING));
        assert_eq!(one_line, normalize_commit("\u{4f60}\u{597d} world"));
        assert_eq!(
            terminal_input::encode(&one_line, TermMode::BRACKETED_PASTE),
            "\u{4f60}\u{597d} world".as_bytes().to_vec(),
            "typing is never bracketed"
        );
    }

    /// A sequence measured in a GUI pass: rime's preedit went `n`, `ni`, `ni h`, `ni ha`, `ni hao`, and
    /// Space committed the word. The bytes at the PTY are the GUI checklist's oracle,
    /// `e4 bd a0 e5 a5 bd`, checked with bracketed paste on, as zsh runs by default, because a commit
    /// is typing and must not arrive bracketed.
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

    /// In GTK 4.22.5, `gtk_window_root_set_focus` clears the widget's `has_focus` before
    /// focusing the input method out. fcitx5-gtk commits its composition during focus-out,
    /// before the gate receives the focus report. The gate must also check GTK's focus
    /// state to drop this commit; trusting only the delayed report would send the
    /// unfinished composition to the shell when a navigation key or click moves focus.
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

    /// The other half: once the focus report has said the keys are gone, a commit that comes later still
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

    /// With an input method attached, printable keys it does not compose arrive as commits,
    /// not `key-pressed` events. fcitx5-gtk forwards keys its server does not consume to GTK's
    /// simple context, including when no server runs. That context commits printable characters,
    /// as do GTK's Wayland context and the multicontext with `GTK_IM_MODULE=none`.
    /// Enter, Tab, Backspace, Escape, arrows, function keys and `Ctrl`/`Alt` chords still use
    /// the key path unless the input method binds them, in which case they never reach the shell.
    /// A commit keeps text but loses key-only information. This test checks the missing release
    /// under kitty event reporting, the keypad digit's distinct code under disambiguation,
    /// and the letter and Space escapes under flag 8. Programs requesting releases or keypad
    /// codes cannot recover that information from a text commit.
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
