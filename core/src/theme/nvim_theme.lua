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
  "Normal", "NormalFloat", "Pmenu", "StatusLine", "WinSeparator", "VertSplit", "Comment", "Visual", "CursorLine",
  "IncSearch", "Search", "DiagnosticWarn", "WarningMsg", "DiagnosticError", "ErrorMsg", "DiagnosticOk",
  "DiagnosticInfo", "Function", "String", "@keyword", "Statement", "@function", "@string", "@type", "Type",
  "@comment", "@number", "Number", "@constant", "Constant", "@variable", "Identifier", "@operator",
  "Operator", "@punctuation", "Delimiter", "@property", "@tag", "Tag",
  -- How a Markdown link's text is drawn in the buffer: a plugin page beside the editor colours its links the same.
  "@markup.link.label", "@markup.link", "Underlined",
  -- The statusline's mode sections, which the panel's own mode block copies. lualine creates them when it
  -- loads (LazyVim: after VimEnter), so the first payload may lack them and the resnapshot timer below
  -- carries them over a few seconds later.
  "lualine_a_normal", "lualine_a_insert", "lualine_a_visual",
}

-- What the user's config left in `g:neovide_scale_factor`: where `Ctrl+0` returns to. Read once at VimEnter, so a
-- `:let` later in the session never counts; nvim has no other moment that says the config has run. A value JSON
-- cannot carry stays out, or the whole payload would be lost with it.
local start_scale = nil
local function read_start_scale()
  local scale = vim.g.neovide_scale_factor
  if scale == nil then
    start_scale = 1.0
  elseif type(scale) == "number" and scale > 0 and scale < math.huge then
    start_scale = scale
  end
end

-- What the editor draws for a Normal-mode cursor, read from `'guicursor'` here because the editor's own cursor
-- table belongs to Neovide and this feed never reads that. The entries that name mode `n` or `a` apply in order
-- and a later one overrides only the fields it sets. An entry's arguments are `block`, `ver<n>`, `hor<n>`,
-- `blinkwait<n>`, `blinkon<n>`, `blinkoff<n>`, or a highlight group (the part before `/`, which names the
-- `lCursor` group). nvim reads the modes and those keywords without regard to case (`N:BLOCK-Cursor` is the same
-- entry as `n:block-Cursor`), so they are compared lowercased; the group keeps its own spelling. Blink times unset
-- stay 0, which the host reads as steady. They are capped well below the largest u32 so a silly number cannot make
-- the whole payload unparsable. A group name nvim would refuse (anything outside ASCII letters, digits, `_`, `.` and
-- `@`) leaves the option set but gives the cursor no group, so nvim draws Normal reversed; the name is not sent,
-- because `:set` accepts a name that is not valid UTF-8 and the host rejects a whole frame that is.
local MAX_BLINK = 0x7fffffff
local function normal_cursor()
  local group, blink = nil, { wait = 0, on = 0, off = 0 }
  for entry in vim.gsplit(vim.o.guicursor, ",", { plain = true }) do
    local modes, args = entry:match("^([^:]*):(.*)$")
    local applies = false
    for mode in vim.gsplit((modes or ""):lower(), "-", { plain = true }) do
      applies = applies or mode == "n" or mode == "a"
    end
    if applies then
      for arg in vim.gsplit(args, "-", { plain = true }) do
        local lowered = arg:lower()
        local kind, digits = lowered:match("^blink(%a+)(%d+)$")
        if kind and blink[kind] then
          blink[kind] = math.min(tonumber(digits), MAX_BLINK)
        elseif lowered ~= "block" and not lowered:match("^ver%d+$") and not lowered:match("^hor%d+$") then
          local name = arg:match("^([^/]*)")
          if name ~= "" then
            group = name:find("^[A-Za-z0-9_.@]+$") and name or nil
          end
        end
      end
    end
  end
  local cursor = { group = group, blinkwait = blink.wait, blinkon = blink.on, blinkoff = blink.off }
  if group then
    local ok, attrs = pcall(vim.api.nvim_get_hl, 0, { name = group, link = false })
    if ok then
      cursor.fg, cursor.bg = attrs.fg, attrs.bg
    end
  end
  return cursor
end

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
    cursor = normal_cursor(),
    options = {
      background = vim.o.background,
      guifont = vim.o.guifont,
      colors_name = vim.g.colors_name or "",
      -- How many rows the command line takes under the editor's statusline: the panel's own status
      -- band sits that many rows above the window's bottom so the two lines are one row.
      cmdheight = vim.o.cmdheight,
      start_scale = start_scale,
    },
  })
end

-- Synchronous on purpose. `sockconnect` connects before it returns and `chansend` writes a line
-- this small straight into the socket, so it is in the kernel before any other autocommand runs.
-- An asynchronous libuv write waits for nvim's loop to turn -- i.e. after every other VimEnter
-- handler in the user's config, which can take hundreds of milliseconds -- and the shell drops a
-- connection that stays silent past its read timeout. (Measured: a 400ms VimEnter
-- handler lost both payloads with the async write, none with this one.)
-- The last line this file got into the socket. Only the timer below reads it.
local last_sent = nil
-- Set by the teardown. A resnapshot the timer already queued with `schedule_wrap` still runs once
-- after the timer is closed, and must write nothing then.
local torn = false

-- `only_if_changed` is the timer's: every payload the shell receives restyles the whole window and
-- re-themes the panel on its GTK thread, and the shell does not deduplicate, so re-sending an
-- unchanged snapshot every few seconds costs every idle window that work forever. An
-- autocommand's send stays unconditional. `last_sent` is set only after
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

local augroup = vim.api.nvim_create_augroup((type(opts) == "table" and opts.group) or "EitriThemeFeed", { clear = true })
vim.api.nvim_create_autocmd("VimEnter", { group = augroup, once = true, callback = function()
  read_start_scale()
end })
vim.api.nvim_create_autocmd({ "VimEnter", "ColorScheme" }, { group = augroup, callback = send_now })
vim.api.nvim_create_autocmd("OptionSet", { group = augroup, pattern = { "background", "guifont", "cmdheight", "guicursor" }, callback = send_now })

-- nvim has no "a highlight group changed" event at all, so a bare
-- `nvim_set_hl` call -- or an autocmd registered AFTER this file's own, which therefore fires
-- after the snapshot above already went out -- never triggers a re-send. Reachable directly from
-- an ordinary LazyVim setup, where plugins commonly tweak highlight groups from
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
  if torn then
    return
  end
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
