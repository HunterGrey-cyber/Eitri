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
//! The precise fix is to tell nvim the moment a write lands. The host can call into nvim over the
//! RPC link it already holds (`crate::editor_rpc::EditorRpc::exec_lua`), but it would first have to
//! see the write, and the writes this wire is for (a `git checkout`, a formatter, another window)
//! never pass through the host, which watches no files. A timer inside nvim catches every one of
//! them, needs no new channel or socket, and the latency it trades away is bounded by
//! [`RELOAD_INTERVAL_MS`].
//!
//! # Which buffers the timer checks
//!
//! What follows was measured on real nvim rather than reasoned about, because an unbidden modal
//! prompt inside an embedded editor would be worse than a stale buffer:
//!
//! - `autoread` defaults to **on** in nvim (unlike vim), and with it an **unmodified** buffer is
//!   reloaded silently.
//! - For a **modified** buffer `checktime` raises `W12` as a dialog (`[O]K, (L)oad File, Load File
//!   (a)nd Options`) whenever a UI is attached: in insert mode it takes the next keys the person
//!   types. Headless, with no UI, nvim answers the dialog itself and only the message shows.
//! - For an unmodified buffer whose `autoread` is **off**, `checktime` raises `W11` as the same
//!   kind of dialog, every tick.
//!
//! So the timer checks only buffers shown in a window of some tab page (a bare `checktime`'s set;
//! `checktime {buf}` would also reload a buffer in no window), unmodified, with `autoread` on. A
//! buffer with unsaved edits, or whose person turned `autoread` off, hears nothing until it is
//! written, where nvim's own "changed since reading it" question still asks. Each window's step runs
//! under `pcall` with its window and buffer checked first: a reload's autocommands may close a window
//! or wipe a buffer the loop has not reached, and an error in a scheduled callback is a message that
//! stops nvim at a hit-enter prompt.
//!
//! Two more cases answer with a message instead of a reload, and the timer keeps them out too:
//!
//! - A shown file that **no longer exists** is `E211`; in Normal mode nvim then pauses for a second,
//!   once per deletion (a name too long for the command line asks for a key). The timer stats the file
//!   first and does not check a buffer whose file is gone; when the file comes back the next tick checks
//!   it again and nvim reloads it.
//! - A file **created after its buffer was opened on a missing path** is `W13`, a dialog that takes the
//!   next key whatever `autoread` says. Lua cannot see nvim's mark for such a buffer, but it holds one
//!   empty line, so a buffer that holds one empty line is read again with `:edit` the first time its file is
//!   there after it was missing, and at first sight; every other buffer goes to `checktime`. The re-read runs
//!   narrowed (no swap-file dialog, no enter or leave events, the options it sets put back), only for a
//!   regular file (opening a FIFO blocks), and the buffer counts as seen only once it has run. A name that
//!   is not a path (`term://`, `oil://`) and a `buftype` other than `""` are not looked at. One `fs_stat` per
//!   shown buffer per tick, at most.

/// How often nvim checks. Each tick stats the file of each buffer shown in a window, so this is a
/// handful of syscalls a second, and one second is far below the time it takes to notice a stale buffer.
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
/// - `pcall` around each window's step, so a `checktime` that errors on some pathological buffer, or
///   a window a reload's autocommand closed, costs that window one tick, not the timer.
/// - The timer handle is kept in a **Lua** global (`_G`), not in `vim.g`. A local would be
///   collected and the timer would stop at an unpredictable moment; `vim.g` is worse than either.
///   `vim.g.x = vim.uv.new_timer()` raises `__newindex`: `vim.g` holds only values convertible
///   to vimscript, and a uv handle is userdata. A text assertion can miss a timer that never
///   starts; the real-nvim integration test checks that it runs.
///
/// Loaded as a chunk into an nvim that is already running, it returns a function that stops the
/// timer, and leaves `_G.eitri_reload_timer` alone if it no longer
/// holds this chunk's own timer.
pub const RELOAD_CMD: &str = concat!("lua ", include_str!("buffer_reload.lua"));

/// The timer as a chunk: one line, so the `--cmd` above can carry it verbatim (its tests read it).
#[cfg(test)]
pub(crate) const BUFFER_RELOAD_LUA: &str = include_str!("buffer_reload.lua");

