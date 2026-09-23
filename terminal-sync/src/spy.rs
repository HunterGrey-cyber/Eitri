//! `SyncSpy` -- the transparent pass-through `vte::ansi::Handler` that drives the barrier.
//!
//! THIS FILE IS HAND-MAINTAINED ON PURPOSE, and it is the mutation target of the gate.
//!
//! Every method of `vte::ansi::Handler` has a silent no-op DEFAULT implementation. A forward
//! that is never written, or is written but forwards the wrong thing, therefore compiles
//! cleanly and silently breaks the terminal. `tests/forwarding.rs` is the net: its method list
//! is regenerated from vte's own trait declaration by `build.rs` on every build, so if vte
//! grows a method this file does not have, the test fails rather than the forward going
//! missing. `scripts/mutation_sweep.sh` proves the net has no holes.
//!
//! (The initial text of the `impl` block below was seeded mechanically from the same trait
//! declaration. It is checked in rather than generated precisely so that it CAN drift and the
//! generated test can catch the drift.)

use alacritty_terminal::vte::ansi::{
    cursor_icon::CursorIcon, Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedPrivateMode, PrivateMode,
    Rgb, ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

use crate::barrier::SyncBarrier;

/// Wraps the authoritative `Term` (or any other `Handler`) for the duration of one parser run.
///
/// It forwards EVERYTHING untouched -- the locked model is a publication barrier, not a
/// replacement parser -- and additionally tells the [`SyncBarrier`] that a dispatch happened.
/// Publication is gated on that dispatch signal and never on "bytes arrived": an 8-byte BSU
/// delivered one byte per read produces seven reads with zero dispatches, and gating on chunk
/// arrival published seven spurious snapshots for it.
///
/// Exactly two methods are intercepted -- `set_private_mode` / `unset_private_mode` carrying
/// `PrivateMode::Named(NamedPrivateMode::SyncUpdate)` (DECSET/DECRST 2026). Both STILL forward:
/// alacritty_terminal 0.26.0 ignores mode 2026 in both directions
/// (`term/mod.rs:1992` and `:2041` are `=> ()`), but it reports it as recognised-but-reset from
/// DECRQM (`:2084`), so swallowing the escape would be a gratuitous divergence from the Term the
/// renderer reads.
pub struct SyncSpy<'a, H: Handler> {
    inner: &'a mut H,
    barrier: &'a mut SyncBarrier,
}

impl<'a, H: Handler> SyncSpy<'a, H> {
    pub fn new(inner: &'a mut H, barrier: &'a mut SyncBarrier) -> Self {
        Self { inner, barrier }
    }

    /// The wrapped handler.
    pub fn inner(&mut self) -> &mut H {
        self.inner
    }
}

