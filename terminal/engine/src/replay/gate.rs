//! Counts the handler calls a stretch of bytes causes, or swallows the first `skip` of them.
//! The source counts the calls the bytes of an unfinished sequence already caused; the destination feeds
//! those bytes again with that many calls swallowed, so its parser ends in the source's state and nothing
//! is applied twice.

use alacritty_terminal::vte::ansi::{
    cursor_icon::CursorIcon, Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb, ScpCharPath,
    ScpUpdateMode, StandardCharset, TabulationClearMode,
};
use terminal_sync::ZeroWidthTarget;

use super::shadow::ShadowTarget;

pub struct Gate<'a, H> {
    inner: &'a mut H,
    skip: usize,
    seen: usize,
}

impl<'a, H> Gate<'a, H> {
    pub fn new(inner: &'a mut H, skip: usize) -> Self {
        Gate { inner, skip, seen: 0 }
    }

    /// Every call that reached this gate, swallowed or not.
    pub fn seen(&self) -> usize {
        self.seen
    }

    fn pass(&mut self) -> bool {
        self.seen += 1;
        self.seen > self.skip
    }
}

impl<H: ZeroWidthTarget> ZeroWidthTarget for Gate<'_, H> {
    fn zerowidth_at_input_target(&self) -> usize {
        self.inner.zerowidth_at_input_target()
    }
}

impl<H: ShadowTarget> ShadowTarget for Gate<'_, H> {
    fn cursor_column(&self) -> usize {
        self.inner.cursor_column()
    }
    fn screen_lines(&self) -> usize {
        self.inner.screen_lines()
    }
    fn columns(&self) -> usize {
        self.inner.columns()
    }
    fn alt_screen(&self) -> bool {
        self.inner.alt_screen()
    }
}

