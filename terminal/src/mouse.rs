//! Mouse reporting, focus reporting and alternate scroll (bottom-terminal phase 3c).
//!
//! GTK-free, like every other file in this crate but `session.rs`'s host-facing types: nothing here
//! names a toolkit, so it is tested with no display. Encodes exactly what xterm's own "Mouse
//! Tracking" section (`ctlseqs.txt`) and `alacritty_terminal`'s `input/mod.rs` (`mouse_report`,
//! `sgr_mouse_report`, `normal_mouse_report`) describe, against the live [`TermMode`] the session
//! thread already holds -- the same way a key is encoded against it (`terminal_input::encode`).
//!
//! **Reports are not typing.** Unlike [`crate::session::SessionCommand::Input`], a mouse or focus
//! report never touches [`crate::screen::Screen::note_input`]: it must not snap a scrolled-back view
//! to the bottom or clear a selection just because the program asked where the pointer is.

use alacritty_terminal::term::TermMode;

/// A mouse button, numbered as xterm's protocol does (left 0, middle 1, right 2 before modifiers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    fn code(self) -> u16 {
        match self {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
        }
    }
}

/// A wheel notch's direction. Only `Up`/`Down` are wired to a host gesture (Task 9's own vertical
/// scroll controller); `Left`/`Right` exist so the encoder is complete for a future horizontal
/// wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelDir {
    Up,
    Down,
    Left,
    Right,
}

impl WheelDir {
    fn code(self) -> u16 {
        match self {
            WheelDir::Up => 64,
            WheelDir::Down => 65,
            WheelDir::Left => 66,
            WheelDir::Right => 67,
        }
    }
}

/// Which modifiers were held when the gesture happened. Added to the button code as xterm's
/// protocol says: `+4` Shift, `+8` Alt, `+16` Ctrl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl MouseMods {
    fn bits(self) -> u16 {
        let mut bits = 0;
        if self.shift {
            bits += 4;
        }
        if self.alt {
            bits += 8;
        }
        if self.ctrl {
            bits += 16;
        }
        bits
    }
}

/// What happened. A `Release` always names the button that was let go (SGR can say so; the normal
/// encoding cannot and reports a fixed release code instead, xterm's own limitation).
/// `Motion(None)`: the pointer moved with no button held, reportable only under `MOUSE_MOTION`
/// (1003). `Motion(Some(button))`: a drag, reportable under `MOUSE_MOTION` or `MOUSE_DRAG` (1002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press(Button),
    Release(Button),
    Motion(Option<Button>),
    Wheel(WheelDir),
}

/// One mouse event to encode: what happened, where (0-based screen row/column -- the program's own
/// screen row, never a scrollback line: a caller must not report from a scrolled-back view), and
/// which modifiers were held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseInput {
    pub kind: MouseKind,
    pub line: u16,
    pub col: u16,
    pub mods: MouseMods,
}

/// The three things the host needs to know, read off the live [`TermMode`] with every frame
/// (`crate::session::Update::mouse`), so it can decide *before* sending whether a gesture is the
/// program's (start a report) or its own (start a selection or scroll the pane): `report` is any
/// mouse mode at all (1000/1002/1003); `alt_screen` and `alternate_scroll` are what
/// [`alternate_scroll`] itself needs from the live mode, published alongside so the host never has
/// to ask the session thread for them separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseModes {
    pub report: bool,
    pub alt_screen: bool,
    pub alternate_scroll: bool,
}

