-- Installed into a user's own running nvim by Eitri's companion panel, through one
-- `nvim_exec_lua(INSTALL, { chan, parts, extra })`. `chan` is the panel's RPC channel; `parts` is a
-- list of `{ name, src, opts }`, each an Eitri snippet that takes its options as `...` and returns
-- its own teardown; `extra.edge_socket` is where `edge()` writes a direction letter.
--
-- nvim has no event for a channel closing, a `kill -9` of the panel included, so a libuv timer asks
-- every 500 ms whether the channel still exists and, once it does not, runs every teardown. The
-- state lives in `_G`, never in `vim.g`, which cannot hold a timer.
local chan, parts, extra = ...
extra = extra or {}

-- One panel serves this nvim at a time. The panel that held it is told, over its own channel,
-- before its glue goes, so it can say "detached" instead of going on as if attached.
local previous = rawget(_G, "__eitri_companion")
if previous then
  if previous.chan ~= chan then
    pcall(vim.rpcnotify, previous.chan, "eitri_replaced", chan)
  end
  pcall(previous.teardown)
end

local state = { chan = chan, teardowns = {}, installed = {}, failed = {} }
_G.__eitri_companion = state
local torn = false

local function teardown()
  if torn then
    return
  end
  torn = true
  if rawget(_G, "__eitri_companion") == state then
    _G.__eitri_companion = nil
  end
  for i = #state.teardowns, 1, -1 do
    pcall(state.teardowns[i])
  end
  state.teardowns = {}
  local timer = state.timer
  if timer and not timer:is_closing() then
    timer:stop()
    timer:close()
  end
end
state.teardown = teardown

for _, part in ipairs(parts) do
  local fn, err = loadstring(part.src, "=eitri:" .. part.name)
  if not fn then
    state.failed[#state.failed + 1] = { part.name, tostring(err) }
  else
    local ok, td = pcall(fn, part.opts or {})
    if ok then
      state.installed[#state.installed + 1] = part.name
      if type(td) == "function" then
        state.teardowns[#state.teardowns + 1] = td
      end
    else
      state.failed[#state.failed + 1] = { part.name, tostring(td) }
    end
  end
end

-- For a navigator plugin's own edge hook (smart-splits' `at_edge`): the same letter the nav
-- fallback writes at nvim's edge. `false` once torn down, so a stale hook does nothing.
local LETTERS = { left = "L", down = "D", up = "U", right = "R" }
function state.edge(direction)
  local letter, socket = LETTERS[direction], extra.edge_socket
  if torn or not letter or not socket then
    return false
  end
  local ok, c = pcall(vim.fn.sockconnect, "pipe", socket, { rpc = false })
  if not ok or c == 0 then
    return false
  end
  pcall(vim.fn.chansend, c, letter .. "\n")
  pcall(vim.fn.chanclose, c)
  return true
end

-- `vim.api` is not allowed in a libuv callback (a fast event): hop to the main loop first.
state.timer = vim.uv.new_timer()
state.timer:start(500, 500, vim.schedule_wrap(function()
  if torn then
    return
  end
  local ok, info = pcall(vim.api.nvim_get_chan_info, chan)
  if ok and next(info) == nil then
    teardown()
  end
end))

return {
  version = 1,
  installed = state.installed,
  failed = state.failed,
  pid = vim.fn.getpid(),
  tmux = (vim.env.TMUX or "") ~= "",
}
