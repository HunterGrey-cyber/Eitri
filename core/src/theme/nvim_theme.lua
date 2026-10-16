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
  -- The statusline's mode sections, which the panel's own mode block copies. lualine creates them when it
  -- loads (LazyVim: after VimEnter), so the first payload may lack them and the resnapshot timer below
  -- carries them over a few seconds later.
  "lualine_a_normal", "lualine_a_insert", "lualine_a_visual",
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
      -- How many rows the command line takes under the editor's statusline: the panel's own status
      -- band sits that many rows above the window's bottom so the two lines are one row.
      cmdheight = vim.o.cmdheight,
    },
  })
end

-- Send synchronously: `sockconnect` connects before returning and `chansend` writes this
-- small line into the socket before another autocommand runs. An asynchronous libuv write
-- waits for nvim's loop, so a slow VimEnter handler can leave the connection silent past
-- the shell's read timeout. A 400ms handler lost both asynchronous payloads but neither
-- synchronous payload. `last_sent` holds the last line written; only the timer reads it.
local last_sent = nil
-- Set by the teardown. A resnapshot the timer already queued with `schedule_wrap` still runs once
-- after the timer is closed, and must write nothing then.
local torn = false

-- The timer uses `only_if_changed` because every received payload restyles the window and
-- rethemes the panel on the GTK thread. The shell does not deduplicate, so repeated idle
-- snapshots would repeat that work. Autocommand sends remain unconditional. Set `last_sent`
-- only after `chansend` writes the line, so a failed send is retried on the next tick.
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
vim.api.nvim_create_autocmd("OptionSet", { group = augroup, pattern = { "background", "guifont", "cmdheight" }, callback = send_now })

-- nvim has no "a highlight group changed" event at all, so a bare
-- `nvim_set_hl` call -- or an autocmd registered AFTER this file's own, which therefore fires
-- after the snapshot above already went out -- never triggers a re-send. Reachable directly from
-- LazyVim setups, where plugins commonly tweak highlight groups from
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
