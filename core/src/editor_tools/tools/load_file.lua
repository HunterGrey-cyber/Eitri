-- Needs the prelude's `__state` and `find_buf` ahead of it. Loads the file at `path`, a resolved name, into a
-- buffer without a prompt of its own in the person's editor, and answers `buf, nil`, `nil, 'swap'` (a swap file belongs to it and the load was
-- given up) or `nil, nil` (nothing could be loaded). A buffer that is already loaded is returned as it is, never
-- read again.
--
-- With a UI attached, anything that does not fit the message line ends in a hit-enter prompt that holds the
-- editor until the person presses a key, which then goes to the prompt. What the person's hooks print is one such
-- thing, and adding, loading and giving up a buffer each run them, so all of it happens under `:silent`; a hook
-- that throws still fails the load.
--
-- A swap file is the other: E325 gets through `:silent`. So the load also runs with the `A` flag of 'shortmess'
-- (taken off again whatever happens unless the person had it, with no autocommand, and only that flag, since a
-- hook may change the option meanwhile), which makes the editor skip a swap file that exists and take the next
-- free name (`.swo`, `.swn`, ...) in silence. A buffer whose swap file is not the first name has met another
-- swap file: the load is given up and the buffer, with the swap file it made, goes. Only a buffer this call
-- added is wiped.

-- Calls `run` under `:silent`. A command is text and cannot hold a function, so `run` waits on `__state` for the
-- one line that calls it. `run` has to catch what it throws: a chunk answers a table.
local function silently(run)
  __state.silently = function()
    __state.silently = nil
    run()
  end
  vim.api.nvim_cmd({ cmd = 'lua', args = { '_G.__eitri_agent.silently()' }, mods = { silent = true } }, {})
end

local function load_file_silently(path)
  local found = find_buf(path)
  if found and vim.api.nvim_buf_is_loaded(found) then
    return found, nil
  end
  local before = {}
  for _, b in ipairs(vim.api.nvim_list_bufs()) do
    before[b] = true
  end
  local swapped = false
  local group = vim.api.nvim_create_augroup('eitri_agent_load_file', { clear = true })
  vim.api.nvim_create_autocmd('SwapExists', {
    group = group,
    callback = function()
      swapped = true
      vim.v.swapchoice = 'q'
    end,
  })
  local added = nil
  local had_flag = vim.go.shortmess:find('A', 1, true) ~= nil
  local ok, failure = pcall(function()
    if not had_flag then
      vim.api.nvim_cmd({ cmd = 'set', args = { 'shortmess+=A' }, mods = { noautocmd = true } }, {})
    end
    added = vim.fn.bufadd(path)
    -- bufadd can answer an existing buffer that names the file through a link: only the same file counts.
    local name = vim.api.nvim_buf_get_name(added)
    if name ~= path and vim.uv.fs_realpath(name) ~= vim.uv.fs_realpath(path) then
      error('another file', 0)
    end
    vim.fn.bufload(added)
  end)
  if not had_flag then
    pcall(vim.api.nvim_cmd, { cmd = 'set', args = { 'shortmess-=A' }, mods = { noautocmd = true } }, {})
  end
  pcall(vim.api.nvim_del_augroup_by_id, group)
  if not ok and tostring(failure):find('E325', 1, true) then
    swapped = true
  end
  local loaded = ok and added ~= nil and vim.api.nvim_buf_is_loaded(added)
  if loaded and not swapped then
    local swapfile = vim.fn.swapname(added)
    swapped = swapfile ~= '' and not swapfile:find('%.swp$')
  end
  if loaded and not swapped then
    return added, nil
  end
  -- Given up: a buffer this call added goes, one that was already listed stays as the person had it.
  if added and vim.api.nvim_buf_is_valid(added) then
    if before[added] then
      pcall(vim.api.nvim_buf_delete, added, { force = true, unload = true })
    else
      pcall(vim.api.nvim_buf_delete, added, { force = true })
    end
  end
  if swapped then
    return nil, 'swap'
  end
  return nil, nil
end

local function load_file(path)
  local buf, why
  silently(function()
    buf, why = load_file_silently(path)
  end)
  return buf, why
end
