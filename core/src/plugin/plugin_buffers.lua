-- Buffer operations take only arguments. A preload owns one hidden buffer and its own assistance policy;
-- it never changes the directory-wide assist registry.
local op, opts = ...
local uv = vim.uv or vim.loop
local reg = rawget(_G, '__eitri_plugin_preloads')
if reg == nil then
  reg = { owners = {}, last = {} }
  rawset(_G, '__eitri_plugin_preloads', reg)
end

local function checked(path)
  if type(path) ~= 'string' or path:sub(1, 1) ~= '/' or path:find('[%z\1-\31\127]')
      or uv.fs_realpath(path) ~= path then
    error('plugin_buffers: the path no longer resolves to itself')
  end
  return path
end

-- vim.fn.bufnr takes a name pattern and accepts a partial match (a.rs finds a.rs.orig); this compares whole names.
local function named(name)
  local full = vim.fn.fnamemodify(name, ':p')
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    local have = vim.api.nvim_buf_get_name(buf)
    if have == name or have == full then
      return buf
    end
  end
  return -1
end

local function made_last(target)
  target = target or reg
  if opts.order == nil then
    return true
  end
  local last = target.last[opts.owner]
  if last and last.maker == opts.maker and opts.order < last.order then
    return false
  end
  target.last[opts.owner] = { maker = opts.maker, order = opts.order }
  return true
end

local function clients(filter)
  return vim.lsp and vim.lsp.get_clients and vim.lsp.get_clients(filter or {}) or {}
end

local function attached(buf)
  return #clients({ bufnr = buf }) > 0
end

-- Configurations turn hints on from an attach, a scheduled call or a later capability registration; the policy
-- is put back at each of these delays after an attach, the same ones the assist snippet uses.
local RETRY_MS = { 100, 500, 1500, 4000 }

local function read(buf)
  local d, h
  if vim.diagnostic and vim.diagnostic.is_enabled then
    d = vim.diagnostic.is_enabled({ bufnr = buf })
  end
  if vim.lsp and vim.lsp.inlay_hint and vim.lsp.inlay_hint.is_enabled then
    h = vim.lsp.inlay_hint.is_enabled({ bufnr = buf })
  end
  return { diagnostics = d, inlay_hints = h }
end

local function set(buf, policy)
  if policy.diagnostics ~= nil and vim.diagnostic and vim.diagnostic.enable then
    vim.diagnostic.enable(policy.diagnostics, { bufnr = buf })
  end
  if policy.inlay_hints ~= nil and vim.lsp and vim.lsp.inlay_hint and vim.lsp.inlay_hint.enable then
    vim.lsp.inlay_hint.enable(policy.inlay_hints, { bufnr = buf })
  end
end

local function wipe()
  local state = reg.owners[opts.owner]
  reg.owners[opts.owner] = nil
  pcall(vim.api.nvim_del_augroup_by_name, 'eitri_plugin_preload_' .. opts.owner)
  if not state or not vim.api.nvim_buf_is_valid(state.buf) then
    return false, 0
  end
  local buf = state.buf
  if vim.b[buf].eitri_plugin_preload ~= opts.owner or vim.b[buf].eitri_plugin_preload_own == nil
      or vim.bo[buf].modified or #vim.fn.win_findbuf(buf) > 0 then
    return false, 0
  end
  local ids = vim.b[buf].eitri_plugin_preload_clients or {}
  vim.api.nvim_buf_delete(buf, { force = false })
  local stopped = 0
  for _, id in ipairs(ids) do
    local client = vim.lsp.get_client_by_id(id)
    if client then
      local held = false
      for attached in pairs(client.attached_buffers) do
        if attached ~= buf then
          held = true
          break
        end
      end
      if not held then
        client:stop()
        stopped = stopped + 1
      end
    end
  end
  return true, stopped
end

