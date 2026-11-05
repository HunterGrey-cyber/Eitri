local origin, eitri_chan = ...
local opts = type(origin) == 'table' and origin or { origin = origin, eitri_chan = eitri_chan }
origin, eitri_chan = opts.origin, opts.eitri_chan
local api = vim.api
local torn = false
local state = {}
local ns = api.nvim_create_namespace('eitri_takeover_room')
local group = api.nvim_create_augroup('EitriTakeoverRoom', { clear = true })
local timer = vim.uv.new_timer()
local origin_chan = vim.fn.sockconnect('pipe', origin, { rpc = true })
if origin_chan == 0 then
  timer:close()
  api.nvim_del_augroup_by_id(group)
  error('could not connect the takeover room to the origin')
end
local leaving = false
local returning = false
local quitting = false
local unreachable = false
local queued = false
local buf

local function notify(method, ...)
  pcall(vim.rpcnotify, eitri_chan, method, ...)
end

local function channel_open(chan)
  local ok, info = pcall(api.nvim_get_chan_info, chan)
  return ok and info.id ~= nil
end

local function guarded()
  if torn or _G.__eitri_room ~= state or vim.v.servername == origin then return false end
  for _, b in ipairs(api.nvim_list_bufs()) do
    if vim.bo[b].buftype ~= 'nofile' or vim.bo[b].modified then return false end
  end
  return true
end

local function show(lines)
  if not buf or not api.nvim_buf_is_valid(buf) then return end
  vim.bo[buf].modifiable = true
  api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modified = false
  vim.bo[buf].modifiable = false
  vim.cmd.redraw()
end

local copy = {
  'This nvim is open in Eitri. It comes back here when Eitri lets go of it. <Enter> takes it back now.',
  'If the screen is blank when it comes back, nvim is waiting for a key: press Esc.',
}

-- An origin that is waiting for a key cannot redraw when the terminal arrives, so the terminal keeps
-- whatever this screen showed last. It must not keep a promise that is no longer true.
local function blank()
  vim.opt.fillchars:append('eob: ')
  show({ '' })
  vim.cmd.redraw({ bang = true })
end

local function unblank()
  vim.opt.fillchars:remove('eob')
  show(copy)
end

function state.teardown()
  if torn then return end
  torn = true
  timer:stop()
  if not timer:is_closing() then timer:close() end
  vim.on_key(nil, ns)
  pcall(api.nvim_del_augroup_by_id, group)
  if buf and api.nvim_buf_is_valid(buf) then pcall(vim.keymap.del, 'n', '<CR>', { buffer = buf }) end
  pcall(vim.fn.chanclose, origin_chan)
  if _G.__eitri_room == state then _G.__eitri_room = nil end
end

function state.return_tui()
  if not guarded() then return false end
  if #api.nvim_list_uis() == 0 then return false end
  if returning then return true end
  local ok, probe = pcall(vim.fn.sockconnect, 'pipe', origin, { rpc = true })
  if not ok or probe == 0 then
    show({ 'nvim is no longer reachable at ' .. origin })
    if not unreachable then notify('origin_unreachable') end
    unreachable = true
    return false
  end
  pcall(vim.fn.chanclose, probe)
  returning = true
  -- Reply to let_go before replacing the room's UI and ending its RPC connections.
  vim.schedule(function()
    if torn then return end
    if not guarded() then returning = false; return end
    leaving = true
    blank()
    -- The address is a literal: `%` and `#` in a socket path are not file-name placeholders.
    local connected = pcall(vim.cmd.connect, { args = { origin }, bang = true, magic = { file = false } })
    if not connected then
      leaving = false
      returning = false
      unblank()
    end
  end)
  return true
end

function state.moving() state.moving_sent = true; return true end
function state.pending() state.pending_sent = true; return true end
function state.held() state.is_held = true; return true end
function state.let_go()
  if torn then return 'refused' end
  -- No UI to give back: once Eitri's link closes there is nothing left to wait for, a move sent earlier included.
  if #api.nvim_list_uis() == 0 then state.let_go_empty = true; return 'no_ui' end
  return state.return_tui() and 'returned' or 'refused'
end
function state.quit()
  if torn or quitting then return true end
  -- A misplaced install may not end a server holding the person's buffers either.
  if not guarded() then return false end
  quitting = true
  vim.schedule(function()
    if torn then return end
    if guarded() then vim.cmd.qall() end
  end)
  return true
end

local function take_back()
  if torn then return end
  notify('take_back')
  pcall(vim.rpcnotify, origin_chan, 'nvim_exec_lua',
    'return _G.__eitri_companion and _G.__eitri_companion.take_back()', {})
  if not state.is_held or not channel_open(eitri_chan) then
    state.return_tui()
  else
    vim.defer_fn(function()
      if torn then return end
      state.return_tui()
    end, 2000)
  end
end

local function setup()
  if torn then return end
  local current = api.nvim_get_current_buf()
  local lines = api.nvim_buf_get_lines(current, 0, -1, false)
  if not vim.bo[current].modified and api.nvim_buf_get_name(current) == ''
      and #lines == 1 and lines[1] == '' then
    buf = current
  else
    buf = api.nvim_create_buf(false, true)
    api.nvim_set_current_buf(buf)
  end
  vim.bo[buf].buftype = 'nofile'
  vim.bo[buf].swapfile = false
  vim.bo[buf].bufhidden = 'hide'
  vim.o.laststatus = 0
  vim.o.ruler = false
  vim.o.showmode = false
  vim.o.more = false
  vim.opt.shortmess:append('aoOstTWIcCF')
  show(copy)
  vim.on_key(function(_, typed) if typed ~= '\r' then return '' end end, ns)
  vim.keymap.set('n', '<CR>', take_back, { buffer = buf, silent = true, nowait = true })
  api.nvim_create_autocmd('UIEnter', { group = group, callback = function()
    if torn then return end
    notify('ui', true)
  end })
  api.nvim_create_autocmd('UILeave', { group = group, callback = function()
    if torn then return end
    if not leaving then notify('ui', false) end
  end })
  timer:start(250, 250, function()
    if torn or queued then return end
    queued = true
    vim.schedule(function()
      if torn then return end
      queued = false
      if not channel_open(origin_chan) then state.quit(); return end
      if not channel_open(eitri_chan) then
        if #api.nvim_list_uis() > 0 then
          if not unreachable then state.return_tui() end
        elseif state.let_go_empty or (not state.moving_sent and not state.pending_sent) then
          state.quit()
        end
      end
    end)
  end)
end

_G.__eitri_room = state
if vim.v.vim_did_enter == 1 then
  setup()
else
  api.nvim_create_autocmd('VimEnter', { group = group, once = true, callback = function()
    if torn then return end
    setup()
  end })
end
return true
