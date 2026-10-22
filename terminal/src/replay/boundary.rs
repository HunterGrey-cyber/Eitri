//! Whether vte's parser stands between two sequences. vte keeps its state private and no handler call marks the end
//! of a sequence (a C0 control inside a CSI is executed while the parser still waits for the final byte; an ignored
//! CSI or an SOS, PM or APC string ends with no call at all), so this is a copy of vte 0.15's state transitions with
//! the actions left out, fed the same bytes, one at a time as the terminal feeds them. A vte upgrade that changes
//! the transitions fails this module's property test.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsPassthrough,
    DcsIgnore,
    OscString,
    SosPmApcString,
}

#[derive(Debug, Clone, Default)]
pub struct BoundaryTracker {
    state: State,
    partial: [u8; 4],
    partial_len: usize,
}

/// A C0 control vte executes in place in most states (everything below 0x20 but CAN, SUB and ESC).
fn executed_c0(b: u8) -> bool {
    matches!(b, 0x00..=0x17 | 0x19 | 0x1C..=0x1F)
}

impl BoundaryTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// In the ground state with no partial UTF-8 character: a cut here loses nothing.
    pub fn at_boundary(&self) -> bool {
        self.state == State::Ground && self.partial_len == 0
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    /// One byte, as vte's `advance` takes a one-byte slice.
    pub fn byte(&mut self, b: u8) {
        if self.partial_len != 0 && self.partial_utf8(b) {
            return;
        }
        match self.state {
            State::Ground => self.ground(b),
            _ => self.change_state(b),
        }
    }

    /// vte's `advance_partial_utf8` with a one-byte slice; true when it consumed the byte.
    fn partial_utf8(&mut self, b: u8) -> bool {
        let old = self.partial_len;
        if old >= self.partial.len() {
            self.partial_len = 0;
            return false;
        }
        self.partial[old] = b;
        self.partial_len += 1;
        match std::str::from_utf8(&self.partial[..self.partial_len]) {
            Ok(text) => {
                let c = text.chars().next().expect("a valid non-empty string");
                self.partial_len = 0;
                c.len_utf8() > old
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if valid > 0 {
                    self.partial_len = 0;
                    return valid > old;
                }
                match err.error_len() {
                    Some(invalid) => {
                        self.partial_len = 0;
                        invalid > old
                    }
                    None => true,
                }
            }
        }
    }

    /// vte's `advance_ground` with a one-byte slice.
    fn ground(&mut self, b: u8) {
        if b == 0x1B {
            self.state = State::Escape;
            return;
        }
        if b < 0x80 {
            return;
        }
        match std::str::from_utf8(&[b]) {
            Ok(_) => {}
            Err(err) => {
                if err.error_len().is_none() {
                    self.partial[0] = b;
                    self.partial_len = 1;
                }
            }
        }
    }

    fn change_state(&mut self, b: u8) {
        use State::*;
        match self.state {
            CsiEntry => match b {
                b if executed_c0(b) => {}
                0x20..=0x2F => self.state = CsiIntermediate,
                0x30..=0x3F => self.state = CsiParam,
                0x40..=0x7E => self.state = Ground,
                _ => self.anywhere(b),
            },
            CsiIgnore => match b {
                b if executed_c0(b) => {}
                0x20..=0x3F | 0x7F => {}
                0x40..=0x7E => self.state = Ground,
                _ => self.anywhere(b),
            },
            CsiIntermediate => match b {
                b if executed_c0(b) => {}
                0x20..=0x2F => {}
                0x30..=0x3F => self.state = CsiIgnore,
                0x40..=0x7E => self.state = Ground,
                _ => self.anywhere(b),
            },
            CsiParam => match b {
                b if executed_c0(b) => {}
                0x20..=0x2F => self.state = CsiIntermediate,
                0x30..=0x3B | 0x7F => {}
                0x3C..=0x3F => self.state = CsiIgnore,
                0x40..=0x7E => self.state = Ground,
                _ => self.anywhere(b),
            },
            DcsEntry => match b {
                b if executed_c0(b) => {}
                0x20..=0x2F => self.state = DcsIntermediate,
                0x30..=0x3F => self.state = DcsParam,
                0x40..=0x7E => self.state = DcsPassthrough,
                0x7F => {}
                _ => self.anywhere(b),
            },
            DcsIntermediate => match b {
                b if executed_c0(b) => {}
                0x20..=0x2F | 0x7F => {}
                0x30..=0x3F => self.state = DcsIgnore,
                0x40..=0x7E => self.state = DcsPassthrough,
                _ => self.anywhere(b),
            },
            DcsParam => match b {
                b if executed_c0(b) => {}
                0x20..=0x2F => self.state = DcsIntermediate,
                0x30..=0x3B | 0x7F => {}
                0x3C..=0x3F => self.state = DcsIgnore,
                0x40..=0x7E => self.state = DcsPassthrough,
                _ => self.anywhere(b),
            },
            DcsPassthrough => match b {
                0x18 | 0x1A | 0x9C => self.state = Ground,
                0x1B => self.state = Escape,
                _ => {}
            },
            DcsIgnore | SosPmApcString => self.anywhere(b),
            Escape => match b {
                b if executed_c0(b) => {}
                0x20..=0x2F => self.state = EscapeIntermediate,
                0x50 => self.state = DcsEntry,
                0x58 | 0x5E | 0x5F => self.state = SosPmApcString,
                0x5B => self.state = CsiEntry,
                0x5D => self.state = OscString,
                0x30..=0x7E => self.state = Ground,
                0x18 | 0x1A => self.state = Ground,
                _ => {}
            },
            EscapeIntermediate => match b {
                b if executed_c0(b) => {}
                0x20..=0x2F | 0x7F => {}
                0x30..=0x7E => self.state = Ground,
                _ => self.anywhere(b),
            },
            OscString => match b {
                0x07 | 0x18 | 0x1A => self.state = Ground,
                0x1B => self.state = Escape,
                _ => {}
            },
            Ground => unreachable!("ground is handled before change_state"),
        }
    }

    /// vte's `anywhere`: CAN and SUB end any sequence, ESC starts a new one, anything else is ignored.
    fn anywhere(&mut self, b: u8) {
        match b {
            0x18 | 0x1A => self.state = State::Ground,
            0x1B => self.state = State::Escape,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::{Params, Parser, Perform};

    /// Every call vte makes, as text, so two runs compare.
    #[derive(Default)]
    struct Calls(Vec<String>);

    fn params(p: &Params) -> Vec<Vec<u16>> {
        p.iter().map(|s| s.to_vec()).collect()
    }

    impl Perform for Calls {
        fn print(&mut self, c: char) {
            self.0.push(format!("print {c:?}"));
        }
        fn execute(&mut self, b: u8) {
            self.0.push(format!("execute {b}"));
        }
        fn hook(&mut self, p: &Params, i: &[u8], ignore: bool, a: char) {
            self.0.push(format!("hook {:?} {i:?} {ignore} {a:?}", params(p)));
        }
        fn put(&mut self, b: u8) {
            self.0.push(format!("put {b}"));
        }
        fn unhook(&mut self) {
            self.0.push("unhook".into());
        }
        fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
            self.0.push(format!("osc {p:?} {bell}"));
        }
        fn csi_dispatch(&mut self, p: &Params, i: &[u8], ignore: bool, a: char) {
            self.0.push(format!("csi {:?} {i:?} {ignore} {a:?}", params(p)));
        }
        fn esc_dispatch(&mut self, i: &[u8], ignore: bool, b: u8) {
            self.0.push(format!("esc {i:?} {ignore} {b}"));
        }
    }

    fn run(bytes: &[u8]) -> Vec<String> {
        let mut parser = Parser::new();
        let mut calls = Calls::default();
        for &b in bytes {
            parser.advance(&mut calls, &[b]);
        }
        calls.0
    }

    /// A deterministic generator, so a failure names its seed.
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<T: Copy>(&mut self, from: &[T]) -> T {
            from[(self.next() % from.len() as u64) as usize]
        }
    }

    /// Streams rich in sequence starts, controls, string terminators and UTF-8 fragments.
    fn stream(rng: &mut XorShift, len: usize) -> Vec<u8> {
        let pieces: &[&[u8]] = &[
            b"\x1b",
            b"[",
            b"]",
            b"P",
            b"X",
            b"^",
            b"_",
            b"\\",
            b"0;",
            b"31",
            b";",
            b":",
            b"?",
            b"m",
            b"H",
            b" ",
            b"!",
            b"q",
            b"\x07",
            b"\x18",
            b"\x1a",
            b"\n",
            b"\r",
            b"\x7f",
            b"\x9c",
            b"\x90",
            b"a",
            b"\xe4\xbd\xa0",
            b"\xe4",
            b"\xbd",
            b"\xf0\x9f\x98\x80",
            b"\xc3",
            b"\xff",
            b"\x80",
        ];
        let mut out = Vec::new();
        while out.len() < len {
            out.extend_from_slice(rng.pick(pieces));
        }
        out
    }

    #[test]
    fn a_cut_where_the_tracker_says_boundary_loses_and_repeats_no_call() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for round in 0..400 {
            let bytes = stream(&mut rng, 80);
            let whole = run(&bytes);
            let mut tracker = BoundaryTracker::new();
            for cut in 0..=bytes.len() {
                if tracker.at_boundary() {
                    let mut split = run(&bytes[..cut]);
                    split.extend(run(&bytes[cut..]));
                    assert_eq!(split, whole, "round {round}, cut {cut}, bytes {bytes:?}");
                }
                if cut < bytes.len() {
                    tracker.byte(bytes[cut]);
                }
            }
        }
    }

    #[test]
    fn known_sequences_leave_the_tracker_where_vte_is() {
        let cases: &[(&[u8], bool)] = &[
            (b"plain", true),
            (b"\x1b[31", false),
            (b"\x1b[31\n", false),
            (b"\x1b[31\nm", true),
            (b"\x1b[?25h", true),
            (b"\x1b[<1;2;3M", true),
            (b"\x1b]0;title", false),
            (b"\x1b]0;title\x07", true),
            (b"\x1b]0;title\x1b\\", true),
            (b"\x1bPq#0", false),
            (b"\x1bPq#0\x1b\\", true),
            (b"\x1b_apc text", false),
            (b"\x1b_apc text\x1b\\", true),
            (b"\x1b[31\x18", true),
            (b"\xe4\xbd", false),
            (b"\xe4\xbd\xa0", true),
            (b"\x1b(", false),
            (b"\x1b(0", true),
        ];
        for (bytes, boundary) in cases {
            let mut t = BoundaryTracker::new();
            t.feed(bytes);
            assert_eq!(t.at_boundary(), *boundary, "{bytes:?}");
        }
    }

    #[test]
    fn the_terminals_parser_never_buffers_a_synchronized_update() {
        use crate::screen::term_config;
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::Term;
        struct Size;
        impl Dimensions for Size {
            fn total_lines(&self) -> usize {
                24
            }
            fn screen_lines(&self) -> usize {
                24
            }
            fn columns(&self) -> usize {
                80
            }
        }
        let mut term = Term::new(term_config(), &Size, VoidListener);
        let mut driver = terminal_sync::SyncDriver::new();
        driver.feed(&mut term, b"\x1b[?2026hhello", |_, _| {});
        assert_eq!(
            driver.sync_bytes_count(),
            0,
            "the tracker follows exactly the bytes fed"
        );
    }
}
