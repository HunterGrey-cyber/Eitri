//! wire 3 -- a buffer whose file changed on disk catches up.
//!
//! # Why this is needed at all, and why it is not agent-specific
//!
//! Every Neovim plugin's "reload after an external change" story is an autocommand on
//! `FocusGained`. LazyVim's is exactly that: `{"FocusGained", "TermClose", "TermLeave"} -> checktime`.
//!
//! **`FocusGained` can never fire in Eitri.** The editor pane's `EventControllerFocus` drives the
//! input method and nothing else (`neovide-editor/src/keyboard.rs`), so when the user clicks into
//! the agent panel nvim is never told it lost focus -- and therefore never told it got it back.
//! Verified by reading every `focus` site in that crate, not inferred. The same gap is why `'<` and
//! `'>` are permanently unset here, which `crate::editor_context` works around separately.
//!
//! So this is broken **today, for everyone, with no agent involved**: a `git checkout`, a formatter,
//! or an edit made in another window does not reach the buffer. It is listed as one of the three
//! MVP wires because the agent will eventually be a fourth way for a file to change underneath you,
//! not because the agent caused it.
//!
//! # Why a timer, and why no socket
//!
//! The precise fix is to tell nvim the moment a write lands. That needs a shell -> nvim channel,
//! and every channel this workspace has runs the other way (nvim writes, the host polls). Building
//! one would also cost a fourth Unix socket, against a budget with **three bytes left** across all
//! future protocols (see `crate::editor_context::feed`'s exact-length test). A timer inside nvim
//! costs neither, and the latency it trades away is bounded by [`RELOAD_INTERVAL_MS`].
//!
//! # Why `checktime` is safe to run unconditionally
//!
//! Measured on real nvim 2026-09-18 rather than reasoned about, because a modal prompt appearing
//! unbidden inside an embedded editor would be worse than the bug:
//!
//! - `autoread` defaults to **on** in nvim (unlike vim), and with it an **unmodified** buffer is
//!   reloaded silently.
//! - A **modified** buffer is not touched. nvim prints `W12: ... has changed and the buffer was
//!   changed in Vim as well` and carries on -- no prompt, no block, the user's edits intact.
//!
//! So this deliberately does **not** inspect `&modified` or `&autoread` first. nvim already makes
//! the right distinction, and a guard here would be a second, worse copy of it.

/// How often nvim checks. `checktime` stats each loaded buffer's file, so this is a handful of
/// syscalls a second, and one second is far below the time it takes to notice a stale buffer.
pub const RELOAD_INTERVAL_MS: u64 = 1000;

/// The one `--cmd`, inline rather than a `dofile`d file.
///
/// `crate::theme::feed` and `crate::editor_context::feed` each write a snippet to disk because each
/// is dozens of lines and needs a socket path handed to it in an environment variable. This has
/// neither: no state, no socket, no directory to create or sweep, and nothing to fail at startup.
/// Inlining keeps wire 3 free of the whole instance-directory machinery.
///
/// Three details, each load-bearing:
/// - `vim.schedule`, because `vim.cmd` in a libuv callback is a "fast event" and fails at runtime
///   with E5560, never at load.
/// - `pcall`, so a `checktime` that errors on some pathological buffer costs one tick, not the
///   timer.
/// - The timer handle is kept in a **Lua** global (`_G`), not in `vim.g`. A local would be
///   collected and the timer would stop at an unpredictable moment; `vim.g` is worse than either.
///   **Measured 2026-09-18:** `vim.g.x = vim.uv.new_timer()` raises `__newindex` -- `vim.g` holds
///   only values convertible to vimscript, and a uv handle is userdata. The timer was then never
///   created at all, the `--cmd`'s error went by unread, and the unit test asserting
///   `contains("vim.g.")` passed the whole time. The integration test is what caught it.
///
/// The same text is also what an injector loads into an nvim that is already running: the chunk
/// returns a function that stops the timer, and leaves `_G.eitri_reload_timer` alone if it no longer
/// holds this chunk's own timer.
pub const RELOAD_CMD: &str = concat!("lua ", include_str!("buffer_reload.lua"));

/// The timer as a chunk: one line, so the `--cmd` above can carry it verbatim.
pub(crate) const BUFFER_RELOAD_LUA: &str = include_str!("buffer_reload.lua");

/// The two arguments to append to the nvim child's command line.
pub fn nvim_args() -> Vec<String> {
    vec!["--cmd".to_string(), RELOAD_CMD.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--cmd` carries one command, and a trailing `--` comment would swallow nothing but also
    /// could never be told apart from code in `ps`: the chunk stays one comment-free line.
    #[test]
    fn the_chunk_is_one_line_with_no_comment() {
        assert!(!BUFFER_RELOAD_LUA.contains('\n'));
        assert!(!BUFFER_RELOAD_LUA.contains("--"));
    }

    #[test]
    fn the_command_is_one_cmd_argument_carrying_lua() {
        assert_eq!(nvim_args().len(), 2);
        assert_eq!(nvim_args()[0], "--cmd");
        assert!(nvim_args()[1].starts_with("lua "));
    }

    /// `vim.cmd` inside a libuv callback is a fast event and fails with E5560 at RUNTIME -- never at
    /// load, and never in a way a `--cmd` reports. The hop is the fix and it is easy to drop while
    /// "simplifying" this line.
    #[test]
    fn the_timer_callback_hops_to_the_main_loop_before_running_a_command() {
        assert!(
            RELOAD_CMD.contains("vim.schedule("),
            "checktime must not run in a fast event"
        );
    }

    /// A local timer handle is collected and the timer stops at an unpredictable moment. It must be
    /// a **Lua** global: the first draft used `vim.g`, which raises `__newindex` for a uv handle, so
    /// the timer was never created -- and this test, then written as `contains("vim.g.")`, passed
    /// against code that could not work even once. It is kept, narrowed, and openly not the proof:
    /// `core/tests/buffer_reload_with_real_nvim.rs` is.
    #[test]
    fn the_timer_handle_is_kept_alive_in_a_lua_global_not_a_vim_one() {
        assert!(RELOAD_CMD.contains("_G."), "a local timer handle is collected");
        assert!(
            !RELOAD_CMD.contains("vim.g."),
            "vim.g cannot hold a uv handle -- it raises __newindex and the timer is never created"
        );
    }

    /// Deliberately unconditional: nvim already reloads only unmodified buffers and only warns
    /// about modified ones, both measured. A guard here would be a second, worse copy of that.
    #[test]
    fn it_does_not_second_guess_nvim_about_modified_or_autoread() {
        assert!(!RELOAD_CMD.contains("modified"), "{RELOAD_CMD}");
        assert!(!RELOAD_CMD.contains("autoread"), "{RELOAD_CMD}");
    }

    /// The interval is stated once and used once. A literal in the Lua that drifts from the
    /// documented constant is exactly the class this project's other protocols keep pinning.
    #[test]
    fn the_documented_interval_is_the_one_the_timer_uses() {
        assert!(
            RELOAD_CMD.contains(&format!(":start({RELOAD_INTERVAL_MS}, {RELOAD_INTERVAL_MS},")),
            "{RELOAD_CMD}"
        );
    }
}
