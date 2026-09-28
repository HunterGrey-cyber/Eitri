-- neovibe theme feed. Loaded by `shell` with `--cmd`, before the user's own config, so it only
-- installs autocommands; it changes no setting. It snapshots the highlight groups `shell` derives
-- its colours from and writes one JSON line to NEOVIBE_THEME_SOCKET.
--
-- The group list must cover `GROUPS_READ` in core/src/theme/tokens.rs; a Rust test checks it.

local socket = vim.env.NEOVIBE_THEME_SOCKET
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

-- Send synchronously: `sockconnect` connects before returning, and `chansend` writes
-- this small payload into the socket before another autocommand runs. An asynchronous
-- libuv write waits for nvim's loop, which may be delayed by slow VimEnter handlers.
-- The shell drops connections that stay silent past its read timeout, so that delay
-- can lose the payload before the asynchronous write runs.
local function send()
  local ok, line = pcall(snapshot)
  if not ok then
    return
  end
  local connected, chan = pcall(vim.fn.sockconnect, "pipe", socket, { rpc = false })
  if not connected or chan == 0 then
    return
  end
  pcall(vim.fn.chansend, chan, line .. "\n")
  pcall(vim.fn.chanclose, chan)
end

local augroup = vim.api.nvim_create_augroup("NeovibeThemeFeed", { clear = true })
vim.api.nvim_create_autocmd({ "VimEnter", "ColorScheme" }, { group = augroup, callback = send })
vim.api.nvim_create_autocmd("OptionSet", { group = augroup, pattern = { "background", "guifont" }, callback = send })

-- nvim has no "a highlight group changed" event at all, so a bare
-- `nvim_set_hl` call -- or an autocmd registered AFTER this file's own, which therefore fires
-- after the snapshot above already went out -- never triggers a re-send. Reachable directly from
-- LazyVim setups, where plugins commonly tweak highlight groups from
-- post-colorscheme/VimEnter autocmds. A periodic re-snapshot is the cheapest self-healing fix that
-- needs no new event: it bounds how stale the panel can stay after such a change to
-- RESNAPSHOT_INTERVAL_MS rather than for the rest of the session. `send` already no-ops safely
-- (via its own `pcall`s) whenever nothing has changed, so resending an unchanged payload costs one
-- cheap connect+write+close and nothing else.
local RESNAPSHOT_INTERVAL_MS = 3000
local resnapshot_timer = vim.uv.new_timer()
-- `nvim_get_hl` is not allowed in a fast event (see `snapshot` above), and a libuv timer callback
-- runs in one -- `vim.schedule_wrap` defers it onto the main loop, same as every other libuv
-- callback that touches the API in this workspace.
resnapshot_timer:start(RESNAPSHOT_INTERVAL_MS, RESNAPSHOT_INTERVAL_MS, vim.schedule_wrap(send))
