//! `terminal-input` -- a `TermMode`-aware keyboard and paste encoder.
//!
//! Turns a [`NormalizedInput`] plus the authoritative terminal's current
//! [`TermMode`](alacritty_terminal::term::TermMode) into the bytes to write to
//! the PTY.
//!
//! The encoder core is vendored verbatim from Alacritty (Apache-2.0; see
//! `LICENSE-APACHE` and `NOTICE` at the crate root, and the provenance header on
//! [`vendored::build_sequence`]). Everything around it -- the regime layer -- is
//! in [`regime`], which is where the real behaviour lives; read its module docs
//! before changing anything.
//!
//! ```
//! use alacritty_terminal::term::TermMode;
//! use terminal_input::keys::{Key, KeyEvent, ModifiersState, NamedKey};
//! use terminal_input::encode_key;
//!
//! // Plain Backspace in a legacy terminal.
//! let key = KeyEvent::press(Key::Named(NamedKey::Backspace));
//! assert_eq!(encode_key(&key, ModifiersState::empty(), TermMode::default()), b"\x7f");
//!
//! // The matching RELEASE emits nothing, even with event reporting on.
//! let mode = TermMode::default() | TermMode::REPORT_EVENT_TYPES;
//! let up = KeyEvent::release(Key::Named(NamedKey::Backspace));
//! assert_eq!(encode_key(&up, ModifiersState::empty(), mode), b"");
//! ```

pub mod keys;
pub mod regime;
pub mod vendored;

pub use regime::{encode, encode_key, encode_paste, NormalizedInput};

/// Whether a key event is a bare modifier press (`Shift`, `Control`, `Alt`,
/// `Super`), regardless of side.
///
/// Vendored from upstream (`keyboard.rs:133-140`) and re-exported because the
/// engine needs it for the same reason upstream does: a bare modifier press must
/// NOT count as terminal input, so it must not scroll the viewport back to the
/// bottom or clear the selection. Upstream guards
/// `ctx.on_terminal_input_start()` with it (`keyboard.rs:96-99`).
pub use vendored::is_modifier_key;
