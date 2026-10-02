-- Eitri theme feed. Loaded by `shell` with `--cmd`, before the user's own config, so it only
-- installs autocommands; it changes no setting. It snapshots the highlight groups `shell` derives
-- its colours from and writes one JSON line to EITRI_THEME_SOCKET.
--
-- The group list must cover `GROUPS_READ` in core/src/theme/tokens.rs; a Rust test checks it.
--
-- Called two ways. Under `--cmd` (`dofile`, no arguments) the socket comes from the environment the
-- host set at spawn. Injected into an already running nvim, the chunk gets a table instead and must
-- not look at the environment at all: an nvim started inside another Eitri window inherits that
-- window's variables, and reading them would feed the wrong panel. An injected chunk returns a
-- function that undoes it.

local opts = ...
local socket
if type(opts) == "table" then
  socket = opts.socket
else
  socket = vim.env.EITRI_THEME_SOCKET
end
if not socket or socket == "" then
  return
end

local GROUPS = {
  "Normal", "NormalFloat", "Pmenu", "StatusLine", "WinSeparator", "VertSplit", "Comment", "Visual",
  "IncSearch", "Search", "DiagnosticWarn", "WarningMsg", "DiagnosticError", "ErrorMsg", "DiagnosticOk",
  "DiagnosticInfo", "Function", "String", "@keyword", "Statement", "@function", "@string", "@type", "Type",
  "@comment", "@number", "Number", "@constant", "Constant", "@variable", "Identifier", "@operator",
  "Operator", "@punctuation", "Delimiter", "@property", "@tag", "Tag",
}

-- Runs on the main loop (autocmd callback): `nvim_get_hl` is not allowed in a fast event.
local function snapshot()
  local groups = vim.empty_dict()
  for _, name in ipairs(GROUPS) do
    local ok, attrs = pcall(vim.api.nvim_get_hl, 0, { name = name, link = false })
    if ok and next(attrs) ~= nil then
      groups[name] = { fg = attrs.fg, bg = attrs.bg, reverse = attrs.reverse == true }
    end
  end
  return vim.json.encode({
    v = 1,
    groups = groups,
    options = {
      background = vim.o.background,
      guifont = vim.o.guifont,
      colors_name = vim.g.colors_name or "",
    },
  })
end

-- Synchronous on purpose. `sockconnect` connects before it returns and `chansend` writes a line
-- this small straight into the socket, so it is in the kernel before any other autocommand runs.
-- An asynchronous libuv write waits for nvim's loop to turn -- i.e. after every other VimEnter
-- handler in the user's config, which can take hundreds of milliseconds -- and the shell drops a
-- connection that stays silent past its read timeout. (Found by the Task 7 review: a 400ms VimEnter
-- handler lost both payloads with the async write, none with this one.)
-- The last line this file got into the socket. Only the timer below reads it.
local last_sent = nil
-- Set by the teardown. A resnapshot the timer already queued with `schedule_wrap` still runs once
-- after the timer is closed, and must write nothing then.
local torn = false

-- `only_if_changed` is the timer's: every payload the shell receives restyles the whole window and
-- re-themes the panel on its GTK thread, and the shell does not deduplicate, so re-sending an
-- unchanged snapshot every few seconds costs every idle window that work forever (sw-theme-5,
-- whole-branch review). An autocommand's send stays unconditional. `last_sent` is set only after
-- `chansend` wrote the line, so a send that failed is retried by the next tick.
local function send(only_if_changed)
  if torn then
    return
  end
  local ok, line = pcall(snapshot)
  if not ok then
    return
  end
  if only_if_changed and line == last_sent then
    return
  end
  local connected, chan = pcall(vim.fn.sockconnect, "pipe", socket, { rpc = false })
  if not connected or chan == 0 then
    return
  end
  local sent, written = pcall(vim.fn.chansend, chan, line .. "\n")
  pcall(vim.fn.chanclose, chan)
  if sent and written ~= 0 then
    last_sent = line
  end
end

-- An autocommand callback that returns true deletes its autocommand, so these return nothing.
local function send_now()
  send(false)
end

local augroup = vim.api.nvim_create_augroup("EitriThemeFeed", { clear = true })
vim.api.nvim_create_autocmd({ "VimEnter", "ColorScheme" }, { group = augroup, callback = send_now })
vim.api.nvim_create_autocmd("OptionSet", { group = augroup, pattern = { "background", "guifont" }, callback = send_now })

-- sw-theme-5 (2026-09-27): nvim has no "a highlight group changed" event at all, so a bare
-- `nvim_set_hl` call -- or an autocmd registered AFTER this file's own, which therefore fires
-- after the snapshot above already went out -- never triggers a re-send. Reachable directly from
-- the owner's own LazyVim setup, where plugins commonly tweak highlight groups from
-- post-colorscheme/VimEnter autocmds. A periodic re-snapshot is the cheapest self-healing fix that
-- needs no new event: it bounds how stale the panel can stay after such a change to
-- RESNAPSHOT_INTERVAL_MS rather than for the rest of the session. It sends only a snapshot that
-- differs from the last one sent, so an unchanged theme costs nvim one `nvim_get_hl` pass per tick
-- and the shell nothing.
local RESNAPSHOT_INTERVAL_MS = 3000
local resnapshot_timer = vim.uv.new_timer()
-- `nvim_get_hl` is not allowed in a fast event (see `snapshot` above), and a libuv timer callback
-- runs in one -- `vim.schedule_wrap` defers it onto the main loop, same as every other libuv
-- callback that touches the API in this workspace.
resnapshot_timer:start(RESNAPSHOT_INTERVAL_MS, RESNAPSHOT_INTERVAL_MS, vim.schedule_wrap(function()
  send(true)
end))

-- Injected into an nvim past VimEnter: that event has fired and will not again, so the first
-- snapshot goes out now rather than at the first resnapshot tick.
if vim.v.vim_did_enter == 1 then
  send_now()
end

return function()
  torn = true
  pcall(vim.api.nvim_del_augroup_by_id, augroup)
  if not resnapshot_timer:is_closing() then
    resnapshot_timer:stop()
    resnapshot_timer:close()
  end
end
