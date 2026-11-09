//! What alacritty 0.26 keeps without a getter, recorded from the handler calls on their way to the `Term`,
//! so a replay can reproduce it: the scroll region, the tab stops, the active charset, the title and its stack,
//! both screens' keyboard-mode stacks, and the last printed character `REP` repeats.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{
    cursor_icon::CursorIcon, Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb, ScpCharPath,
    ScpUpdateMode, StandardCharset, TabulationClearMode,
};
use alacritty_terminal::Term;
use terminal_sync::ZeroWidthTarget;

/// alacritty's title and keyboard-mode stack depth.
pub const KEYBOARD_STACK_MAX: usize = 4096;
const TITLE_BYTES_MAX: usize = 4096;
const TITLE_STACK_BYTES_MAX: usize = 16 * 1024 * 1024;
// Reserve a full title per entry. This also matches the emulator's oldest-first depth eviction.
const TITLE_STACK_MAX: usize = TITLE_STACK_BYTES_MAX / TITLE_BYTES_MAX;
// Both depths must match alacritty so keyboard overflow sees the same title stack in each.
const _: () = assert!(TITLE_STACK_MAX == KEYBOARD_STACK_MAX);
const TAB_EVERY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadows {
    /// Mirrors the `Term` config's `kitty_keyboard`: while it is off, alacritty records no keyboard mode either.
    pub kitty_keyboard: bool,
    /// As `set_scrolling_region` last accepted it (1-based top, bottom); `None` is the whole screen.
    pub scroll_region: Option<(usize, usize)>,
    pub tab_stops: Vec<bool>,
    pub active_charset: CharsetIndex,
    pub title: Option<String>,
    pub title_stack: Vec<Option<String>>,
    /// The stack of the screen shown now, and of the other screen.
    pub keyboard_active: Vec<KeyboardModes>,
    pub keyboard_inactive: Vec<KeyboardModes>,
    /// The character of the last `input` call, before any charset mapping: what `REP` repeats.
    pub last_input: Option<char>,
}

impl Shadows {
    pub fn new(columns: usize, kitty_keyboard: bool) -> Self {
        Shadows {
            kitty_keyboard,
            scroll_region: None,
            tab_stops: (0..columns).map(|i| i % TAB_EVERY == 0).collect(),
            active_charset: CharsetIndex::default(),
            title: None,
            title_stack: Vec::new(),
            keyboard_active: Vec::new(),
            keyboard_inactive: Vec::new(),
            last_input: None,
        }
    }

    /// What a `Term::resize` does to this state: the scroll region is reset; the tab stops are extended with the
    /// default stop every 8 columns, or cut.
    pub fn resize(&mut self, columns: usize) {
        self.scroll_region = None;
        let mut index = self.tab_stops.len();
        self.tab_stops.resize_with(columns, || {
            let stop = index % TAB_EVERY == 0;
            index += 1;
            stop
        });
    }

    /// What RIS resets. `last_input` lives in the parser, which RIS does not reach.
    fn reset(&mut self, columns: usize) {
        let last_input = self.last_input;
        *self = Shadows::new(columns, self.kitty_keyboard);
        self.last_input = last_input;
    }

    /// A stack this deep cannot be replayed faithfully (alacritty drops titles at the limit).
    pub fn too_deep(&self) -> bool {
        self.keyboard_active.len() >= KEYBOARD_STACK_MAX || self.keyboard_inactive.len() >= KEYBOARD_STACK_MAX
    }
}

/// What the shadows need to read from the handler they wrap.
pub trait ShadowTarget {
    fn cursor_column(&self) -> usize;
    fn screen_lines(&self) -> usize;
    fn columns(&self) -> usize;
    fn alt_screen(&self) -> bool;
}

impl<L: EventListener> ShadowTarget for Term<L> {
    fn cursor_column(&self) -> usize {
        self.grid().cursor.point.column.0
    }
    fn screen_lines(&self) -> usize {
        Dimensions::screen_lines(self)
    }
    fn columns(&self) -> usize {
        Dimensions::columns(self)
    }
    fn alt_screen(&self) -> bool {
        self.mode().contains(TermMode::ALT_SCREEN)
    }
}

/// Records handler state on its way to `inner`, bounding titles and refusing pushes that would panic.
pub struct Shadowed<'a, H> {
    inner: &'a mut H,
    shadows: &'a mut Shadows,
}

impl<'a, H> Shadowed<'a, H> {
    pub fn new(inner: &'a mut H, shadows: &'a mut Shadows) -> Self {
        Shadowed { inner, shadows }
    }
}

