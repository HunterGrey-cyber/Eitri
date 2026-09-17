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

-- Synchronous on purpose. `sockconnect` connects before it returns and `chansend` writes a line
-- this small straight into the socket, so it is in the kernel before any other autocommand runs.
-- An asynchronous libuv write waits for nvim's loop to turn -- i.e. after every other VimEnter
-- handler in the user's config, which can take hundreds of milliseconds -- and the shell drops a
-- connection that stays silent past its read timeout. (Found by the Task 7 review: a 400ms VimEnter
-- handler lost both payloads with the async write, none with this one.)
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