#[rustfmt::skip]
impl<H: Handler> Handler for Gate<'_, H> {
    fn set_title(&mut self, a0: Option<String>) { if self.pass() { self.inner.set_title(a0); } }
    fn set_cursor_style(&mut self, a0: Option<CursorStyle>) { if self.pass() { self.inner.set_cursor_style(a0); } }
    fn set_cursor_shape(&mut self, a0: CursorShape) { if self.pass() { self.inner.set_cursor_shape(a0); } }
    fn input(&mut self, a0: char) { if self.pass() { self.inner.input(a0); } }
    fn goto(&mut self, a0: i32, a1: usize) { if self.pass() { self.inner.goto(a0, a1); } }
    fn goto_line(&mut self, a0: i32) { if self.pass() { self.inner.goto_line(a0); } }
    fn goto_col(&mut self, a0: usize) { if self.pass() { self.inner.goto_col(a0); } }
    fn insert_blank(&mut self, a0: usize) { if self.pass() { self.inner.insert_blank(a0); } }
    fn move_up(&mut self, a0: usize) { if self.pass() { self.inner.move_up(a0); } }
    fn move_down(&mut self, a0: usize) { if self.pass() { self.inner.move_down(a0); } }
    fn identify_terminal(&mut self, a0: Option<char>) { if self.pass() { self.inner.identify_terminal(a0); } }
    fn device_status(&mut self, a0: usize) { if self.pass() { self.inner.device_status(a0); } }
    fn move_forward(&mut self, a0: usize) { if self.pass() { self.inner.move_forward(a0); } }
    fn move_backward(&mut self, a0: usize) { if self.pass() { self.inner.move_backward(a0); } }
    fn move_down_and_cr(&mut self, a0: usize) { if self.pass() { self.inner.move_down_and_cr(a0); } }
    fn move_up_and_cr(&mut self, a0: usize) { if self.pass() { self.inner.move_up_and_cr(a0); } }
    fn put_tab(&mut self, a0: u16) { if self.pass() { self.inner.put_tab(a0); } }
    fn backspace(&mut self) { if self.pass() { self.inner.backspace(); } }
    fn carriage_return(&mut self) { if self.pass() { self.inner.carriage_return(); } }
    fn linefeed(&mut self) { if self.pass() { self.inner.linefeed(); } }
    fn bell(&mut self) { if self.pass() { self.inner.bell(); } }
    fn substitute(&mut self) { if self.pass() { self.inner.substitute(); } }
    fn newline(&mut self) { if self.pass() { self.inner.newline(); } }
    fn set_horizontal_tabstop(&mut self) { if self.pass() { self.inner.set_horizontal_tabstop(); } }
    fn scroll_up(&mut self, a0: usize) { if self.pass() { self.inner.scroll_up(a0); } }
    fn scroll_down(&mut self, a0: usize) { if self.pass() { self.inner.scroll_down(a0); } }
    fn insert_blank_lines(&mut self, a0: usize) { if self.pass() { self.inner.insert_blank_lines(a0); } }
    fn delete_lines(&mut self, a0: usize) { if self.pass() { self.inner.delete_lines(a0); } }
    fn erase_chars(&mut self, a0: usize) { if self.pass() { self.inner.erase_chars(a0); } }
    fn delete_chars(&mut self, a0: usize) { if self.pass() { self.inner.delete_chars(a0); } }
    fn move_backward_tabs(&mut self, a0: u16) { if self.pass() { self.inner.move_backward_tabs(a0); } }
    fn move_forward_tabs(&mut self, a0: u16) { if self.pass() { self.inner.move_forward_tabs(a0); } }
    fn save_cursor_position(&mut self) { if self.pass() { self.inner.save_cursor_position(); } }
    fn restore_cursor_position(&mut self) { if self.pass() { self.inner.restore_cursor_position(); } }
    fn clear_line(&mut self, a0: LineClearMode) { if self.pass() { self.inner.clear_line(a0); } }
    fn clear_screen(&mut self, a0: ClearMode) { if self.pass() { self.inner.clear_screen(a0); } }
    fn clear_tabs(&mut self, a0: TabulationClearMode) { if self.pass() { self.inner.clear_tabs(a0); } }
    fn set_tabs(&mut self, a0: u16) { if self.pass() { self.inner.set_tabs(a0); } }
    fn reset_state(&mut self) { if self.pass() { self.inner.reset_state(); } }
    fn reverse_index(&mut self) { if self.pass() { self.inner.reverse_index(); } }
    fn terminal_attribute(&mut self, a0: Attr) { if self.pass() { self.inner.terminal_attribute(a0); } }
    fn set_mode(&mut self, a0: Mode) { if self.pass() { self.inner.set_mode(a0); } }
    fn unset_mode(&mut self, a0: Mode) { if self.pass() { self.inner.unset_mode(a0); } }
    fn report_mode(&mut self, a0: Mode) { if self.pass() { self.inner.report_mode(a0); } }
    fn set_private_mode(&mut self, a0: PrivateMode) { if self.pass() { self.inner.set_private_mode(a0); } }
    fn unset_private_mode(&mut self, a0: PrivateMode) { if self.pass() { self.inner.unset_private_mode(a0); } }
    fn report_private_mode(&mut self, a0: PrivateMode) { if self.pass() { self.inner.report_private_mode(a0); } }
    fn set_scrolling_region(&mut self, a0: usize, a1: Option<usize>) { if self.pass() { self.inner.set_scrolling_region(a0, a1); } }
    fn set_keypad_application_mode(&mut self) { if self.pass() { self.inner.set_keypad_application_mode(); } }
    fn unset_keypad_application_mode(&mut self) { if self.pass() { self.inner.unset_keypad_application_mode(); } }
    fn set_active_charset(&mut self, a0: CharsetIndex) { if self.pass() { self.inner.set_active_charset(a0); } }
    fn configure_charset(&mut self, a0: CharsetIndex, a1: StandardCharset) { if self.pass() { self.inner.configure_charset(a0, a1); } }
    fn set_color(&mut self, a0: usize, a1: Rgb) { if self.pass() { self.inner.set_color(a0, a1); } }
    fn dynamic_color_sequence(&mut self, a0: String, a1: usize, a2: &str) { if self.pass() { self.inner.dynamic_color_sequence(a0, a1, a2); } }
    fn reset_color(&mut self, a0: usize) { if self.pass() { self.inner.reset_color(a0); } }
    fn clipboard_store(&mut self, a0: u8, a1: &[u8]) { if self.pass() { self.inner.clipboard_store(a0, a1); } }
    fn clipboard_load(&mut self, a0: u8, a1: &str) { if self.pass() { self.inner.clipboard_load(a0, a1); } }
    fn decaln(&mut self) { if self.pass() { self.inner.decaln(); } }
    fn push_title(&mut self) { if self.pass() { self.inner.push_title(); } }
    fn pop_title(&mut self) { if self.pass() { self.inner.pop_title(); } }
    fn text_area_size_pixels(&mut self) { if self.pass() { self.inner.text_area_size_pixels(); } }
    fn text_area_size_chars(&mut self) { if self.pass() { self.inner.text_area_size_chars(); } }
    fn set_hyperlink(&mut self, a0: Option<Hyperlink>) { if self.pass() { self.inner.set_hyperlink(a0); } }
    fn set_mouse_cursor_icon(&mut self, a0: CursorIcon) { if self.pass() { self.inner.set_mouse_cursor_icon(a0); } }
    fn report_keyboard_mode(&mut self) { if self.pass() { self.inner.report_keyboard_mode(); } }
    fn push_keyboard_mode(&mut self, a0: KeyboardModes) { if self.pass() { self.inner.push_keyboard_mode(a0); } }
    fn pop_keyboard_modes(&mut self, a0: u16) { if self.pass() { self.inner.pop_keyboard_modes(a0); } }
    fn set_keyboard_mode(&mut self, a0: KeyboardModes, a1: KeyboardModesApplyBehavior) { if self.pass() { self.inner.set_keyboard_mode(a0, a1); } }
    fn set_modify_other_keys(&mut self, a0: ModifyOtherKeys) { if self.pass() { self.inner.set_modify_other_keys(a0); } }
    fn report_modify_other_keys(&mut self) { if self.pass() { self.inner.report_modify_other_keys(); } }
    fn set_scp(&mut self, a0: ScpCharPath, a1: ScpUpdateMode) { if self.pass() { self.inner.set_scp(a0, a1); } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::shadow::{Shadowed, Shadows};
    use crate::screen::term_config;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    use alacritty_terminal::Term;
    use terminal_sync::SyncDriver;

    struct Size;
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            5
        }
        fn screen_lines(&self) -> usize {
            5
        }
        fn columns(&self) -> usize {
            20
        }
    }

    #[test]
    fn a_gate_with_nothing_to_skip_counts_every_call_and_forwards_it() {
        let mut t = Term::new(term_config(), &Size, VoidListener);
        let mut d = SyncDriver::new();
        let mut g = Gate::new(&mut t, 0);
        d.feed(&mut g, b"ab\n", |_, _| {});
        assert_eq!(g.seen(), 3);
        assert_eq!(t.grid()[Line(0)][Column(1)].c, 'b');
    }

    #[test]
    fn an_unfinished_sequence_fed_again_with_its_calls_swallowed_applies_nothing_twice() {
        // The source: `ESC[31` then a line feed, cut before the final byte.
        let mut src = Term::new(term_config(), &Size, VoidListener);
        let mut sd = SyncDriver::new();
        let mut counted = Gate::new(&mut src, 0);
        sd.feed(&mut counted, b"\x1b[31\n", |_, _| {});
        let calls = counted.seen();
        assert_eq!(calls, 1, "the line feed inside the CSI was executed");
        // The destination: its replayed screen already shows the line feed; a fresh parser is then fed the raw
        // bytes with that many calls swallowed, then the rest.
        let mut dst = Term::new(term_config(), &Size, VoidListener);
        let mut shadows = Shadows::new(20, true);
        let mut dd = SyncDriver::new();
        dd.feed(&mut Shadowed::new(&mut dst, &mut shadows), b"\n", |_, _| {});
        let mut fresh = SyncDriver::new();
        fresh.feed(
            &mut Gate::new(&mut Shadowed::new(&mut dst, &mut shadows), calls),
            b"\x1b[31\n",
            |_, _| {},
        );
        let dd = &mut fresh;
        dd.feed(&mut Shadowed::new(&mut dst, &mut shadows), b"mX", |_, _| {});
        sd.feed(&mut src, b"mX", |_, _| {});
        for term in [&src, &dst] {
            assert_eq!(term.grid().cursor.point.line, Line(1), "one line feed");
            assert_eq!(term.grid()[Line(1)][Column(0)].c, 'X');
            assert_eq!(term.grid()[Line(1)][Column(0)].fg, Color::Named(NamedColor::Red));
        }
    }
}