impl<H: ShadowTarget> Shadowed<'_, H> {
    fn swap_if_screen_changed(&mut self, before: bool) {
        if self.inner.alt_screen() != before {
            std::mem::swap(&mut self.shadows.keyboard_active, &mut self.shadows.keyboard_inactive);
        }
    }
}

impl<H: ZeroWidthTarget> ZeroWidthTarget for Shadowed<'_, H> {
    fn zerowidth_at_input_target(&self) -> usize {
        self.inner.zerowidth_at_input_target()
    }
}

impl<H: ShadowTarget> ShadowTarget for Shadowed<'_, H> {
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
impl<H: Handler + ShadowTarget> Handler for Shadowed<'_, H> {
    fn set_title(&mut self, mut a0: Option<String>) {
        if let Some(title) = &mut a0 {
            let mut end = title.len().min(TITLE_BYTES_MAX);
            while !title.is_char_boundary(end) { end -= 1; }
            title.truncate(end);
            title.shrink_to_fit();
        }
        self.shadows.title = a0.clone();
        self.inner.set_title(a0);
    }
    fn set_cursor_style(&mut self, a0: Option<CursorStyle>) { self.inner.set_cursor_style(a0); }
    fn set_cursor_shape(&mut self, a0: CursorShape) { self.inner.set_cursor_shape(a0); }
    fn input(&mut self, a0: char) { self.shadows.last_input = Some(a0); self.inner.input(a0); }
    fn goto(&mut self, a0: i32, a1: usize) { self.inner.goto(a0, a1); }
    fn goto_line(&mut self, a0: i32) { self.inner.goto_line(a0); }
    fn goto_col(&mut self, a0: usize) { self.inner.goto_col(a0); }
    fn insert_blank(&mut self, a0: usize) { self.inner.insert_blank(a0); }
    fn move_up(&mut self, a0: usize) { self.inner.move_up(a0); }
    fn move_down(&mut self, a0: usize) { self.inner.move_down(a0); }
    fn identify_terminal(&mut self, a0: Option<char>) { self.inner.identify_terminal(a0); }
    fn device_status(&mut self, a0: usize) { self.inner.device_status(a0); }
    fn move_forward(&mut self, a0: usize) { self.inner.move_forward(a0); }
    fn move_backward(&mut self, a0: usize) { self.inner.move_backward(a0); }
    fn move_down_and_cr(&mut self, a0: usize) { self.inner.move_down_and_cr(a0); }
    fn move_up_and_cr(&mut self, a0: usize) { self.inner.move_up_and_cr(a0); }
    fn put_tab(&mut self, a0: u16) { self.inner.put_tab(a0); }
    fn backspace(&mut self) { self.inner.backspace(); }
    fn carriage_return(&mut self) { self.inner.carriage_return(); }
    fn linefeed(&mut self) { self.inner.linefeed(); }
    fn bell(&mut self) { self.inner.bell(); }
    fn substitute(&mut self) { self.inner.substitute(); }
    fn newline(&mut self) { self.inner.newline(); }
    fn set_horizontal_tabstop(&mut self) {
        let col = self.inner.cursor_column();
        self.inner.set_horizontal_tabstop();
        if let Some(stop) = self.shadows.tab_stops.get_mut(col) { *stop = true; }
    }
    fn scroll_up(&mut self, a0: usize) { self.inner.scroll_up(a0); }
    fn scroll_down(&mut self, a0: usize) { self.inner.scroll_down(a0); }
    fn insert_blank_lines(&mut self, a0: usize) { self.inner.insert_blank_lines(a0); }
    fn delete_lines(&mut self, a0: usize) { self.inner.delete_lines(a0); }
    fn erase_chars(&mut self, a0: usize) { self.inner.erase_chars(a0); }
    fn delete_chars(&mut self, a0: usize) { self.inner.delete_chars(a0); }
    fn move_backward_tabs(&mut self, a0: u16) { self.inner.move_backward_tabs(a0); }
    fn move_forward_tabs(&mut self, a0: u16) { self.inner.move_forward_tabs(a0); }
    fn save_cursor_position(&mut self) { self.inner.save_cursor_position(); }
    fn restore_cursor_position(&mut self) { self.inner.restore_cursor_position(); }
    fn clear_line(&mut self, a0: LineClearMode) { self.inner.clear_line(a0); }
    fn clear_screen(&mut self, a0: ClearMode) { self.inner.clear_screen(a0); }
    fn clear_tabs(&mut self, a0: TabulationClearMode) {
        let col = self.inner.cursor_column();
        match a0 {
            TabulationClearMode::Current => { if let Some(stop) = self.shadows.tab_stops.get_mut(col) { *stop = false; } }
            TabulationClearMode::All => self.shadows.tab_stops.iter_mut().for_each(|s| *s = false),
        }
        self.inner.clear_tabs(a0);
    }
    fn set_tabs(&mut self, a0: u16) { self.inner.set_tabs(a0); }
    fn reset_state(&mut self) { self.inner.reset_state(); let cols = self.inner.columns(); self.shadows.reset(cols); }
    fn reverse_index(&mut self) { self.inner.reverse_index(); }
    fn terminal_attribute(&mut self, a0: Attr) { self.inner.terminal_attribute(a0); }
    fn set_mode(&mut self, a0: Mode) { self.inner.set_mode(a0); }
    fn unset_mode(&mut self, a0: Mode) { self.inner.unset_mode(a0); }
    fn report_mode(&mut self, a0: Mode) { self.inner.report_mode(a0); }
    fn set_private_mode(&mut self, a0: PrivateMode) { let before = self.inner.alt_screen(); self.inner.set_private_mode(a0); self.swap_if_screen_changed(before); }
    fn unset_private_mode(&mut self, a0: PrivateMode) { let before = self.inner.alt_screen(); self.inner.unset_private_mode(a0); self.swap_if_screen_changed(before); }
    fn report_private_mode(&mut self, a0: PrivateMode) { self.inner.report_private_mode(a0); }
    fn set_scrolling_region(&mut self, a0: usize, a1: Option<usize>) {
        let bottom = a1.unwrap_or_else(|| self.inner.screen_lines());
        self.inner.set_scrolling_region(a0, a1);
        if a0 < bottom { self.shadows.scroll_region = Some((a0, bottom)); }
    }
    fn set_keypad_application_mode(&mut self) { self.inner.set_keypad_application_mode(); }
    fn unset_keypad_application_mode(&mut self) { self.inner.unset_keypad_application_mode(); }
    fn set_active_charset(&mut self, a0: CharsetIndex) { self.shadows.active_charset = a0; self.inner.set_active_charset(a0); }
    fn configure_charset(&mut self, a0: CharsetIndex, a1: StandardCharset) { self.inner.configure_charset(a0, a1); }
    fn set_color(&mut self, a0: usize, a1: Rgb) { self.inner.set_color(a0, a1); }
    fn dynamic_color_sequence(&mut self, a0: String, a1: usize, a2: &str) { self.inner.dynamic_color_sequence(a0, a1, a2); }
    fn reset_color(&mut self, a0: usize) { self.inner.reset_color(a0); }
    fn clipboard_store(&mut self, a0: u8, a1: &[u8]) { self.inner.clipboard_store(a0, a1); }
    fn clipboard_load(&mut self, a0: u8, a1: &str) { self.inner.clipboard_load(a0, a1); }
    fn decaln(&mut self) { self.inner.decaln(); }
    fn push_title(&mut self) {
        if self.shadows.title_stack.len() >= TITLE_STACK_MAX { self.shadows.title_stack.remove(0); }
        self.shadows.title_stack.push(self.shadows.title.clone());
        self.inner.push_title();
    }
    fn pop_title(&mut self) {
        if let Some(title) = self.shadows.title_stack.pop() { self.shadows.title = title; }
        self.inner.pop_title();
    }
    fn text_area_size_pixels(&mut self) { self.inner.text_area_size_pixels(); }
    fn text_area_size_chars(&mut self) { self.inner.text_area_size_chars(); }
    fn set_hyperlink(&mut self, a0: Option<Hyperlink>) { self.inner.set_hyperlink(a0); }
    fn set_mouse_cursor_icon(&mut self, a0: CursorIcon) { self.inner.set_mouse_cursor_icon(a0); }
    fn report_keyboard_mode(&mut self) { self.inner.report_keyboard_mode(); }
    fn push_keyboard_mode(&mut self, a0: KeyboardModes) {
        if self.shadows.kitty_keyboard {
            if self.shadows.keyboard_active.len() >= KEYBOARD_STACK_MAX {
                // The pinned emulator removes a title here, and panics if none exists.
                // Dropping the call leaves both keyboard stacks and the active mode unchanged.
                if self.shadows.title_stack.is_empty() { return; }
                self.shadows.title_stack.remove(0);
            }
            self.shadows.keyboard_active.push(a0);
        }
        self.inner.push_keyboard_mode(a0);
    }
    fn pop_keyboard_modes(&mut self, a0: u16) {
        if self.shadows.kitty_keyboard {
            let len = self.shadows.keyboard_active.len().saturating_sub(a0 as usize);
            self.shadows.keyboard_active.truncate(len);
        }
        self.inner.pop_keyboard_modes(a0);
    }
    fn set_keyboard_mode(&mut self, a0: KeyboardModes, a1: KeyboardModesApplyBehavior) { self.inner.set_keyboard_mode(a0, a1); }
    fn set_modify_other_keys(&mut self, a0: ModifyOtherKeys) { self.inner.set_modify_other_keys(a0); }
    fn report_modify_other_keys(&mut self) { self.inner.report_modify_other_keys(); }
    fn set_scp(&mut self, a0: ScpCharPath, a1: ScpUpdateMode) { self.inner.set_scp(a0, a1); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::term_config;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Column, Line};
    use terminal_sync::SyncDriver;

    struct Size(usize, usize);
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    fn setup(cols: usize, rows: usize) -> (Term<VoidListener>, Shadows, SyncDriver) {
        (
            Term::new(term_config(), &Size(cols, rows), VoidListener),
            Shadows::new(cols, true),
            SyncDriver::new(),
        )
    }

    fn feed(t: &mut Term<VoidListener>, s: &mut Shadows, d: &mut SyncDriver, bytes: &[u8]) {
        d.feed(&mut Shadowed::new(t, s), bytes, |_, _| {});
    }

    /// The columns a run of HTs from column 0 stops at: the `Term`'s own tab stops, read through behaviour.
    fn stops(t: &mut Term<VoidListener>, d: &mut SyncDriver) -> Vec<usize> {
        let mut out = Vec::new();
        let mut s = Shadows::new(Dimensions::columns(t), true);
        feed(t, &mut s, d, b"\r");
        loop {
            let before = t.grid().cursor.point.column.0;
            feed(t, &mut s, d, b"\t");
            let after = t.grid().cursor.point.column.0;
            if after == before {
                break;
            }
            out.push(after);
        }
        out
    }

    fn shadow_stops(s: &Shadows) -> Vec<usize> {
        (1..s.tab_stops.len())
            .filter(|&i| s.tab_stops[i])
            .collect::<Vec<_>>()
            .into_iter()
            .chain(std::iter::once(s.tab_stops.len() - 1))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[test]
    fn the_scroll_region_is_kept_and_forgotten_on_resize_and_ris() {
        let (mut t, mut s, mut d) = setup(80, 24);
        feed(&mut t, &mut s, &mut d, b"\x1b[5;20r");
        assert_eq!(s.scroll_region, Some((5, 20)));
        feed(&mut t, &mut s, &mut d, b"\x1b[30;10r");
        assert_eq!(
            s.scroll_region,
            Some((5, 20)),
            "an invalid region is ignored, as alacritty does"
        );
        s.resize(100);
        assert_eq!(s.scroll_region, None);
        feed(&mut t, &mut s, &mut d, b"\x1b[3;9r\x1bc");
        assert_eq!(s.scroll_region, None);
    }

    #[test]
    fn tab_stops_follow_hts_and_tbc_like_the_terms_own() {
        let (mut t, mut s, mut d) = setup(40, 5);
        feed(
            &mut t,
            &mut s,
            &mut d,
            b"\x1b[3g\x1b[1;5H\x1bH\x1b[1;13H\x1bH\x1b[1;30H\x1bH\x1b[1;13H\x1b[g",
        );
        assert_eq!(shadow_stops(&s), stops(&mut t, &mut d));
        assert!(s.tab_stops[4] && !s.tab_stops[12] && s.tab_stops[29]);
    }

    #[test]
    fn a_resize_extends_or_cuts_the_tab_stops_as_alacritty_does() {
        let mut s = Shadows::new(20, true);
        s.tab_stops[3] = true;
        s.resize(10);
        assert_eq!(s.tab_stops.len(), 10);
        assert!(s.tab_stops[3] && s.tab_stops[8]);
        s.resize(25);
        assert!(s.tab_stops[16] && s.tab_stops[24] && !s.tab_stops[20]);
    }

    #[test]
    fn keyboard_stacks_are_kept_per_screen_and_swap_with_it() {
        let (mut t, mut s, mut d) = setup(80, 24);
        feed(&mut t, &mut s, &mut d, b"\x1b[>1u");
        feed(&mut t, &mut s, &mut d, b"\x1b[?1049h\x1b[>3u\x1b[>5u");
        assert_eq!(
            s.keyboard_active,
            vec![
                KeyboardModes::from_bits_truncate(3),
                KeyboardModes::from_bits_truncate(5)
            ]
        );
        assert_eq!(s.keyboard_inactive, vec![KeyboardModes::from_bits_truncate(1)]);
        feed(&mut t, &mut s, &mut d, b"\x1b[?1049l");
        assert_eq!(s.keyboard_active, vec![KeyboardModes::from_bits_truncate(1)]);
        feed(&mut t, &mut s, &mut d, b"\x1b[<1u");
        assert!(s.keyboard_active.is_empty());
    }

    #[test]
    fn ris_clears_every_shadow_but_the_last_printed_character() {
        let (mut t, mut s, mut d) = setup(80, 24);
        feed(
            &mut t,
            &mut s,
            &mut d,
            b"\x1b[>1u\x1b]2;t\x07\x1b[22t\x0eX\x1b[2;9r\x1bc",
        );
        assert_eq!(s.keyboard_active, Vec::<KeyboardModes>::new());
        assert!(s.title_stack.is_empty() && s.title.is_none() && s.scroll_region.is_none());
        assert_eq!(s.active_charset, CharsetIndex::G0);
        assert_eq!(s.last_input, Some('X'));
    }

    #[test]
    fn titles_follow_push_and_pop() {
        let (mut t, mut s, mut d) = setup(80, 24);
        feed(
            &mut t,
            &mut s,
            &mut d,
            b"\x1b]2;one\x07\x1b[22t\x1b]2;two\x07\x1b[22t\x1b]2;three\x07",
        );
        assert_eq!(s.title.as_deref(), Some("three"));
        assert_eq!(s.title_stack, vec![Some("one".into()), Some("two".into())]);
        feed(&mut t, &mut s, &mut d, b"\x1b[23t");
        assert_eq!(s.title.as_deref(), Some("two"));
    }

    #[test]
    fn a_keyboard_push_past_the_depth_limit_drops_the_oldest_title_as_alacritty_does() {
        let (mut t, mut s, mut d) = setup(80, 24);
        feed(&mut t, &mut s, &mut d, b"\x1b]2;a\x07\x1b[22t\x1b]2;b\x07\x1b[22t");
        for _ in 0..KEYBOARD_STACK_MAX {
            feed(&mut t, &mut s, &mut d, b"\x1b[>1u");
        }
        assert!(s.too_deep());
        assert_eq!(s.title_stack, vec![Some("a".into()), Some("b".into())]);
        feed(&mut t, &mut s, &mut d, b"\x1b[>1u");
        assert_eq!(s.title_stack, vec![Some("b".into())]);
    }

    #[test]
    fn the_last_input_is_the_character_before_charset_mapping() {
        let (mut t, mut s, mut d) = setup(80, 24);
        feed(&mut t, &mut s, &mut d, b"\x1b(0q");
        assert_eq!(s.last_input, Some('q'));
        assert_ne!(
            t.grid()[Line(0)][Column(0)].c,
            'q',
            "the Term stored the mapped line-drawing character"
        );
        feed(&mut t, &mut s, &mut d, b"\x0e");
        assert_eq!(s.active_charset, CharsetIndex::G1);
    }

    #[test]
    fn a_shadowed_term_draws_exactly_what_a_bare_one_does() {
        let corpus: &[u8] = b"\x1b[31mred\x1b[0m\r\n\x1b[1;4mx\x1b(0lqk\x1b(B\ttab\x1b[2;5r\x1b[5;1H\n\n\n\x1b]2;t\x07\x1b[?1049hALT\x1b[?1049l";
        let (mut a, mut s, mut d1) = setup(40, 8);
        feed(&mut a, &mut s, &mut d1, corpus);
        let mut b = Term::new(term_config(), &Size(40, 8), VoidListener);
        let mut d2 = SyncDriver::new();
        d2.feed(&mut b, corpus, |_, _| {});
        for line in 0..8 {
            for col in 0..40 {
                assert_eq!(
                    a.grid()[Line(line)][Column(col)],
                    b.grid()[Line(line)][Column(col)],
                    "{line},{col}"
                );
            }
        }
        assert_eq!(a.grid().cursor.point, b.grid().cursor.point);
        assert_eq!(*a.mode(), *b.mode());
    }
}
