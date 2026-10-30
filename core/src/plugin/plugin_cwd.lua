-- Changes a tab's directory for one owner and restores it only while it still has the value set here.
-- Every value arrives as an argument; directory names never become Lua or Ex source.
local op, opts = ...
local owner = opts.owner
local key = 'eitri_plugin_cwd_' .. owner
local api = vim.api
local uv = vim.uv or vim.loop

local last = rawget(_G, '__eitri_plugin_cwd_last')
if last == nil then
  last = {}
  rawset(_G, '__eitri_plugin_cwd_last', last)
end
local previous = last[owner]
local stale = opts.order ~= nil and previous ~= nil and previous.maker == opts.maker
  and opts.order < previous.order
if not stale and opts.order ~= nil then
  last[owner] = { maker = opts.maker, order = opts.order }
end

local function tabvar(tab, name)
  local ok, value = pcall(api.nvim_tabpage_get_var, tab, name)
  if ok then return value end
end

local function resolves(dir)
  return type(dir) == 'string' and dir:sub(1, 1) == '/' and uv ~= nil and uv.fs_realpath(dir) == dir
end

local function tcd(dir)
  if not resolves(dir) then
    error('plugin_cwd: the directory no longer resolves to itself')
  end
  api.nvim_cmd({ cmd = 'tcd', args = { dir }, magic = { file = false } }, {})
end

-- Runs `action` with one of the tab's windows current and restores the current one. Nvim saves and
-- restores the process directory around such a call only when one side has a local directory, so the
-- callers below change a directory this way only in a tab that already has a tab-local one; otherwise
-- the `tcd` would leave the process, and with it the tab the user is in, in the new directory.
local function in_tab(tab, action)
  return api.nvim_win_call(api.nvim_tabpage_get_win(tab), action)
end

if op == 'cd' then
  local tab = api.nvim_get_current_tabpage()
  local origin = nil
  if tabvar(tab, 'eitri_plugin_diff') == owner then
    -- A diff tab stands for the tab it was opened from; with that tab gone, the change stays here.
    local from = tabvar(tab, 'eitri_plugin_diff_from')
    if type(from) == 'number' and api.nvim_tabpage_is_valid(from) then
      origin = from
      tab = from
    end
  end
  if stale then return { dir = opts.dir, tab = tab } end
  if not resolves(opts.dir) then
    error('plugin_cwd: the directory no longer resolves to itself')
  end
  -- The change is made in the current tab, so nvim moves the process directory with the tab it is
  -- shown in; the origin of a diff tab is selected first and stays selected.
  if origin ~= nil then api.nvim_set_current_tabpage(origin) end
  local state = tabvar(tab, key) or {
    baseline_local = vim.fn.haslocaldir(-1, 0),
    baseline_dir = vim.fn.getcwd(-1, 0),
  }
  tcd(opts.dir)
  state.last_set = opts.dir
  api.nvim_tabpage_set_var(tab, key, state)
  return { dir = opts.dir, tab = tab }
elseif op == 'clear' then
  local restored, moved = 0, 0
  if stale then return { restored = restored, moved = moved } end
  for _, tab in ipairs(api.nvim_list_tabpages()) do
    local state = tabvar(tab, key)
    if state ~= nil then
      local done = false
      -- A tab with no tab-local directory any more had it replaced by a global `:cd` there: the user
      -- moved it. A baseline that no longer resolves to itself is left as well, and the remaining
      -- tabs are still visited.
      pcall(in_tab, tab, function()
        if vim.fn.haslocaldir(-1, 0) == 1 and vim.fn.getcwd(-1, 0) == state.last_set then
          -- Nvim cannot remove a tab-local cwd. Using the current global cwd preserves its effective
          -- directory without changing the global cwd or any other tab.
          tcd(state.baseline_local == 1 and state.baseline_dir or vim.fn.getcwd(-1, -1))
          done = true
        end
      end)
      if done then restored = restored + 1 else moved = moved + 1 end
      pcall(api.nvim_tabpage_del_var, tab, key)
    end
  end
  return { restored = restored, moved = moved }
end
error('plugin_cwd: unknown op ' .. tostring(op))