impl MouseModes {
    pub fn from_mode(mode: TermMode) -> Self {
        MouseModes {
            report: mode.intersects(TermMode::MOUSE_MODE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        }
    }
}

impl Default for MouseModes {
    /// `alternate_scroll` is `true`: `TermMode::default()` already sets `ALTERNATE_SCROLL`
    /// (`alacritty_terminal::term::mod.rs`'s own `Default for TermMode`), so a program that has set
    /// no mode at all still gets `less`/`man` scrolling by the wheel.
    fn default() -> Self {
        MouseModes::from_mode(TermMode::default())
    }
}

/// How many reports one wheel notch is worth for [`alternate_scroll`]: xterm/foot/alacritty all
/// send three arrow keys per notch in the alternate screen (owner ruling R6, "wheel: 3 lines a
/// notch" -- the same multiplier `shell`'s own scrollback wheel handling uses, kept in one place so
/// the two never drift apart).
pub const LINES_PER_NOTCH: usize = 3;

/// The X10/1000 (no SGR) coordinate limit: a coordinate at or past this cannot be encoded as one
/// byte (`33 + coord` would be `>= 256`) and the whole report is dropped -- never wrapped
/// (xterm's own limitation; alacritty's `normal_mouse_report` does the same).
const X10_LIMIT: u16 = 223;

/// The UTF-8 (1005) coordinate limit (xterm ctlseqs, "UTF-8 (1005)").
const UTF8_LIMIT: u16 = 2015;

/// `Some(base_code_before_mods, is_release_action)`. Shared between the SGR and normal encodings:
/// SGR adds mods to `base` for both a press and a release (and marks the release with `m` instead of
/// `M`, still naming the real button); the normal encoding can only ever report release as a fixed
/// code (`3 + mods`), so it uses `is_release` and ignores `base` for that case.
fn base_and_release(kind: MouseKind) -> (u16, bool) {
    match kind {
        MouseKind::Press(button) => (button.code(), false),
        MouseKind::Release(button) => (button.code(), true),
        MouseKind::Motion(button) => (32 + button.map_or(3, Button::code), false),
        MouseKind::Wheel(dir) => (dir.code(), false),
    }
}

/// Whether `mode` asks to be told about `kind` at all. Deliberately not a bare check of
/// `MOUSE_REPORT_CLICK`: alacritty's own `set_private_mode` makes 1000/1002/1003 mutually exclusive
/// private modes, so a terminal that asked only for 1002 (drag) has `MOUSE_DRAG` set and
/// `MOUSE_REPORT_CLICK` clear -- a check on that one bit alone would silently drop every click a
/// program like `htop` sends under 1002 alone.
fn wants(kind: MouseKind, mode: TermMode) -> bool {
    match kind {
        MouseKind::Press(_) | MouseKind::Release(_) | MouseKind::Wheel(_) => mode.intersects(TermMode::MOUSE_MODE),
        MouseKind::Motion(button) => {
            mode.contains(TermMode::MOUSE_MOTION) || (mode.contains(TermMode::MOUSE_DRAG) && button.is_some())
        }
    }
}

/// Encodes one mouse event against the live `mode`. `None`: the mode does not ask for it (`wants`
/// says so), or the coordinate cannot be encoded in the active (non-SGR) protocol and would
/// otherwise wrap (`X10_LIMIT`/`UTF8_LIMIT`) -- the report is dropped, never sent with a wrapped
/// coordinate.
pub fn encode_mouse(input: &MouseInput, mode: TermMode) -> Option<Vec<u8>> {
    if !wants(input.kind, mode) {
        return None;
    }
    let (base, is_release) = base_and_release(input.kind);
    let mods = input.mods.bits();
    if mode.contains(TermMode::SGR_MOUSE) {
        let code = base + mods;
        let action = if is_release { 'm' } else { 'M' };
        Some(format!("\x1b[<{code};{};{}{action}", input.col + 1, input.line + 1).into_bytes())
    } else {
        let code = if is_release { 3 + mods } else { base + mods };
        let utf8 = mode.contains(TermMode::UTF8_MOUSE);
        let limit = if utf8 { UTF8_LIMIT } else { X10_LIMIT };
        if input.col >= limit || input.line >= limit {
            return None;
        }
        let mut bytes = vec![0x1b, b'[', b'M', 32 + code as u8];
        push_coord(&mut bytes, input.col, utf8);
        push_coord(&mut bytes, input.line, utf8);
        Some(bytes)
    }
}

/// One normal-encoding coordinate: `33 + coord`, as one raw byte if it fits (always true when
/// `utf8` is false, since the caller already checked `X10_LIMIT`), else as that code point's UTF-8
/// encoding (1005).
fn push_coord(bytes: &mut Vec<u8>, coord: u16, utf8: bool) {
    let v = 33u32 + u32::from(coord);
    if utf8 && v >= 128 {
        let mut buf = [0u8; 4];
        if let Some(ch) = char::from_u32(v) {
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    } else {
        bytes.push(v as u8);
    }
}

/// Focus reporting (DECSET 1004): `None` unless the mode asked for it.
pub fn encode_focus(focused: bool, mode: TermMode) -> Option<&'static [u8]> {
    if !mode.contains(TermMode::FOCUS_IN_OUT) {
        return None;
    }
    Some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

/// A wheel notch scrolled locally in the alternate screen (`less`, `man`, anything without a mouse
/// mode of its own): `notches` groups of [`LINES_PER_NOTCH`] arrow-key sequences (`CSI A`/`CSI B`,
/// or `SS3 A`/`B` under `APP_CURSOR` -- xterm's DECCKM convention). `None`: not the alternate screen,
/// `ALTERNATE_SCROLL` (1007) is off, a mouse mode is on (the program gets a real wheel report
/// instead, `encode_mouse`'s job), or `dir` is not vertical.
pub fn alternate_scroll(dir: WheelDir, notches: u16, mode: TermMode, alt_screen: bool) -> Option<Vec<u8>> {
    if !alt_screen || !mode.contains(TermMode::ALTERNATE_SCROLL) || mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let letter: u8 = match dir {
        WheelDir::Up => b'A',
        WheelDir::Down => b'B',
        WheelDir::Left | WheelDir::Right => return None,
    };
    let seq: [u8; 3] = if mode.contains(TermMode::APP_CURSOR) {
        [0x1b, b'O', letter]
    } else {
        [0x1b, b'[', letter]
    };
    let reps = LINES_PER_NOTCH * usize::from(notches);
    let mut out = Vec::with_capacity(seq.len() * reps);
    for _ in 0..reps {
        out.extend_from_slice(&seq);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_MODS: MouseMods = MouseMods {
        shift: false,
        alt: false,
        ctrl: false,
    };

    fn input(kind: MouseKind, line: u16, col: u16, mods: MouseMods) -> MouseInput {
        MouseInput { kind, line, col, mods }
    }

    #[test]
    fn no_mouse_mode_reports_nothing() {
        let input = input(MouseKind::Press(Button::Left), 0, 0, NO_MODS);
        assert_eq!(encode_mouse(&input, TermMode::empty()), None);
    }

    #[test]
    fn sgr_press_release_and_a_modified_right_click() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let press = input(MouseKind::Press(Button::Left), 2, 4, NO_MODS);
        assert_eq!(encode_mouse(&press, mode).unwrap(), b"\x1b[<0;5;3M");
        let release = input(MouseKind::Release(Button::Left), 2, 4, NO_MODS);
        assert_eq!(encode_mouse(&release, mode).unwrap(), b"\x1b[<0;5;3m");
        let ctrl_right = input(
            MouseKind::Press(Button::Right),
            2,
            4,
            MouseMods { ctrl: true, ..NO_MODS },
        );
        assert_eq!(encode_mouse(&ctrl_right, mode).unwrap(), b"\x1b[<18;5;3M");
        let wheel = input(MouseKind::Wheel(WheelDir::Up), 2, 4, NO_MODS);
        assert_eq!(encode_mouse(&wheel, mode).unwrap(), b"\x1b[<64;5;3M");
    }

    #[test]
    fn sgr_wheel_down_and_alt_modified_press() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let wheel = input(MouseKind::Wheel(WheelDir::Down), 2, 4, NO_MODS);
        assert_eq!(encode_mouse(&wheel, mode).unwrap(), b"\x1b[<65;5;3M");
        let alt_press = input(MouseKind::Press(Button::Left), 2, 4, MouseMods { alt: true, ..NO_MODS });
        assert_eq!(encode_mouse(&alt_press, mode).unwrap(), b"\x1b[<8;5;3M");
    }

    #[test]
    fn x10_press_and_release_without_sgr() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        let press = input(MouseKind::Press(Button::Left), 2, 4, NO_MODS);
        assert_eq!(encode_mouse(&press, mode).unwrap(), b"\x1b[M\x20\x25\x23");
        let release = input(MouseKind::Release(Button::Left), 2, 4, NO_MODS);
        assert_eq!(encode_mouse(&release, mode).unwrap(), b"\x1b[M\x23\x25\x23");
    }