#[rustfmt::skip]
impl<H: Handler> Handler for SyncSpy<'_, H> {
    fn set_title(&mut self, a0: Option<String>) { self.barrier.note_dispatch(); self.inner.set_title(a0); }
    fn set_cursor_style(&mut self, a0: Option<CursorStyle>) { self.barrier.note_dispatch(); self.inner.set_cursor_style(a0); }
    fn set_cursor_shape(&mut self, a0: CursorShape) { self.barrier.note_dispatch(); self.inner.set_cursor_shape(a0); }
    fn input(&mut self, a0: char) { self.barrier.note_dispatch(); self.inner.input(a0); }
    fn goto(&mut self, a0: i32, a1: usize) { self.barrier.note_dispatch(); self.inner.goto(a0, a1); }
    fn goto_line(&mut self, a0: i32) { self.barrier.note_dispatch(); self.inner.goto_line(a0); }
    fn goto_col(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.goto_col(a0); }
    fn insert_blank(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.insert_blank(a0); }
    fn move_up(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.move_up(a0); }
    fn move_down(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.move_down(a0); }
    fn identify_terminal(&mut self, a0: Option<char>) { self.barrier.note_dispatch(); self.inner.identify_terminal(a0); }
    fn device_status(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.device_status(a0); }
    fn move_forward(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.move_forward(a0); }
    fn move_backward(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.move_backward(a0); }
    fn move_down_and_cr(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.move_down_and_cr(a0); }
    fn move_up_and_cr(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.move_up_and_cr(a0); }
    fn put_tab(&mut self, a0: u16) { self.barrier.note_dispatch(); self.inner.put_tab(a0); }
    fn backspace(&mut self) { self.barrier.note_dispatch(); self.inner.backspace(); }
    fn carriage_return(&mut self) { self.barrier.note_dispatch(); self.inner.carriage_return(); }
    fn linefeed(&mut self) { self.barrier.note_dispatch(); self.inner.linefeed(); }
    fn bell(&mut self) { self.barrier.note_dispatch(); self.inner.bell(); }
    fn substitute(&mut self) { self.barrier.note_dispatch(); self.inner.substitute(); }
    fn newline(&mut self) { self.barrier.note_dispatch(); self.inner.newline(); }
    fn set_horizontal_tabstop(&mut self) { self.barrier.note_dispatch(); self.inner.set_horizontal_tabstop(); }
    fn scroll_up(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.scroll_up(a0); }
    fn scroll_down(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.scroll_down(a0); }
    fn insert_blank_lines(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.insert_blank_lines(a0); }
    fn delete_lines(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.delete_lines(a0); }
    fn erase_chars(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.erase_chars(a0); }
    fn delete_chars(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.delete_chars(a0); }
    fn move_backward_tabs(&mut self, a0: u16) { self.barrier.note_dispatch(); self.inner.move_backward_tabs(a0); }
    fn move_forward_tabs(&mut self, a0: u16) { self.barrier.note_dispatch(); self.inner.move_forward_tabs(a0); }
    fn save_cursor_position(&mut self) { self.barrier.note_dispatch(); self.inner.save_cursor_position(); }
    fn restore_cursor_position(&mut self) { self.barrier.note_dispatch(); self.inner.restore_cursor_position(); }
    fn clear_line(&mut self, a0: LineClearMode) { self.barrier.note_dispatch(); self.inner.clear_line(a0); }
    fn clear_screen(&mut self, a0: ClearMode) { self.barrier.note_dispatch(); self.inner.clear_screen(a0); }
    fn clear_tabs(&mut self, a0: TabulationClearMode) { self.barrier.note_dispatch(); self.inner.clear_tabs(a0); }
    fn set_tabs(&mut self, a0: u16) { self.barrier.note_dispatch(); self.inner.set_tabs(a0); }
    fn reset_state(&mut self) { self.barrier.note_dispatch(); self.inner.reset_state(); }
    fn reverse_index(&mut self) { self.barrier.note_dispatch(); self.inner.reverse_index(); }
    fn terminal_attribute(&mut self, a0: Attr) { self.barrier.note_dispatch(); self.inner.terminal_attribute(a0); }
    fn set_mode(&mut self, a0: Mode) { self.barrier.note_dispatch(); self.inner.set_mode(a0); }
    fn unset_mode(&mut self, a0: Mode) { self.barrier.note_dispatch(); self.inner.unset_mode(a0); }
    fn report_mode(&mut self, a0: Mode) { self.barrier.note_dispatch(); self.inner.report_mode(a0); }
    fn set_private_mode(&mut self, a0: PrivateMode) {
        if a0 == PrivateMode::Named(NamedPrivateMode::SyncUpdate) {
            self.barrier.begin_sync();
        } else {
            self.barrier.note_dispatch();
        }
        self.inner.set_private_mode(a0);
    }
    fn unset_private_mode(&mut self, a0: PrivateMode) {
        if a0 == PrivateMode::Named(NamedPrivateMode::SyncUpdate) {
            self.barrier.end_sync();
        } else {
            self.barrier.note_dispatch();
        }
        self.inner.unset_private_mode(a0);
    }
    fn report_private_mode(&mut self, a0: PrivateMode) { self.barrier.note_dispatch(); self.inner.report_private_mode(a0); }
    fn set_scrolling_region(&mut self, a0: usize, a1: Option<usize>) { self.barrier.note_dispatch(); self.inner.set_scrolling_region(a0, a1); }
    fn set_keypad_application_mode(&mut self) { self.barrier.note_dispatch(); self.inner.set_keypad_application_mode(); }
    fn unset_keypad_application_mode(&mut self) { self.barrier.note_dispatch(); self.inner.unset_keypad_application_mode(); }
    fn set_active_charset(&mut self, a0: CharsetIndex) { self.barrier.note_dispatch(); self.inner.set_active_charset(a0); }
    fn configure_charset(&mut self, a0: CharsetIndex, a1: StandardCharset) { self.barrier.note_dispatch(); self.inner.configure_charset(a0, a1); }
    fn set_color(&mut self, a0: usize, a1: Rgb) { self.barrier.note_dispatch(); self.inner.set_color(a0, a1); }
    fn dynamic_color_sequence(&mut self, a0: String, a1: usize, a2: &str) { self.barrier.note_dispatch(); self.inner.dynamic_color_sequence(a0, a1, a2); }
    fn reset_color(&mut self, a0: usize) { self.barrier.note_dispatch(); self.inner.reset_color(a0); }
    fn clipboard_store(&mut self, a0: u8, a1: &[u8]) { self.barrier.note_dispatch(); self.inner.clipboard_store(a0, a1); }
    fn clipboard_load(&mut self, a0: u8, a1: &str) { self.barrier.note_dispatch(); self.inner.clipboard_load(a0, a1); }
    fn decaln(&mut self) { self.barrier.note_dispatch(); self.inner.decaln(); }
    fn push_title(&mut self) { self.barrier.note_dispatch(); self.inner.push_title(); }
    fn pop_title(&mut self) { self.barrier.note_dispatch(); self.inner.pop_title(); }
    fn text_area_size_pixels(&mut self) { self.barrier.note_dispatch(); self.inner.text_area_size_pixels(); }
    fn text_area_size_chars(&mut self) { self.barrier.note_dispatch(); self.inner.text_area_size_chars(); }
    fn set_hyperlink(&mut self, a0: Option<Hyperlink>) { self.barrier.note_dispatch(); self.inner.set_hyperlink(a0); }
    fn set_mouse_cursor_icon(&mut self, a0: CursorIcon) { self.barrier.note_dispatch(); self.inner.set_mouse_cursor_icon(a0); }
    fn report_keyboard_mode(&mut self) { self.barrier.note_dispatch(); self.inner.report_keyboard_mode(); }
    fn push_keyboard_mode(&mut self, a0: KeyboardModes) { self.barrier.note_dispatch(); self.inner.push_keyboard_mode(a0); }
    fn pop_keyboard_modes(&mut self, a0: u16) { self.barrier.note_dispatch(); self.inner.pop_keyboard_modes(a0); }
    fn set_keyboard_mode(&mut self, a0: KeyboardModes, a1: KeyboardModesApplyBehavior) { self.barrier.note_dispatch(); self.inner.set_keyboard_mode(a0, a1); }
    fn set_modify_other_keys(&mut self, a0: ModifyOtherKeys) { self.barrier.note_dispatch(); self.inner.set_modify_other_keys(a0); }
    fn report_modify_other_keys(&mut self) { self.barrier.note_dispatch(); self.inner.report_modify_other_keys(); }
    fn set_scp(&mut self, a0: ScpCharPath, a1: ScpUpdateMode) { self.barrier.note_dispatch(); self.inner.set_scp(a0, a1); }
}