/// The two arguments to append to the nvim child's command line.
pub fn nvim_args() -> Vec<String> {
    vec!["--cmd".to_string(), RELOAD_CMD.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_cmd_and_its_snippet_are_byte_identical_to_today() {
        const SNIPPET: &str = r#"_G.eitri_reload_timer = vim.uv.new_timer(); local t = _G.eitri_reload_timer; local files = {} local function reread(b) local function put(name, value) vim.api.nvim_cmd({ cmd = "set", args = { name .. "=" .. vim.fn.escape(value, " \\|\"") }, mods = { noautocmd = true } }, {}) end local function flag(name, on) vim.api.nvim_cmd({ cmd = "set", args = { (on and "" or "no") .. name }, mods = { noautocmd = true } }, {}) end local short, ignore, chdir = vim.go.shortmess, vim.go.eventignore, vim.go.autochdir local readonly, listed = vim.bo[b].readonly, vim.bo[b].buflisted local ok = pcall(function() put("shortmess", short .. "A") vim.go.eventignore = (ignore == "" and "" or ignore .. ",") .. "BufEnter,BufLeave,WinEnter,WinLeave" flag("autochdir", false) vim.api.nvim_buf_call(b, function() vim.cmd("silent keepalt keepjumps edit") end) end) pcall(put, "shortmess", short) pcall(function() vim.go.eventignore = ignore end) pcall(flag, "autochdir", chdir) pcall(function() vim.api.nvim_buf_call(b, function() if readonly and not vim.bo[b].readonly then vim.api.nvim_cmd({ cmd = "setlocal", args = { "readonly" }, mods = { noautocmd = true } }, {}) end if not listed and vim.bo[b].buflisted then vim.api.nvim_cmd({ cmd = "setlocal", args = { "nobuflisted" }, mods = { noautocmd = true } }, {}) end end) end) return ok end local function checkable(b) if vim.bo[b].buftype ~= "" then return nil end local name = vim.api.nvim_buf_get_name(b) if name == "" or name:find("^%a[%w+.-]*://") then return nil end local s = files[b] if not s or s.name ~= name then s = { name = name } files[b] = s end local blank = vim.api.nvim_buf_line_count(b) == 1 and vim.api.nvim_buf_get_lines(b, 0, 1, false)[1] == "" local stat = vim.uv.fs_stat(name) if not stat then s.seen = false return nil end if blank and not s.seen and stat.type == "file" then return "reread" end s.seen = true return "check" end; _G.eitri_reload_timer:start(1000, 1000, function() vim.schedule(function() for b in pairs(files) do if not vim.api.nvim_buf_is_valid(b) then files[b] = nil end end local seen = {} for _, w in ipairs(vim.api.nvim_list_wins()) do pcall(function() if not vim.api.nvim_win_is_valid(w) then return end local b = vim.api.nvim_win_get_buf(w) if seen[b] or not vim.api.nvim_buf_is_valid(b) then return end seen[b] = true if vim.bo[b].modified then return end local ar = vim.api.nvim_get_option_value('autoread', { buf = b }) if ar == nil or ar == -1 then ar = vim.go.autoread end if ar ~= true and ar ~= 1 then return end local how = checkable(b) if how == 'reread' then if reread(b) then files[b].seen = true end elseif how then vim.cmd('checktime ' .. b) end end) end end) end); return function() if not t:is_closing() then t:stop(); t:close() end; if _G.eitri_reload_timer == t then _G.eitri_reload_timer = nil end end"#;
        assert_eq!(include_str!("buffer_reload.lua").as_bytes(), SNIPPET.as_bytes());
        assert_eq!(RELOAD_CMD.as_bytes(), [b"lua ".as_slice(), SNIPPET.as_bytes()].concat());
    }

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
    /// load, and never in a way a `--cmd` reports. The hop is what avoids that, and it is easy to drop while
    /// "simplifying" this line.
    #[test]
    fn the_timer_callback_hops_to_the_main_loop_before_running_a_command() {
        assert!(
            RELOAD_CMD.contains("vim.schedule("),
            "checktime must not run in a fast event"
        );
    }

    /// A local timer handle is collected and the timer stops at an unpredictable moment. It must be
    /// a **Lua** global: `vim.g` raises `__newindex` for a uv handle, so no timer starts there.
    /// This text check only guards the spelling; `core/tests/buffer_reload_with_real_nvim.rs`
    /// checks the running timer.
    #[test]
    fn the_timer_handle_is_kept_alive_in_a_lua_global_not_a_vim_one() {
        assert!(RELOAD_CMD.contains("_G."), "a local timer handle is collected");
        assert!(
            !RELOAD_CMD.contains("vim.g."),
            "vim.g cannot hold a uv handle -- it raises __newindex and the timer is never created"
        );
    }

    /// Under a UI a modified buffer would get `W12` and a `noautoread` one `W11`, each as a dialog; the
    /// timer leaves both alone, and checks each window and buffer before it touches them.
    /// `core/tests/buffer_reload_with_real_nvim.rs` checks the dialogs and the closed window.
    #[test]
    fn it_skips_modified_and_noautoread_buffers_and_guards_each_window() {
        assert!(RELOAD_CMD.contains("vim.bo[b].modified"), "{RELOAD_CMD}");
        assert!(RELOAD_CMD.contains("nvim_get_option_value('autoread'"), "{RELOAD_CMD}");
        assert!(RELOAD_CMD.contains("pcall(function()"), "{RELOAD_CMD}");
        assert!(RELOAD_CMD.contains("nvim_win_is_valid(w)"), "{RELOAD_CMD}");
        assert!(RELOAD_CMD.contains("nvim_buf_is_valid(b)"), "{RELOAD_CMD}");
        assert!(RELOAD_CMD.contains("vim.uv.fs_stat(name)"), "{RELOAD_CMD}");
        assert!(RELOAD_CMD.contains("vim.cmd('checktime ' .. b)"), "{RELOAD_CMD}");
        assert!(RELOAD_CMD.contains("keepalt keepjumps edit"), "{RELOAD_CMD}");
        assert!(RELOAD_CMD.contains("stat.type == \"file\""), "{RELOAD_CMD}");
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
