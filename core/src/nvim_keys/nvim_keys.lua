-- Eitri nvim-keys feed (spec 2026-09-26 §3). Loaded with --cmd before the user's config; installs
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
-- Set by the teardown: a debounce already waiting in `defer_fn` must send nothing afterwards.
local torn = false

local function cut(s)
  if type(s) ~= "string" then
    return nil
  end
  return vim.fn.strcharpart(s, 0, MAX_TEXT)
end

local function report()
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
  return vim.json.encode({
    v = 1,
    mapleader = (type(leader) == "string" and leader ~= "") and vim.fn.keytrans(leader) or vim.NIL,
    timeoutlen = vim.o.timeoutlen,
    timeout = vim.o.timeout,
    maps = maps,
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
    local ok, payload = pcall(report)
    if ok and payload ~= last then
      last = payload
      send(payload)
    end
  end, 100)
end
_G.__eitri_keys_schedule = schedule

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
end