    #[test]
    fn x10_boundary_col_222_is_the_last_encodable() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        let press = input(MouseKind::Press(Button::Left), 0, 222, NO_MODS);
        let bytes = encode_mouse(&press, mode).unwrap();
        assert_eq!(*bytes.last().unwrap(), 33, "line byte unchanged (33 + line 0)");
        assert_eq!(bytes[4], 0xFF, "33 + 222 = 255 = 0xFF, still one byte");
    }

    #[test]
    fn x10_col_223_cannot_be_encoded_and_is_dropped() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        let press = input(MouseKind::Press(Button::Left), 0, 223, NO_MODS);
        assert_eq!(encode_mouse(&press, mode), None, "33 + 223 = 256 would wrap a u8");
        let line_over = input(MouseKind::Press(Button::Left), 223, 0, NO_MODS);
        assert_eq!(encode_mouse(&line_over, mode), None, "the line limit is the same");
    }

    #[test]
    fn utf8_mouse_encodes_col_223_as_two_utf8_bytes() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        let press = input(MouseKind::Press(Button::Left), 2, 223, NO_MODS);
        // 33 + 223 = 256 = U+0100, UTF-8 0xC4 0x80.
        assert_eq!(encode_mouse(&press, mode).unwrap(), b"\x1b[M\x20\xc4\x80\x23");
    }

    #[test]
    fn sgr_has_no_coordinate_limit() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let press = input(MouseKind::Press(Button::Left), 2, 300, NO_MODS);
        assert_eq!(encode_mouse(&press, mode).unwrap(), b"\x1b[<0;301;3M");
    }

    #[test]
    fn motion_is_gated_by_1002_or_1003_not_1000_alone() {
        let click_only = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let with_button = input(MouseKind::Motion(Some(Button::Left)), 2, 4, NO_MODS);
        assert_eq!(
            encode_mouse(&with_button, click_only),
            None,
            "1000 alone never reports motion"
        );

        let drag = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        assert_eq!(
            encode_mouse(&with_button, drag).unwrap(),
            b"\x1b[<32;5;3M",
            "1002 with a held button reports"
        );
        let no_button = input(MouseKind::Motion(None), 2, 4, NO_MODS);
        assert_eq!(encode_mouse(&no_button, drag), None, "1002 needs a held button");

        let any_motion = TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE;
        assert_eq!(
            encode_mouse(&no_button, any_motion).unwrap(),
            b"\x1b[<35;5;3M",
            "1003 reports motion with no button held"
        );
    }

    /// Alacritty's own `set_private_mode` makes 1000/1002/1003 mutually exclusive: a terminal that
    /// asked only for 1002 has `MOUSE_REPORT_CLICK` clear. A press must still be reported.
    #[test]
    fn a_press_is_reported_under_1002_alone() {
        let mode = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        let press = input(MouseKind::Press(Button::Left), 0, 0, NO_MODS);
        assert_eq!(encode_mouse(&press, mode).unwrap(), b"\x1b[<0;1;1M");
    }

    #[test]
    fn encode_focus_only_under_1004() {
        assert_eq!(encode_focus(true, TermMode::empty()), None);
        let mode = TermMode::FOCUS_IN_OUT;
        assert_eq!(encode_focus(true, mode), Some(&b"\x1b[I"[..]));
        assert_eq!(encode_focus(false, mode), Some(&b"\x1b[O"[..]));
    }

    #[test]
    fn alternate_scroll_repeats_arrows_per_notch() {
        let mode = TermMode::ALTERNATE_SCROLL;
        let bytes = alternate_scroll(WheelDir::Up, 2, mode, true).unwrap();
        assert_eq!(bytes, b"\x1b[A".repeat(6));
    }

    #[test]
    fn alternate_scroll_under_app_cursor_sends_ss3() {
        let mode = TermMode::ALTERNATE_SCROLL | TermMode::APP_CURSOR;
        let bytes = alternate_scroll(WheelDir::Up, 2, mode, true).unwrap();
        assert_eq!(bytes, b"\x1bOA".repeat(6));
    }

    #[test]
    fn alternate_scroll_is_none_off_the_alternate_screen() {
        let mode = TermMode::ALTERNATE_SCROLL;
        assert_eq!(alternate_scroll(WheelDir::Up, 1, mode, false), None);
    }

    #[test]
    fn alternate_scroll_is_none_when_a_mouse_mode_is_on() {
        let mode = TermMode::ALTERNATE_SCROLL | TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            alternate_scroll(WheelDir::Up, 1, mode, true),
            None,
            "the program gets the wheel instead"
        );
    }

    #[test]
    fn mouse_modes_default_has_alternate_scroll() {
        let modes = MouseModes::default();
        assert!(
            modes.alternate_scroll,
            "TermMode::default() already sets ALTERNATE_SCROLL"
        );
        assert!(!modes.report);
        assert!(!modes.alt_screen);
    }

    #[test]
    fn mouse_modes_from_mode_reads_all_three() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::ALT_SCREEN;
        let modes = MouseModes::from_mode(mode);
        assert!(modes.report);
        assert!(modes.alt_screen);
        assert!(!modes.alternate_scroll, "ALTERNATE_SCROLL bit is not set here");
    }
}
