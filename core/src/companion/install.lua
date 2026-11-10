-- Installed into a user's own running nvim by Eitri's companion panel, through one
-- `nvim_exec_lua(INSTALL, { chan, parts, extra })`. `chan` is the panel's RPC channel; `parts` is a
-- list of `{ name, src, opts }`, each an Eitri snippet that takes its options as `...` and returns
-- its own teardown or a table of teardown and calls; `extra.edge_socket` serves `edge()`.
-- `extra.slot` selects an independent page slot; absent, it selects the agent slot.
--
-- nvim has no event for a channel closing, a `kill -9` of the panel included, so a libuv timer asks
-- every 500 ms whether the channel still exists and, once it does not, runs every teardown. The
-- state lives in `_G`, never in `vim.g`, which cannot hold a timer.
local chan, parts, extra = ...
extra = extra or {}

-- A takeover owns the whole nvim, including the window marker its commands install.
-- Refuse before checking a page's slot or touching any previous holder.
local agent = rawget(_G, "__eitri_companion")
if type(agent) == "table" and agent.mode == "takeover" then
  return { refused = "takeover" }
end
if type(rawget(_G, "__eitri_window")) == "table" then
  return { refused = "eitri_window" }
end
local takeover = extra.mode == "takeover"

-- Each slot has one holder. Tell its previous holder before removing the glue,
-- so it can say "detached" instead of going on as if attached.
local page_id
if extra.slot ~= nil then
  assert(type(extra.slot) == "table" and extra.slot.kind == "page", "invalid page slot")
  page_id = extra.slot.id
  assert(type(page_id) == "string" and #page_id >= 1 and #page_id <= 40
    and page_id:match("^[a-z0-9][a-z0-9%-]*$") and page_id:sub(-1) ~= "-", "invalid page id")
end
local pages = rawget(_G, "__eitri_pages")
if takeover and pages then
  -- A teardown removes its entry, so collect holders before walking them.
  local holders = {}
  for _, page in pairs(pages) do holders[#holders + 1] = page end
  for _, page in ipairs(holders) do
    pcall(vim.rpcnotify, page.chan, "eitri_replaced", chan)
    pcall(page.teardown)
  end
  pages = rawget(_G, "__eitri_pages")
end
local previous
if page_id then
  previous = pages and pages[page_id]
else
  previous = rawget(_G, "__eitri_companion")
end
if previous then
  if previous.chan ~= chan then
    pcall(vim.rpcnotify, previous.chan, "eitri_replaced", chan)
  end
  pcall(previous.teardown)
end

local state = { chan = chan, teardowns = {}, calls = {}, installed = {}, failed = {}, torn = false }
if takeover then state.mode = "takeover" end
if page_id then
  pages = rawget(_G, "__eitri_pages") or {}
  _G.__eitri_pages = pages
  pages[page_id] = state
else
  _G.__eitri_companion = state
end
local torn = false

local function teardown()
  if torn then
    return
  end
  torn = true
  state.torn = true
  if not page_id and rawget(_G, "__eitri_companion") == state then
    _G.__eitri_companion = nil
  end
  for i = #state.teardowns, 1, -1 do
    pcall(state.teardowns[i])
  end
  if page_id and pages[page_id] == state then
    pages[page_id] = nil
    if next(pages) == nil and rawget(_G, "__eitri_pages") == pages then
      _G.__eitri_pages = nil
    end
  end
  state.teardowns = {}
  state.calls = {}
  local timer = state.timer
  if timer and not timer:is_closing() then
    timer:stop()
    timer:close()
  end
end
state.teardown = teardown
if takeover then
  function state.take_back()
    if torn then return false end
    -- Fork cleanup may remove its record, so keep the channel before tearing down.
    local fork = rawget(_G, "__neovide_attach")
    local fork_chan = type(fork) == "table" and fork.chan or nil
    teardown()
    if fork_chan then pcall(vim.fn.chanclose, fork_chan) end
    return true
  end
end

function state.adopt(fn)
  if torn or type(fn) ~= "function" then
    return false
  end
  state.teardowns[#state.teardowns + 1] = fn
  return true
end

for _, part in ipairs(parts) do
  local fn, err = loadstring(part.src, "=eitri:" .. part.name)
  if not fn then
    state.failed[#state.failed + 1] = { part.name, tostring(err) }
  else
    local opts = part.opts or {}
    opts.generation = extra.generation
    local ok, td = pcall(fn, opts)
    if ok then
      state.installed[#state.installed + 1] = part.name
      if type(td) == "function" then
        state.adopt(td)
      elseif type(td) == "table" then
        state.adopt(td.teardown)
        for name, call in pairs(td.calls or {}) do
          if type(call) == "function" then
            state.calls[name] = call
          end
        end
      end
    else
      state.failed[#state.failed + 1] = { part.name, tostring(td) }
    end
  end
end

-- For a navigator plugin's own edge hook (smart-splits' `at_edge`): the same letter the nav
-- fallback writes at nvim's edge. `false` once torn down, so a stale hook does nothing.
if not page_id then
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
    local prefix = extra.generation and ("G " .. extra.generation .. " ") or ""
    pcall(vim.fn.chansend, c, prefix .. letter .. "\n")
    pcall(vim.fn.chanclose, c)
    return true
  end
end

-- `vim.api` is not allowed in a libuv callback (a fast event): hop to the main loop first.
state.timer = vim.uv.new_timer()
state.timer:start(500, 500, vim.schedule_wrap(function()
  if torn then
    return
  end
  -- A closed channel has no id. nvim 0.10 answers it with a typed empty dictionary
  -- ({[vim.type_idx] = vim.types.dictionary}), which next() does not see as empty.
  local ok, info = pcall(vim.api.nvim_get_chan_info, chan)
  if ok and info.id == nil then
    teardown()
    return
  end
  if takeover then
    -- The fork's channel is kept from the first tick that sees its record: the record takes itself out once that
    -- channel is gone (its own look at the channel), so a later tick may find no record at all.
    if state.fork_chan == nil then
      local fork = rawget(_G, "__neovide_attach")
      state.fork_chan = type(fork) == "table" and fork.chan or nil
    end
    if state.fork_chan then
      local fork_ok, fork_info = pcall(vim.api.nvim_get_chan_info, state.fork_chan)
      if fork_ok and fork_info.id == nil then teardown() end
    end
  end
end))

return {
  version = 2,
  slot = page_id and "page" or "agent",
  installed = state.installed,
  failed = state.failed,
  pid = vim.fn.getpid(),
  tmux = (vim.env.TMUX or "") ~= "",
}
