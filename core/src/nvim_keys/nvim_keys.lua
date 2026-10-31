-- Eitri nvim-keys feed. Loaded with --cmd before the user's config; installs
-- autocommands and one dict watcher, changes no setting and no mapping. Writes one JSON line per
-- report to EITRI_KEYS_SOCKET, only when the report differs from the last one sent.
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
  socket = vim.env.EITRI_KEYS_SOCKET
end
if not socket or socket == "" then
  return
end

local MAX_MAPS, MAX_TEXT = 2000, 200
local last, scheduled = nil, false
-- A rescan asks for the next report to be sent even when nothing changed, and for the chords in
-- `extra_chords` to be probed in that report only. Both are read, and cleared, when the debounce fires.
local forced, extra_chords = false, nil
-- Set by the teardown: a debounce already waiting in `defer_fn` must send nothing afterwards.
local torn = false

-- Text from a mapping can hold bytes that are not UTF-8; one such byte would make the whole line
-- unreadable to the host, so each is replaced with U+FFFD before the text leaves nvim.
local function valid_utf8(s)
  if not s:find("[\128-\255]") then
    return s
  end
  local out, i, n = {}, 1, #s
  while i <= n do
    local c = s:byte(i)
    local len = 0
    if c < 0x80 then
      len = 1
    elseif c >= 0xC2 and c <= 0xDF then
      len = 2
    elseif c >= 0xE0 and c <= 0xEF then
      len = 3
    elseif c >= 0xF0 and c <= 0xF4 then
      len = 4
    end
    local ok = len > 0 and i + len - 1 <= n
    if ok and len > 1 then
      local second = s:byte(i + 1)
      ok = second >= 0x80 and second <= 0xBF
        and not (c == 0xE0 and second < 0xA0)
        and not (c == 0xED and second > 0x9F)
        and not (c == 0xF0 and second < 0x90)
        and not (c == 0xF4 and second > 0x8F)
      for j = i + 2, i + len - 1 do
        local b = ok and s:byte(j) or 0
        if b < 0x80 or b > 0xBF then
          ok = false
        end
      end
    end
    if ok then
      out[#out + 1] = s:sub(i, i + len - 1)
      i = i + len
    else
      out[#out + 1] = "\239\191\189"
      i = i + 1
    end
  end
  return table.concat(out)
end

local function cut(s)
  if type(s) ~= "string" then
    return nil
  end
  return valid_utf8(vim.fn.strcharpart(s, 0, MAX_TEXT))
end

-- The probe: the few chords the presets can pick as a prefix, read in every mode they could collide
-- in, and which of the known plugins are present. Mirrored by PROBED_CHORDS, PROBED_MODES and
-- KNOWN_PLUGINS in `mod.rs` (a test compares the lists).
local PROBED_CHORDS = { "<C-b>", "<C-a>", "<C-Space>", "<C-h>", "<C-j>", "<C-k>", "<C-l>" }
local PROBED_MODES = { "n", "x", "i", "t" }
local KNOWN_PLUGINS = {
  "vim-tmux-navigator",
  "smart-splits.nvim",
  "Navigator.nvim",
  "which-key.nvim",
  "blink.cmp",
  "nvim-cmp",
  "lazy.nvim",
  "LazyVim",
  "NvChad",
  "AstroNvim",
  "LunarVim",
}

-- nvim spells a control chord's letter in upper case and `<C-Space>` as `<Nul>` or `<C-@>` depending
-- on how it was written, so chords are compared through one lower-case spelling.
local function chord_key(lhs)
  local s = lhs:lower()
  if s == "<nul>" or s == "<c-@>" then
    return "<c-space>"
  end
  return s
end

local PROBED = {}
for _, chord in ipairs(PROBED_CHORDS) do
  PROBED[chord_key(chord)] = true
end

local MAX_EXTRA_BYTES = 16

-- The chords a rescan adds to the probe, as the set `probe_keys` compares against. Only `<...>` notation
-- of at most 16 bytes is taken; anything else a caller passes is skipped. `nil` when nothing is left.
local function extra_set(list)
  if type(list) ~= "table" then
    return nil
  end
  local set, any = {}, false
  for _, lhs in ipairs(list) do
    if type(lhs) == "string" and #lhs <= MAX_EXTRA_BYTES and lhs:match("^<.+>$") then
      set[chord_key(lhs)] = true
      any = true
    end
  end
  return any and set or nil
end

-- Which plugin or config a mapping came from: the plugin's directory name, `config` for a file under
-- nvim's config directory, nothing otherwise. Never a path. A mapping a Lua file sets has no script
-- id, so it has no origin.
local function origin_of(sid)
  if type(sid) ~= "number" or sid <= 0 then
    return nil
  end
  local ok, info = pcall(vim.fn.getscriptinfo, { sid = sid })
  if not ok or type(info) ~= "table" or type(info[1]) ~= "table" or type(info[1].name) ~= "string" then
    return nil
  end
  local path = info[1].name:gsub("\\", "/")
  local rest = path
  local okc, config = pcall(vim.fn.stdpath, "config")
  local under_config = false
  if okc and type(config) == "string" and config ~= "" then
    config = config:gsub("\\", "/")
    if path:sub(1, #config + 1) == config .. "/" then
      under_config = true
      rest = "/" .. path:sub(#config + 2)
    end
  end
  local name = rest:match("/pack/[^/]+/start/([^/]+)/")
    or rest:match("/pack/[^/]+/opt/([^/]+)/")
    or (not under_config and rest:match("/lazy/([^/]+)/"))
  if name then
    return cut(name)
  end
  return under_config and "config" or nil
end

local function probe_keys(extra)
  local keys = {}
  for _, mode in ipairs(PROBED_MODES) do
    for _, scope in ipairs({ "global", "buffer" }) do
      local ok, list = pcall(function()
        if scope == "global" then
          return vim.api.nvim_get_keymap(mode)
        end
        return vim.api.nvim_buf_get_keymap(0, mode)
      end)
      if ok then
        for _, m in ipairs(list) do
          local lhs = vim.fn.keytrans(m.lhsraw or m.lhs)
          local key = chord_key(lhs)
          if PROBED[key] or (extra and extra[key]) then
            keys[#keys + 1] = {
              mode = mode,
              lhs = lhs,
              scope = scope,
              rhs = cut(m.rhs),
              desc = cut(m.desc),
              callback = m.callback ~= nil,
              origin = origin_of(m.sid),
            }
          end
        end
      end
    end
  end
  return keys
end

-- Present means a module the plugin owns is already in `package.loaded` (or its own global is set);
-- nothing here loads a module. lazy.nvim is read the same way: `plugins()` is called only on a lazy
-- that is already loaded, to find the plugins it will load later.
local function probe_plugins()
  local loaded = package.loaded
  local detect = {
    ["vim-tmux-navigator"] = function()
      return vim.g.loaded_tmux_navigator ~= nil
    end,
    ["smart-splits.nvim"] = function()
      return loaded["smart-splits"]
    end,
    ["Navigator.nvim"] = function()
      return loaded["Navigator"]
    end,
    ["which-key.nvim"] = function()
      return loaded["which-key"]
    end,
    ["blink.cmp"] = function()
      return loaded["blink.cmp"]
    end,
    ["nvim-cmp"] = function()
      return loaded["cmp"]
    end,
    ["lazy.nvim"] = function()
      return loaded["lazy"]
    end,
    ["LazyVim"] = function()
      return loaded["lazyvim"] or loaded["LazyVim"]
    end,
    ["NvChad"] = function()
      return loaded["nvchad"]
    end,
    ["AstroNvim"] = function()
      return loaded["astronvim"]
    end,
    ["LunarVim"] = function()
      return rawget(_G, "lvim")
    end,
  }
  local declared = {}
  local lazy = loaded["lazy"]
  if type(lazy) == "table" and type(lazy.plugins) == "function" then
    local ok, list = pcall(lazy.plugins)
    if ok and type(list) == "table" then
      for _, plugin in ipairs(list) do
        if type(plugin) == "table" and type(plugin.name) == "string" then
          declared[plugin.name] = true
        end
      end
    end
  end
  local plugins = {}
  for _, name in ipairs(KNOWN_PLUGINS) do
    local ok, present = pcall(detect[name])
    if ok and present then
      plugins[name] = "loaded"
    elseif declared[name] then
      plugins[name] = "declared"
    end
  end
  if next(plugins) == nil then
    return vim.empty_dict()
  end
  return plugins
end

local function probe(extra)
  return { keys = probe_keys(extra), plugins = probe_plugins() }
end

local function report(extra)
  local maps = {}
  for _, m in ipairs(vim.api.nvim_get_keymap("n")) do
    local lhs = vim.fn.keytrans(m.lhsraw or m.lhs)
    if not lhs:find("<Plug>", 1, true) and not lhs:find("<SNR>", 1, true) then
      maps[#maps + 1] = { lhs = lhs, rhs = cut(m.rhs), desc = cut(m.desc), callback = m.callback ~= nil }
      if #maps >= MAX_MAPS then
        break
      end
    end
  end
  local leader = vim.g.mapleader
  local okp, probed = pcall(probe, extra)
  return vim.json.encode({
    v = 1,
    mapleader = (type(leader) == "string" and leader ~= "") and vim.fn.keytrans(leader) or vim.NIL,
    timeoutlen = vim.o.timeoutlen,
    timeout = vim.o.timeout,
    maps = maps,
    probe = okp and probed or nil,
  })
end

-- Connect/write/close are asynchronous, exactly as in the editor-context snippet (the tested one).
-- The host may accept before this connect callback writes any bytes; it retains incomplete lines
-- across polls instead of waiting on the GTK thread.
local function send(payload)
  local pipe = vim.uv.new_pipe(false)
  pipe:connect(socket, function(err)
    if err then
      pipe:close()
      return
    end
    pipe:write(payload .. "\n", function()
      pipe:close()
    end)
  end)
end

-- nvim has no "mappings changed" event (autocmd.txt), so re-read after the events that plausibly
-- change them, debounced, and send only a difference.
local function schedule()
  if scheduled then
    return
  end
  scheduled = true
  vim.defer_fn(function()
    if torn then
      return
    end
    scheduled = false
    local force, extra = forced, extra_chords
    forced, extra_chords = false, nil
    -- Whether anything changed is judged on the report without the extra chords, so a probe widened
    -- for one report does not make the next, ordinary one look changed.
    local ok, base = pcall(report)
    if ok and (force or base ~= last) then
      local payload = base
      if extra then
        local okx, widened = pcall(report, extra)
        if okx then
          payload = widened
        end
      end
      last = base
      send(payload)
    end
  end, 100)
end
_G.__eitri_keys_schedule = schedule

-- Sends the next report even when it is unchanged, probing `extra_lhs` (a list of `<...>` chords) as well
-- for that report. A debounce already waiting honours it; otherwise one is started.
local function rescan(extra_lhs)
  if torn then
    return false
  end
  forced = true
  local set = extra_set(extra_lhs)
  if set then
    extra_chords = extra_chords or {}
    for key in pairs(set) do
      extra_chords[key] = true
    end
  end
  schedule()
  return true
end
_G.__eitri_keys_rescan = rescan

local group = vim.api.nvim_create_augroup("eitri_keys", { clear = true })
vim.api.nvim_create_autocmd({ "VimEnter", "SourcePost", "FocusLost" }, { group = group, callback = schedule })
vim.api.nvim_create_autocmd("User", { group = group, pattern = { "VeryLazy", "LazyLoad" }, callback = schedule })
vim.api.nvim_create_autocmd("OptionSet", { group = group, pattern = { "timeoutlen", "timeout" }, callback = schedule })
vim.cmd([[
  function! EitriKeysLeaderChanged(d, k, z) abort
    call v:lua.__eitri_keys_schedule()
  endfunction
  call dictwatcheradd(g:, 'mapleader', 'EitriKeysLeaderChanged')
]])

-- Injected into an nvim past VimEnter: that event has fired and will not again.
if vim.v.vim_did_enter == 1 then
  schedule()
end

return function()
  torn = true
  pcall(vim.api.nvim_del_augroup_by_id, group)
  -- `vim.g` is not a Vimscript dictionary, so the watcher is removed from Vimscript, where it was added.
  pcall(vim.cmd, [[
    silent! call dictwatcherdel(g:, 'mapleader', 'EitriKeysLeaderChanged')
    silent! delfunction EitriKeysLeaderChanged
  ]])
  if rawget(_G, "__eitri_keys_schedule") == schedule then
    _G.__eitri_keys_schedule = nil
  end
  if rawget(_G, "__eitri_keys_rescan") == rescan then
    _G.__eitri_keys_rescan = nil
  end
end