if op == 'diff' or op == 'close_diffs' then
  local api = vim.api
  local diffs = rawget(_G, '__eitri_plugin_diffs')
  if not diffs then
    diffs = { last = {} }
    rawset(_G, '__eitri_plugin_diffs', diffs)
  end
  local function tabvar(tab, key)
    local ok, value = pcall(api.nvim_tabpage_get_var, tab, key)
    return ok and value or nil
  end
  if op == 'close_diffs' then
    if not made_last(diffs) then return { closed = 0 } end
    local current, closed = api.nvim_get_current_tabpage(), 0
    for _, tab in ipairs(api.nvim_list_tabpages()) do
      local newer = opts.order and tabvar(tab, 'eitri_plugin_diff_maker') == opts.maker
        and (tabvar(tab, 'eitri_plugin_diff_order') or 0) > opts.order
      if tab ~= current and not newer and tabvar(tab, 'eitri_plugin_diff') == opts.owner then
        -- Closing a tab hides its modified normal buffers; it never deletes or writes them.
        local ok = pcall(api.nvim_cmd, { cmd = 'tabclose', bang = true,
          args = { tostring(api.nvim_tabpage_get_number(tab)) } }, {})
        if ok and not api.nvim_tabpage_is_valid(tab) then closed = closed + 1 end
      end
    end
    return { closed = closed }
  end
  local function validate(side)
    if side.path ~= nil then
      checked(side.path)
    elseif type(side.title) ~= 'string' or side.title == '' or vim.fn.strchars(side.title) > 80
        or side.title:find('[%z\1-\31\127]') or type(side.text) ~= 'string' or #side.text > 96 * 1024
        or side.text:find('[%z\1-\8\11-\31\127]') or side.text:find('\194[\128-\159]')
        or (side.filetype ~= nil and (type(side.filetype) ~= 'string' or #side.filetype < 1
          or #side.filetype > 32 or side.filetype:find('[^a-z0-9_]'))) then
      error('plugin_buffers: invalid diff text side')
    end
  end
  validate(opts.left)
  validate(opts.right)
  local closed = diffs.last[opts.owner]
  if opts.order and closed and closed.maker == opts.maker and opts.order < closed.order then
    return { tab = api.nvim_get_current_tabpage() }
  end
  local from = api.nvim_get_current_tabpage()
  if tabvar(from, 'eitri_plugin_diff') == opts.owner then
    local origin = tabvar(from, 'eitri_plugin_diff_from')
    if type(origin) == 'number' and api.nvim_tabpage_is_valid(origin) then from = origin end
  end
  local tab, scratch = nil, {}
  local function show(side)
    if side.path ~= nil then
      local path = checked(side.path)
      -- bufadd can return an existing buffer named by a link, so ownership is checked by handle.
      local existing = {}
      for _, buf in ipairs(api.nvim_list_bufs()) do
        existing[buf] = { loaded = api.nvim_buf_is_loaded(buf) }
      end
      local buf = vim.fn.bufadd(path)
      local before = existing[buf]
      vim.fn.bufload(buf)
      if uv.fs_realpath(path) == path and uv.fs_realpath(api.nvim_buf_get_name(buf)) == path then
        api.nvim_win_set_buf(0, buf)
        -- A file shown in a window is one the user edits, so it belongs in the buffer list like an :edit.
        vim.bo[buf].buflisted = true
      end
      -- Reading or showing a file can run autocommands that rename it or retarget a symlink.
      if uv.fs_realpath(path) ~= path or uv.fs_realpath(api.nvim_buf_get_name(buf)) ~= path
          or api.nvim_get_current_buf() ~= buf then
        if before == nil then
          pcall(api.nvim_buf_delete, buf, { force = true })
        elseif not before.loaded then
          pcall(api.nvim_buf_delete, buf, { unload = true, force = true })
        end
        error('plugin_buffers: the loaded diff file no longer resolves to the checked path')
      end
      return
    end
    -- Text is a view, and must not trigger a filetype server or any file-reading autocommand.
    local events = vim.o.eventignore
    vim.o.eventignore = 'all'
    local ok, why = pcall(function()
      local buf = api.nvim_create_buf(false, true)
      scratch[#scratch + 1] = buf
      vim.bo[buf].buftype = 'nofile'
      vim.bo[buf].bufhidden = 'wipe'
      vim.bo[buf].swapfile = false
      vim.bo[buf].modeline = false
      local name = 'plugin:' .. opts.owner .. ': ' .. side.title
      local unique, n = name, 1
      while named(unique) >= 0 do
        n = n + 1
        unique = name .. ' [' .. n .. ']'
      end
      api.nvim_buf_set_name(buf, unique)
      local lines = vim.split(side.text, '\n', { plain = true })
      if #lines > 1 and lines[#lines] == '' then table.remove(lines) end
      api.nvim_buf_set_lines(buf, 0, -1, false, lines)
      if side.filetype then vim.bo[buf].filetype = side.filetype end
      vim.bo[buf].modified = false
      vim.bo[buf].modifiable = false
      api.nvim_win_set_buf(0, buf)
    end)
    vim.o.eventignore = events
    if not ok then error(why) end
    -- Syntax loading is separate from FileType, whose hooks commonly start language servers.
    if side.filetype then vim.bo[api.nvim_get_current_buf()].syntax = side.filetype end
  end
  -- :tabnew and :vnew each start their window on a new empty buffer; the side replaces it, and an empty buffer
  -- that is still listed afterwards would collect in the buffer list with every diff.
  local function placeholder()
    local buf = api.nvim_get_current_buf()
    if api.nvim_buf_get_name(buf) == '' and vim.bo[buf].buftype == '' and not vim.bo[buf].modified
        and api.nvim_buf_line_count(buf) == 1 and api.nvim_buf_get_lines(buf, 0, 1, false)[1] == '' then
      vim.bo[buf].bufhidden = 'wipe'
    end
  end
  local ok, why = pcall(function()
    api.nvim_cmd({ cmd = 'tabnew' }, {})
    tab = api.nvim_get_current_tabpage()
    api.nvim_tabpage_set_var(tab, 'eitri_plugin_diff', opts.owner)
    api.nvim_tabpage_set_var(tab, 'eitri_plugin_diff_from', from)
    if opts.order then
      api.nvim_tabpage_set_var(tab, 'eitri_plugin_diff_maker', opts.maker)
      api.nvim_tabpage_set_var(tab, 'eitri_plugin_diff_order', opts.order)
    end
    placeholder()
    show(opts.left)
    api.nvim_cmd({ cmd = 'diffthis' }, {})
    -- Start the other window on an empty buffer so splitting never shares a wipe-on-hide scratch.
    api.nvim_cmd({ cmd = 'vnew', mods = { split = 'botright' } }, {})
    placeholder()
    show(opts.right)
    api.nvim_cmd({ cmd = 'diffthis' }, {})
  end)
  if not ok then
    if tab and api.nvim_tabpage_is_valid(tab) then
      pcall(api.nvim_cmd, { cmd = 'tabclose', args = { tostring(api.nvim_tabpage_get_number(tab)) } }, {})
    end
    for _, buf in ipairs(scratch) do
      if api.nvim_buf_is_valid(buf) then pcall(api.nvim_buf_delete, buf, { force = true }) end
    end
    error(why)
  end
  return { tab = tab }
end

if op == 'wipe_preload' then
  if not made_last() then
    return { replaced = false, stopped_clients = 0 }
  end
  local replaced, stopped = wipe()
  return { replaced = replaced, stopped_clients = stopped }
elseif op == 'preload' then
  local path = checked(opts.path)
  if not made_last() then
    return { buf = 0, replaced = false, stopped_clients = 0 }
  end
  local old = reg.owners[opts.owner]
  local existing = named(path)
  local replaced, stopped = false, 0
  if not old or old.buf ~= existing then
    replaced, stopped = wipe()
  end
  local buf = vim.fn.bufadd(path)
  -- A buffer already shown or edited belongs to the user, and an assisted buffer already has a policy.
  local assist = rawget(_G, '__eitri_plugin_assist')
  if #vim.fn.win_findbuf(buf) > 0 or vim.bo[buf].modified or (assist and assist.buffers[buf])
      or (vim.api.nvim_buf_is_loaded(buf) and vim.b[buf].eitri_plugin_preload ~= opts.owner) then
    return { buf = buf, replaced = replaced, stopped_clients = stopped }
  end
  local before = {}
  for _, client in ipairs(clients()) do
    before[client.id] = true
  end
  local own = vim.b[buf].eitri_plugin_preload_own
  if own == nil then
    own = read(buf)
    -- Before any server attaches, hints read as off whatever the configuration will do on the attach; that is
    -- no value to restore, so the hint value waits for the first look after an attach, and until then the
    -- preload writes no hint value of its own (which would be read back in place of the configuration's).
    if own.inlay_hints ~= nil and not attached(buf) then
      own.hint_pending = true
    end
    vim.b[buf].eitri_plugin_preload_own = own
  end
  vim.b[buf].eitri_plugin_preload = opts.owner
  local ids = vim.b[buf].eitri_plugin_preload_clients or {}
  vim.b[buf].eitri_plugin_preload_clients = ids
  local state = { buf = buf }
  reg.owners[opts.owner] = state
  local function active()
    return reg.owners[opts.owner] == state and vim.api.nvim_buf_is_valid(buf)
      and vim.b[buf].eitri_plugin_preload_own ~= nil
  end
  local function hold()
    if active() then
      local hints = opts.inlay_hints
      if vim.b[buf].eitri_plugin_preload_own.hint_pending then
        hints = nil
      end
      set(buf, { diagnostics = opts.diagnostics, inlay_hints = hints })
    end
  end
  -- Takes the pending hint value once a server is attached and the configuration's attach handlers have run,
  -- then holds the policy.
  local function settle()
    if not active() then
      return
    end
    local mine = vim.b[buf].eitri_plugin_preload_own
    if mine.hint_pending and attached(buf) then
      mine.inlay_hints = read(buf).inlay_hints
      mine.hint_pending = nil
      vim.b[buf].eitri_plugin_preload_own = mine
    end
    hold()
  end
  local function record()
    if not active() then
      return
    end
    for _, client in ipairs(clients({ bufnr = buf })) do
      if not before[client.id] and not vim.tbl_contains(ids, client.id) then
        ids[#ids + 1] = client.id
      end
    end
    vim.b[buf].eitri_plugin_preload_clients = ids
  end
  local group = vim.api.nvim_create_augroup('eitri_plugin_preload_' .. opts.owner, { clear = true })
  vim.api.nvim_create_autocmd('LspAttach', {
    group = group, buffer = buf,
    callback = function()
      record()
      hold()
      vim.schedule(function() record() hold() end)
      for _, ms in ipairs(RETRY_MS) do
        vim.defer_fn(settle, ms)
      end
    end,
  })
  hold()
  vim.fn.bufload(buf)
  -- Autocommands may have changed the name or the symlink while the file was read.
  if uv.fs_realpath(path) ~= path or vim.api.nvim_buf_get_name(buf) ~= path then
    wipe()
    error('plugin_buffers: the loaded file no longer resolves to the checked path')
  end
  record()
  hold()
  -- A buffer preloaded again after its server attached gets no new attach to settle a pending hint value on.
  if vim.b[buf].eitri_plugin_preload_own.hint_pending and attached(buf) then
    for _, ms in ipairs(RETRY_MS) do
      vim.defer_fn(settle, ms)
    end
  end
  return { buf = buf, replaced = replaced, stopped_clients = stopped }
elseif op == 'modified' or op == 'reload' then
  local root = checked(opts.root)
  local prefix = root == '/' and '/' or root .. '/'
  local paths, reloaded, kept = {}, 0, 0
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].buftype == '' then
      local name = vim.api.nvim_buf_get_name(buf)
      local path = uv.fs_realpath(name)
      -- Only a buffer whose file still exists can be reloaded; its name may be a symlink to the file.
      local exists = path ~= nil
      -- Unsaved work survives a removed file or directory. Resolve the nearest existing ancestor so a
      -- symlinked parent still cannot make a buffer outside the root look like one inside it.
      if not path and name ~= '' then
        local parent, suffix = vim.fs.dirname(name), '/' .. vim.fs.basename(name)
        while parent do
          local resolved = uv.fs_realpath(parent)
          if resolved then
            path = vim.fs.normalize(resolved .. suffix)
            break
          end
          local next_parent = vim.fs.dirname(parent)
          if next_parent == parent then
            break
          end
          suffix = '/' .. vim.fs.basename(parent) .. suffix
          parent = next_parent
        end
      end
      if path and path:sub(1, #prefix) == prefix then
        if vim.bo[buf].modified then
          paths[#paths + 1] = path
          kept = kept + 1
        elseif op == 'reload' and exists then
          local tick = vim.api.nvim_buf_get_changedtick(buf)
          local autoread = vim.bo[buf].autoread
          vim.bo[buf].autoread = true
          -- checktime preserves unchanged buffers and reloads changed ones without an interactive prompt.
          local ok, err = pcall(vim.api.nvim_cmd, {
            cmd = 'checktime', args = { tostring(buf) }, magic = { file = false },
          }, {})
          if vim.api.nvim_buf_is_valid(buf) then
            vim.bo[buf].autoread = autoread
            if vim.api.nvim_buf_get_changedtick(buf) ~= tick then
              reloaded = reloaded + 1
            end
          end
          if not ok then
            error(err)
          end
        end
      end
    end
  end
  table.sort(paths)
  if op == 'modified' then
    return { paths = paths }
  end
  return { reloaded = reloaded, kept_modified = kept }
end
error('plugin_buffers: unknown op ' .. tostring(op))
