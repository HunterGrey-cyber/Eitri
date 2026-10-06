//! Focus handoff from the editor through `vim-tmux-navigator`.
//! Neovim first runs `wincmd h` or `wincmd l` between its own splits. Only when the
//! window number stays unchanged does the plugin call `tmux select-pane`. This keeps
//! pane focus changes from interrupting navigation within the editor.
//!
//! `TMUX`, `TMUX_PANE` and a shim-prefixed `PATH` are set only on the embedded
//! Neovim child. Process-global environment changes would leak into agent subprocesses
//! and race concurrent environment reads. Other terminal sessions keep their own environment.
//! The shim is exposed as `tmux` in a private directory and sends direction letters
//! through a Unix socket. The core pane-switch module owns the directory, socket,
//! parser and cleanup so each GUI can supply its own event loop.
//!
//! This module locates the shim and supplies the focus grabs wired in `main.rs`.
//! `editor_feeds::FeedPump` drains the socket.
//!
//! Messages arrive while the editor has focus, after Neovim reaches a split boundary.
//! Each letter maps to a [`Direction`] through [`letter_direction`]; layout geometry
//! selects the neighbouring module. With the default `[editor | agent]` layout, `R`
//! focuses the agent, `U` focuses the top bar, and `L` or `D` at an edge does nothing.
//! A module below the editor is reachable with `D`. A shown terminal is unzoomed and
//! focused; a hidden terminal is absent from the geometry, so `D` does nothing there.
//!
//! Navigation from a focused web module uses a GTK capture-phase key handler on its
//! host ([`nav_direction`], wired in `main.rs`) and consults the same geometry.
//!
//! If the shim cannot be found or the socket cannot be bound, [`open`] returns `None`.
//! No environment is injected, and the plugin uses plain `wincmd` navigation without
//! calling tmux. The editor continues to work without pane focus handoff.

use std::path::{Path, PathBuf};

use gtk4::gdk::{Key, ModifierType};

use crate::layout::Direction;
use eitri_core::pane_switch::sweep_stale_dirs;
pub(crate) use eitri_core::pane_switch::{PaneMessage, PaneSwitchChannel};

/// The direction a shim letter names: `vim-tmux-navigator`'s `select-pane -L/-R/-U/-D`. Anything
/// else is not a direction (the shim only ever sends these four).
pub(crate) fn letter_direction(letter: char) -> Option<Direction> {
    match letter {
        'L' => Some(Direction::Left),
        'R' => Some(Direction::Right),
        'U' => Some(Direction::Up),
        'D' => Some(Direction::Down),
        _ => None,
    }
}

/// `Ctrl+h/j/k/l` in a web module or the bottom terminal (`terminal::navigation` is this): Control
/// held and nothing else but CapsLock -- `Shift`, `Alt`, `Super` or `Meta` held means it is not a
/// move, so a page's (or a terminal program's) `Ctrl+Shift+K` or `Ctrl+Alt+h` stays its own.
///
/// **The bottom terminal's exact-Ctrl rule since it was hosted as a module**.
/// Until then this matched the key itself, lower case, with Control held and anything else ignored:
/// the rule the agent panel's controller had always used for `Ctrl+h`/`Ctrl+k`. `main`'s terminal
/// review replaced that for the panel's `Ctrl+j` into the terminal with this one, because it
/// matched CapsLock's `Key::J` never and `Ctrl+Alt+j` always.
/// One controller asks the geometry for all four chords now, so they share the rule: CapsLock no
/// longer turns `Ctrl+h/j/k/l` in a web module into a key for the page, and `Ctrl+Alt+h/k/l` no
/// longer moves.
pub(crate) fn nav_direction(key: Key, state: ModifierType) -> Option<Direction> {
    let others = ModifierType::SHIFT_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK | ModifierType::META_MASK;
    if !state.contains(ModifierType::CONTROL_MASK) || state.intersects(others) {
        return None;
    }
    // GDK delivers the UPPERCASE keyval under CapsLock, not `Key::k` with `LOCK_MASK` alone.
    match key.to_lower() {
        Key::h => Some(Direction::Left),
        Key::j => Some(Direction::Down),
        Key::k => Some(Direction::Up),
        Key::l => Some(Direction::Right),
        _ => None,
    }
}

/// How many let-through key events [`LetThrough`] remembers. A page that handles a key never sends
/// it back, so its entry stays until this many later ones push it out.
const LET_THROUGH_REMEMBERED: usize = 16;

/// Remembers key events passed to a web module's page so they can be recognised on return.
///
/// WebKit handles keys asynchronously. If the page leaves a key unhandled, WebKitGTK
/// queues the same `GdkEvent` again so GTK bindings can process it. The second dispatch
/// starts at the window and reaches the host's capture-phase controller again.
/// For example, when `move_focus` returns `false` and the page leaves `Ctrl+l` unhandled,
/// the same chord returns and could log the missing neighbour twice without moving focus.
///
/// Retains a reference to each `gdk::Event` so its address cannot be reused while
/// remembered, and compares events by identity. Several events can be outstanding
/// because another chord can reach the page before the first one returns.
#[derive(Debug)]
pub(crate) struct LetThrough<E> {
    outstanding: std::collections::VecDeque<E>,
}

impl<E: PartialEq> LetThrough<E> {
    pub(crate) fn new() -> Self {
        Self {
            outstanding: std::collections::VecDeque::new(),
        }
    }

    /// Remembers `event` as let through to the page.
    pub(crate) fn let_through(&mut self, event: E) {
        if self.outstanding.len() == LET_THROUGH_REMEMBERED {
            self.outstanding.pop_front();
        }
        self.outstanding.push_back(event);
    }

