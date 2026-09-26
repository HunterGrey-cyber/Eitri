//! `Ctrl+h`/`Ctrl+l` focus handoff between the editor pane and the agent panel, implemented by
//! *leveraging* the user's real `christoomey/vim-tmux-navigator` rather than intercepting the
//! keys at the GTK level.
//!
//! This module is the GTK half: locating the shim binary, the `glib` timer that drains the socket,
//! and the focus grabs `main.rs` wires to each direction. The protocol under it -- the private
//! directory, the fake-`tmux` symlink, the socket, the parser, the cleanup -- is
//! `neovibe_core::pane_switch`, moved there in L2 T5 (2026-09-17) so a second host can reuse it
//! with its own run-loop timer.
//!
//! # The mechanism, and why it is shaped this way
//!
//! `vim-tmux-navigator`'s `s:TmuxAwareNavigate` (read in full at
//! `~/.local/share/nvim/lazy/vim-tmux-navigator/plugin/tmux_navigator.vim`, lines 121-158) does
//! three things in order:
//!
//! 1. runs a real `wincmd h`/`wincmd l` -- so navigation between real Neovim `:vsplit` windows
//!    keeps working exactly as it always has;
//! 2. compares `winnr()` before and after, to detect that Neovim had nowhere left to go;
//! 3. **only then**, and only if `$TMUX` is non-empty, shells out to
//!    `tmux -S <socket> select-pane -t <pane> -<L|R|U|D>`.
//!
//! Step 2 is the whole point. A hardcoded GTK-level `Ctrl+l` intercept in `shell` would jump to
//! the agent panel even when the user only meant to move to the split on their right; the
//! boundary decision belongs to Neovim, which is the only party that knows its own window layout.
//! So instead of taking that decision away, `shell` makes the embedded nvim believe it is running
//! inside tmux and turns its resulting `select-pane` call into a focus switch:
//!
//! - `TMUX` and `TMUX_PANE` are set on the `nvim --embed` child process **only** (via
//!   `NeovideEditorPaneOptions::child_env` -> `LiveHarnessOptions::child_env` ->
//!   `CmdLineSettings::child_env` -> `Command::env`), never on this process. `std::env::set_var`
//!   is deliberately not used: it mutates process-global state, would leak into the `claude`
//!   subprocesses `agent` spawns, and races concurrent `getenv` in a multi-threaded GTK app.
//! - `PATH` for that same child is prefixed with a private directory whose only entry is a
//!   symlink named `tmux` pointing at this workspace's own `neovibe-tmux-shim` binary
//!   (`shell/src/bin/neovibe-tmux-shim.rs`).
//! - The shim writes one direction letter to the Unix socket named by
//!   `NEOVIBE_PANE_SWITCH_SOCKET`; [`listen`] polls that socket from the GTK main loop and hands
//!   the letter to the host's callback.
//!
//! Because all three are per-child environment variables, a real terminal + real tmux + real
//! Neovim session on the same machine is untouched -- it is a different process tree with its own
//! real environment.
//!
//! # Directions
//!
//! A message only ever arrives while the *editor* has focus (that is the only context in which
//! the Neovim plugin's mappings are live) and Neovim has already hit its own boundary. Each letter
//! is a [`Direction`] ([`letter_direction`]), and where it goes is geometry, not a table: the
//! neighbouring module that way (`neovibe_core::layout::navigate`, modules design §6.2). For the
//! default `[editor | agent]` that is exactly what the old four arms did -- `R` the agent (the
//! load-bearing case), `U` the top bar, `L`/`D` tmux's no-op at the edge of its grid -- and
//! `navigation_reproduces_todays_dispatch_on_the_default_tree` holds it there. **What changed:**
//! `D` now reaches a module below the editor. It used to reach the native terminal pane, went
//! with it on 2026-09-19 (`freeze/terminal-stack`), and was never wired for a Lua panel in the
//! bottom slot, which was mouse-reachable only until the modules design. The bottom terminal that
//! came back on 2026-09-23 is such a module: shown, `D` unzooms and focuses it, as its own `'D'` arm
//! did on `main` before the modules design re-homed it; hidden, it is not in the geometry and `D`
//! is the edge's no-op again (`main`'s window review, M4, is why `U` above says the top bar).
//!
//! The opposite direction (a web module focused, `Ctrl+h` back to the editor) does **not** go
//! through this mechanism -- the Neovim plugin has no relevance while a `WebView` has focus. That
//! is a GTK capture-phase key handler on each web module's host ([`nav_direction`], wired in
//! `main.rs`), and it asks the same geometry.
//!
//! # Failure mode
//!
//! Every step degrades to "the feature is simply absent": if the shim binary can't be located, or
//! the socket can't be bound, [`open`] returns `None`, no environment is injected, the
//! embedded nvim never sees `$TMUX`, and `vim-tmux-navigator` behaves exactly as it does outside
//! tmux (plain `wincmd` wrappers, no shell-out). Nothing about the editor breaks.

