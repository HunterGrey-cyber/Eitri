local __stamp, args = ...
local api = vim.api
local takeover_options = select(3, ...)
local takeover_glue = rawget(_G, '__eitri_companion')
if type(takeover_options) == 'table' and takeover_options.takeover_chan then
  if not takeover_glue or takeover_glue.mode ~= 'takeover' or takeover_glue.torn
    or takeover_glue.chan ~= takeover_options.takeover_chan then
    return { refused = 'the takeover glue is gone' }
  end
end
if takeover_glue and takeover_glue.mode == 'takeover' then
  if type(takeover_options) ~= 'table' or takeover_options.takeover_chan ~= takeover_glue.chan
    or takeover_glue.torn then return { refused = 'the takeover glue is gone' } end
end
local __state = rawget(_G, '__eitri_agent')
if not __state then __state = { seq = {}, scopes = 0 }; _G.__eitri_agent = __state end
if takeover_glue and takeover_glue.mode == 'takeover' then
  if not __state.takeover_buffers then
    __state.takeover_buffers = {}
    local state = __state
    local torn = false
    table.insert(takeover_glue.teardowns, function()
      if torn then return end
      torn = true
      local view = state.view
      if view then
        view.cleared = true
        pcall(api.nvim_buf_del_extmark, view.buf, api.nvim_create_namespace('eitri_agent_view'), view.mark)
        pcall(api.nvim_del_autocmd, view.autocmd)
        if view.timer and not view.timer:is_closing() then view.timer:stop(); view.timer:close() end
      end
      pcall(api.nvim_del_augroup_by_name, 'eitri_agent_view')
      for _, win in ipairs(api.nvim_list_wins()) do pcall(api.nvim_win_del_var, win, 'eitri_agent_view') end
      for buf, record in pairs(state.takeover_buffers) do
        -- A buffer the person has used since the load is theirs: one they opened (bufadd leaves a buffer unlisted,
        -- opening it lists it) or one they entered again after it was unloaded.
        local used = false
        if api.nvim_buf_is_valid(buf) then
          if record.existed then
            local info = vim.fn.getbufinfo(buf)[1]
            used = info ~= nil and info.lastused >= record.at
          else
            used = vim.bo[buf].buflisted
          end
        end
        if api.nvim_buf_is_valid(buf) and not used and not vim.bo[buf].modified and #vim.fn.win_findbuf(buf) == 0 then
          pcall(api.nvim_buf_delete, buf, {unload=record.existed})
        end
      end
      if state.lsp then state.lsp.entries = {}; state.lsp.actions = {} end
      if rawget(_G, '__eitri_agent') == state then _G.__eitri_agent = nil end
    end)
  end
end
-- Only this invocation's file loader is wrapped; the person's vim.fn table is never changed.
local vim = vim
if takeover_glue and takeover_glue.mode == 'takeover' and args.load then
  local takeover_existing = {}
  for _, buf in ipairs(api.nvim_list_bufs()) do takeover_existing[buf] = true end
  local original = vim
  vim = setmetatable({fn=setmetatable({bufload=function(buf)
    if not api.nvim_buf_is_loaded(buf) then
      -- bufadd has already allocated an id; a previously unloaded buffer is kept as an unloaded buffer.
      __state.takeover_buffers[buf] = { existed = takeover_existing[buf] == true, at = os.time() }
    end
    return original.fn.bufload(buf)
  end}, {__index=original.fn})}, {__index=original})
end
do
  local sec, usec = vim.uv.gettimeofday()
  local now = sec * 1000 + usec / 1000
  if now > __stamp.deadline_ms then return { expired = true } end
  local last = __state.seq[__stamp.scope]
  if last and last.seq >= __stamp.seq then return { expired = true } end
  if not last then
    __state.scopes = __state.scopes + 1
    if __state.scopes > 64 then
      local oldest, at = nil, math.huge
      for scope, entry in pairs(__state.seq) do
        if entry.at < at then oldest, at = scope, entry.at end
      end
      __state.seq[oldest] = nil
      __state.scopes = __state.scopes - 1
    end
  end
  __state.seq[__stamp.scope] = { seq = __stamp.seq, at = now }
end