    /// `true` if `event` was let through and this is WebKit handing it back. Forgets it: WebKit
    /// hands an event back at most once.
    pub(crate) fn is_second_delivery(&mut self, event: &E) -> bool {
        match self.outstanding.iter().position(|e| e == event) {
            Some(at) => {
                self.outstanding.remove(at);
                true
            }
            None => false,
        }
    }
}

/// A `gdk::Event` compared by identity, which is what [`LetThrough`] needs: WebKit hands back the
/// same event object, and `gdk::Event` itself has no `PartialEq`.
pub(crate) struct SameEvent(pub(crate) gtk4::gdk::Event);

impl PartialEq for SameEvent {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_ptr() == other.0.as_ptr()
    }
}

/// Reclaims stale directories, finds the shim binary and binds the channel, or returns `None`
/// having logged why.
///
/// The sweep is unconditional and first, so it still happens on a machine where the shim binary is
/// missing and this call is about to return `None`.
pub(crate) fn open() -> Option<PaneSwitchChannel> {
    sweep_stale_dirs();
    let shim = locate_shim_binary()?;
    PaneSwitchChannel::bind(&shim)
}

/// Finds the `eitri-tmux-shim` binary next to the currently-running executable. Both are
/// binaries of the same Cargo package, so they always land in the same directory -- `target/debug`
/// during development, a single `bin/` directory for any real install.
///
/// Stays in `shell` rather than moving into the core protocol with everything else: which binary
/// plays `tmux`, and where a host keeps it, is that host's packaging, not the wire.
fn locate_shim_binary() -> Option<PathBuf> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("[pane_switch] current_exe() failed: {e} -- Ctrl+h/j/k/l out of nvim disabled");
            return None;
        }
    };
    let candidate = exe.parent()?.join("eitri-tmux-shim");
    if !Path::new(&candidate).is_file() {
        eprintln!(
            "[pane_switch] {} not found -- Ctrl+h/j/k/l out of nvim disabled (build it with `cargo build -p shell`)",
            candidate.display()
        );
        return None;
    }
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shims_four_letters_are_the_four_directions() {
        assert_eq!(letter_direction('L'), Some(Direction::Left));
        assert_eq!(letter_direction('R'), Some(Direction::Right));
        assert_eq!(letter_direction('U'), Some(Direction::Up));
        assert_eq!(letter_direction('D'), Some(Direction::Down));
        assert_eq!(letter_direction('r'), None);
        assert_eq!(letter_direction('X'), None);
    }

    #[test]
    fn only_control_with_hjkl_and_nothing_else_held_moves() {
        let ctrl = ModifierType::CONTROL_MASK;
        assert_eq!(nav_direction(Key::h, ctrl), Some(Direction::Left));
        assert_eq!(nav_direction(Key::j, ctrl), Some(Direction::Down));
        assert_eq!(nav_direction(Key::k, ctrl), Some(Direction::Up));
        assert_eq!(nav_direction(Key::l, ctrl), Some(Direction::Right));
        assert_eq!(
            nav_direction(Key::k, ModifierType::empty()),
            None,
            "plain k is a letter"
        );
        assert_eq!(nav_direction(Key::K, ctrl | ModifierType::SHIFT_MASK), None);
        assert_eq!(nav_direction(Key::a, ctrl), None, "Ctrl+a is the prefix's");
        // The terminal's exact-Ctrl rule, now every
        // web module's: CapsLock is not a chord, and it delivers the uppercase keyval.
        assert_eq!(
            nav_direction(Key::J, ctrl | ModifierType::LOCK_MASK),
            Some(Direction::Down),
            "CapsLock delivers the uppercase keyval"
        );
        for held in [
            ModifierType::ALT_MASK,
            ModifierType::SUPER_MASK,
            ModifierType::META_MASK,
        ] {
            assert_eq!(nav_direction(Key::j, ctrl | held), None, "{held:?} held: the page's");
        }
    }

    /// A chord let through to the page comes back once, and is known when it does -- so the
    /// controller does not decide it (and log it) a second time. Identity stands in for `gdk::Event`'s
    /// pointer equality here: `1` is one press, `2` another.
    #[test]
    fn a_key_let_through_to_the_page_is_known_when_webkit_hands_it_back() {
        let mut seen = LetThrough::new();
        assert!(!seen.is_second_delivery(&1), "the first delivery is new");
        seen.let_through(1);
        assert!(seen.is_second_delivery(&1), "WebKit put it back");
        assert!(
            !seen.is_second_delivery(&1),
            "forgotten once handed back: WebKit hands an event back at most once"
        );
        assert!(
            !seen.is_second_delivery(&2),
            "a press never let through never comes back"
        );
    }

    /// The round trip through the web process is asynchronous: a second chord can be let through
    /// before the first comes back, and both are still known, in either order.
    #[test]
    fn two_chords_in_flight_are_each_known_when_they_come_back() {
        let mut seen = LetThrough::new();
        seen.let_through(1);
        seen.let_through(2);
        assert!(seen.is_second_delivery(&2));
        assert!(seen.is_second_delivery(&1));
        assert!(!seen.is_second_delivery(&1));
    }

    /// A key the page handled never comes back; it is pushed out by later ones instead of being
    /// held for the life of the window.
    #[test]
    fn a_key_the_page_handled_is_forgotten_after_enough_later_ones() {
        let mut seen = LetThrough::new();
        seen.let_through(0);
        for press in 1..=LET_THROUGH_REMEMBERED {
            seen.let_through(press);
        }
        assert!(!seen.is_second_delivery(&0), "pushed out");
        assert!(seen.is_second_delivery(&1), "the oldest still remembered");
        assert!(seen.is_second_delivery(&LET_THROUGH_REMEMBERED), "the newest");
    }
}