use std::path::{Path, PathBuf};

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;

use crate::layout::Direction;
use neovibe_core::pane_switch::{accept_pending_messages, sweep_stale_dirs, POLL_INTERVAL};
pub(crate) use neovibe_core::pane_switch::{PaneMessage, PaneSwitchChannel};

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
/// **The bottom terminal's exact-Ctrl rule since it was hosted as a module** (modules P1, Task 11).
/// Until then this matched the key itself, lower case, with Control held and anything else ignored:
/// the rule the agent panel's controller had always used for `Ctrl+h`/`Ctrl+k`. `main`'s terminal
/// review replaced that for the panel's `Ctrl+j` into the terminal with this one, because it
/// matched CapsLock's `Key::J` never and `Ctrl+Alt+j` always (review 2026-09-23, task-8 minor 3).
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

/// The key events a web module's `Ctrl+h/j/k/l` controller let through to the page, so it knows
/// each one when WebKit hands it back.
///
/// **WebKit delivers a key the page did not handle twice.** Its key handling is asynchronous: the
/// first delivery goes to the web process, and when the page reports the key unhandled, WebKitGTK
/// puts the SAME `GdkEvent` back on the display's queue so GTK's own bindings get a turn
/// (`webkitWebViewBasePropagateKeyEvent`, `gdk_display_put_event`, WebKitWebViewBase.cpp in
/// 2.52.6). That second dispatch starts at the window again and runs the capture-phase controller
/// on the host again. A chord with nowhere to go is exactly that case: `move_focus` answers
/// `false`, the key goes on to the page, the panel does not handle `Ctrl+l`, and the chord comes
/// back. The GUI pass of 2026-09-23 (item 2) saw "no module that way" logged twice per press.
/// Nothing moved twice, because nothing moved.
///
/// Holds the events themselves (a reference each, for `gdk::Event`), so no address is reused by a
/// later event while it is remembered, and equality is identity. More than one because the
/// round trip through the web process is asynchronous, so a second chord can be let through before
/// the first comes back.
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

/// Starts polling the socket from the GTK main loop, invoking `on_message` with a direction letter
/// or a quit-cancelled generation for each message the shim, or a cancelled `:confirm qall`, sends.
///
/// A poll loop on the main thread rather than a background thread with a blocking `accept()`:
/// the callback has to touch GTK widgets (grabbing focus), which is main-thread-only anyway,
/// and `agent`'s own hook listener already paid for the lesson that a blocking `accept()` with
/// no stop signal is a real hang waiting to happen.
///
/// Takes the channel's listener, so a second call on the same channel logs and does nothing rather
/// than installing a second timer that would race the first for every connection.
pub(crate) fn listen(channel: &mut PaneSwitchChannel, on_message: impl Fn(PaneMessage) + 'static) {
    let Some(listener) = channel.take_listener() else {
        eprintln!("[pane_switch] listen() called twice -- ignoring");
        return;
    };
    glib::timeout_add_local(POLL_INTERVAL, move || {
        for message in accept_pending_messages(&listener) {
            on_message(message);
        }
        glib::ControlFlow::Continue
    });
}

/// Finds the `neovibe-tmux-shim` binary next to the currently-running executable. Both are
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
    let candidate = exe.parent()?.join("neovibe-tmux-shim");
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
        // The terminal's exact-Ctrl rule (`main`'s review 2026-09-23, task-8 minor 3), now every
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
