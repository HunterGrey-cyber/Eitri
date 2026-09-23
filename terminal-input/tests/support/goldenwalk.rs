//! The single definition of how the golden digests are computed, shared by the
//! generator (`examples/gen_golden.rs`) and the checker (`tests/golden.rs`) so
//! the two can never drift apart.
#![allow(dead_code)]

use super::digest::Fnv;
use super::sweep;

/// One line per mode combination: `<mode label> <fnv1a-64 hex>`.
///
/// The digest covers, for that mode, every modifier set x key shape x state, in
/// a fixed order, hashing the case label and the produced bytes. Any change to
/// any produced byte changes the line.
pub fn golden_lines() -> Vec<String> {
    let mod_sets = sweep::mod_sets();
    let shapes = sweep::shapes();
    let mut out = Vec::new();

    for (mode_label, mode) in sweep::modes() {
        let mut h = Fnv::default();
        for (mods_label, mods) in &mod_sets {
            for shape in &shapes {
                for (state_label, state, repeat) in sweep::STATES {
                    let event = shape.event(state, repeat);
                    let bytes = terminal_input::encode_key(&event, *mods, mode);
                    h.write(mods_label.as_bytes());
                    h.write(b"\x00");
                    h.write(shape.label.as_bytes());
                    h.write(b"\x00");
                    h.write(state_label.as_bytes());
                    h.write(b"\x00");
                    h.write(&bytes);
                    h.write(b"\x00");
                }
            }
        }
        out.push(format!("{mode_label} {}", h.hex()));
    }

    // Paste is part of the encoder surface, so it gets a line too.
    let mut h = Fnv::default();
    for (_, mode) in sweep::modes() {
        for extra in [
            alacritty_terminal::term::TermMode::empty(),
            alacritty_terminal::term::TermMode::BRACKETED_PASTE,
        ] {
            for text in ["", "a", "x\ny", "x\r\ny", "\x1b[201~evil\n", "\x03ctrl-c"] {
                for bracketed in [true, false] {
                    h.write(&terminal_input::encode_paste(text, mode | extra, bracketed));
                    h.write(b"\x00");
                }
            }
        }
    }
    out.push(format!("__paste__ {}", h.hex()));

    out
}
