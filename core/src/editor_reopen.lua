-- The input is data only. File names reach :edit as absolute, checked arguments with file magic disabled.
local api, uv = vim.api, vim.uv
local socket, source = vim.env.EITRI_PANE_SWITCH_SOCKET, vim.env.EITRI_REOPEN_LUA
if not socket or socket == '' or not source or source == '' then
  return
end
local dir = source:match('^(.*)/')
if not dir then
  return
end

local function send(line)
  local ok, channel = pcall(vim.fn.sockconnect, 'pipe', socket, { rpc = false })
  if ok and channel ~= 0 then
    pcall(vim.fn.chansend, channel, line .. '\n')
    pcall(vim.fn.chanclose, channel)
  end
end

local function session_tool()
  if vim.v.this_session ~= '' then return 'this_session' end
  if _G.MiniSessions then return 'mini.sessions' end
  for _, name in ipairs({ 'persistence', 'auto-session', 'resession', 'possession', 'nvim-possession',
    'session_manager', 'persisted' }) do
    if package.loaded[name] then return name end
  end
  -- Inspect an already loaded lazy spec without loading a plugin manager or any session module.
  local lazy = package.loaded['lazy.core.config']
  if type(lazy) == 'table' and type(lazy.plugins) == 'table' then
    for _, pair in ipairs({ { 'persistence.nvim', 'persistence' }, { 'auto-session', 'auto-session' },
      { 'resession.nvim', 'resession' }, { 'possession.nvim', 'possession' },
      { 'nvim-possession', 'nvim-possession' }, { 'neovim-session-manager', 'session_manager' },
      { 'persisted.nvim', 'persisted' } }) do
      if lazy.plugins[pair[1]] then return pair[2] end
    end
  end
  if vim.g.startify_session_autoload == true or vim.g.startify_session_autoload == 1 then return 'startify' end
  if vim.g.loaded_prosession ~= nil and vim.g.loaded_prosession ~= false then return 'prosession' end
end

local function plain(buf)
  if vim.bo[buf].buftype ~= '' or api.nvim_buf_get_name(buf) ~= '' or vim.bo[buf].modified then return false end
  local lines = api.nvim_buf_get_lines(buf, 0, -1, false)
  return #lines == 1 and lines[1] == ''
end

local group = api.nvim_create_augroup('eitri_editor_reopen', { clear = true })
-- Keep startup removable by a configuration's ungrouped autocmd! command.
api.nvim_create_autocmd('VimEnter', {
  once = true,
  nested = true,
  callback = function()
    local known, reason = pcall(function() return vim.v.startreason end)
    local restarted = known and type(reason) == 'string' and reason:find('^restart') ~= nil
    local ok, input = pcall(function()
      local fd = assert(uv.fs_open(dir .. '/reopen-in.json', 'r', 0))
      local stat = uv.fs_fstat(fd)
      local text
      if stat and stat.type == 'file' and stat.size <= 256 * 1024 then
        text = uv.fs_read(fd, stat.size, 0)
      end
      uv.fs_close(fd)
      return vim.json.decode(assert(text))
    end)
    -- With no input there is no host token to answer; a well-formed off line remains useful to a reader.
    local token = ok and type(input) == 'table' and input.token or '0000000000000000'
    if type(token) ~= 'string' or #token ~= 16 or token:find('[^0-9a-f]') then return end
    local function aside(word) send('A ' .. token .. ' ' .. word) end
    if not ok or type(input) ~= 'table' then
      if not restarted then aside('off') end
      return
    end
    -- The window ends nvim by closing its stdin. By VimLeavePre such an nvim has unloaded its buffers, which puts
    -- every cursor in column 0; the UI leaving comes before that. The answer taken there lends its columns only when
    -- the exit's own answer has every column 0 and is otherwise the same: a change made after a UI left (another UI
    -- still attached) is then never lost to it.
    local at_leave
    local function columns_lost(fresh, kept, key)
      if key == 'col' and type(fresh) == 'number' and type(kept) == 'number' then return fresh == 0 end
      if type(fresh) ~= type(kept) then return false end
      if type(fresh) ~= 'table' then return fresh == kept end
      for k, v in pairs(fresh) do
        if not columns_lost(v, kept[k], k) then return false end
      end
      for k in pairs(kept) do
        if fresh[k] == nil then return false end
      end
      return true
    end
    local function handoff()
      local answer = assert(loadfile(dir .. '/editor_reopen_capture.lua'))(input)
      if answer and at_leave and columns_lost(vim.json.decode(answer.json), vim.json.decode(at_leave.json)) then
        answer = at_leave
      end
      if not answer then return end
      local content = vim.json.encode({ same = answer.same, record = vim.json.decode(answer.json) })
      if #content > 256 * 1024 then return end
      local bytes = uv.random(4)
      if not bytes then return end
      local suffix = (bytes:gsub('.', function(byte) return ('%02x'):format(byte:byte()) end))
      local temporary = dir .. '/reopen-out-' .. token .. '-' .. suffix .. '.tmp'
      local fd = uv.fs_open(temporary, 'wx', 384)
      if not fd then return end
      local success = pcall(function()
        assert(uv.fs_fchmod(fd, 384))
        local offset = 0
        while offset < #content do
          local count = assert(uv.fs_write(fd, content:sub(offset + 1), offset))
          assert(count > 0)
          offset = offset + count
        end
      end)
      local closed = uv.fs_close(fd)
      if success and closed and uv.fs_rename(temporary, dir .. '/reopen-out-' .. token .. '.json') then return end
      uv.fs_unlink(temporary)
    end
    -- Captures answer for the token from here on, and nvim's exit leaves its handoff.
    local function keep()
      api.nvim_create_autocmd('VimLeavePre', { group = group, callback = function() pcall(handoff) end })
      api.nvim_create_autocmd('UILeave', { group = group, callback = function()
        local ok, answer = pcall(function() return assert(loadfile(dir .. '/editor_reopen_capture.lua'))(input) end)
        at_leave = ok and answer or nil
      end })
    end
    -- A server `:restart` started runs these same `--cmd`s. What it shows is nvim's own restart (its session, or
    -- none with `!`), and the window has already had its answer for this input, so nothing is opened or said. It
    -- keeps the window's record from the editor's UI on: by then the nvim before it has exited, and the handoff it
    -- left would stand for a session that is gone, so it goes. A window that asks for no capture reads none of it.
    if restarted then
      local function take_over()
        pcall(uv.fs_unlink, dir .. '/reopen-out-' .. token .. '.json')
        package.loaded['eitri.reopen'] = { done = token }
        keep()
      end
      if #api.nvim_list_uis() > 0 then take_over()
      else api.nvim_create_autocmd('UIEnter', { group = group, once = true, callback = take_over }) end
      return
    end
    if vim.fn.argc() > 0 then aside('args'); return end
    local tool = session_tool()
    if tool then aside(tool); return end
    local initial_win = api.nvim_get_current_win()
    if #api.nvim_list_tabpages() ~= 1 or not plain(api.nvim_get_current_buf()) then aside('screen'); return end
    -- Floating windows (a progress popup, a notification) are not part of the screen's layout.
    local function layout_windows()
      local windows = {}
      for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
        if api.nvim_win_get_config(win).relative == '' then windows[#windows + 1] = win end
      end
      return windows
    end
    local initial_windows = layout_windows()
    local root = vim.base64.decode(input.root)
    local started, elapsed = uv.hrtime(), 0
    send('B ' .. token)

    local opened, gone, failed, squeezed = 0, 0, 0, 0
    local first_gone, first_failed
    local swap = api.nvim_create_autocmd('SwapExists', {
      group = group,
      callback = function()
        local success, info = pcall(vim.fn.swapinfo, vim.v.swapname)
        -- An unreadable swap is kept safe too: opening read-only never destroys recoverable work.
        if not success or info.error or info.dirty ~= 0 or info.pid ~= 0 then vim.v.swapchoice = 'o'
        else vim.v.swapchoice = 'e' end
      end,
    })
    local function error_line(err)
      io.stderr:write('[editor] reopen: ' .. tostring(err):match('[^\r\n]*') .. '\n')
    end
    local function lost(name, kind, err)
      if kind == 'g' then
        gone = gone + 1
        first_gone = first_gone or name
      else
        failed = failed + 1
        first_failed = first_failed or name
        if err then error_line(err) end
      end
    end
    local function checked(encoded)
      local decoded, name = pcall(vim.base64.decode, encoded)
      if not decoded or #name == 0 or #name > 4096 or name:sub(1, 1) ~= '/' or name:find('\0', 1, true) then
        return nil, 'g'
      end
      local stat = uv.fs_stat(name)
      if not stat or stat.type ~= 'file' then return name, 'g' end
      if not uv.fs_access(name, 'R') then return name, 'f' end
      return name
    end
    local function visit(node, fn)
      if node.leaf then fn(node)
      else for _, child in ipairs(node.children) do visit(child, fn) end end
    end
    local function prepare(node)
      if node.leaf then
        local name, kind = checked(node.leaf.f)
        if kind then lost(name or '?', kind); return end
        return { leaf = node.leaf, name = name }
      end
      local children = {}
      for _, child in ipairs(node.row or node.col or {}) do
        local ready = prepare(child)
        if ready then children[#children + 1] = ready end
      end
      if #children == 1 then return children[1] end
      if #children > 1 then return { kind = node.row and 'row' or 'col', children = children } end
    end
    local tabs = {}
    if not input.skip and type(input.record) == 'table' then
      for index, tab in ipairs(input.record.tabs) do
        local tree = prepare(tab.tree)
        if tree then tabs[#tabs + 1] = { tree = tree, index = index } end
      end
    end

    local function list(node, squeeze)
      local name, kind = checked(node.leaf.f)
      if kind then lost(name or node.name, kind); return false end
      local success, err = pcall(function()
        local buf = vim.fn.bufadd(name)
        if buf == 0 then error('could not list the file') end
        vim.bo[buf].buflisted = true
      end)
      if not success then lost(name, 'f', err); return false end
      if squeeze then squeezed = squeezed + 1 end
      return true
    end
    local function list_tree(node)
      visit(node, function(leaf) list(leaf, true) end)
    end
    local function open(node, win)
      if node.bad then return false end
      local name, kind = checked(node.leaf.f)
      if kind then node.bad = true; lost(name or node.name, kind); return false end
      local previous
      local success, err = pcall(function()
        previous = api.nvim_win_get_buf(win)
        api.nvim_set_current_win(win)
        if api.nvim_buf_get_name(previous) ~= name or not api.nvim_buf_is_loaded(previous) then
          api.nvim_cmd({ cmd = 'edit', args = { name }, magic = { file = false } }, {})
        end
        if api.nvim_buf_get_name(api.nvim_win_get_buf(win)) ~= name then error('the open changed its file name') end
      end)
      if not success then
        node.bad = true
        lost(name, 'f', err)
        if previous and api.nvim_buf_is_valid(previous) and api.nvim_win_is_valid(win) then
          pcall(api.nvim_win_set_buf, win, previous)
        end
        return false
      end
      if not node.opened then opened = opened + 1; node.opened = true end
      node.win = win
      return true
    end
    local function view(node)
      if node.win and api.nvim_win_is_valid(node.win) then
        pcall(api.nvim_win_call, node.win, function()
          vim.fn.winrestview({ lnum = node.leaf.lnum, col = node.leaf.col, curswant = node.leaf.want,
            topline = node.leaf.top, leftcol = node.leaf.left })
        end)
      end
    end
    local function first(node)
      while not node.leaf do node = node.children[1] end
      return node
    end
    local function last(node)
      while not node.leaf do node = node.children[#node.children] end
      return node
    end
    local function current(node)
      local chosen
      visit(node, function(leaf) if leaf.leaf.cur then chosen = leaf end end)
      return chosen or first(node)
    end
    local function measure(node, dimension, saved)
      if node.leaf then
        if saved then return node.leaf[dimension] end
        return dimension == 'w' and api.nvim_win_get_width(node.win) or api.nvim_win_get_height(node.win)
      end
      local sum, maximum = 0, 0
      for _, child in ipairs(node.children) do
        local size = measure(child, dimension, saved)
        sum, maximum = sum + size, math.max(maximum, size)
      end
      if (dimension == 'w') == (node.kind == 'row') then return sum + #node.children - 1 end
      return maximum
    end
    -- A child is sized by moving the separator after it, which nvim takes as the edge of the whole child; resizing
    -- one window inside a child that is split itself would move a boundary inside that child instead. The move runs
    -- in the window's own tab, since nvim walks the layout of the current one.
    local function set_size(node, dimension, size)
      local offset = size - measure(node, dimension, false)
      if offset == 0 then return end
      local win, moved = last(node).win, nil
      api.nvim_win_call(win, function()
        if dimension == 'w' then moved = vim.fn.win_move_separator(win, offset)
        else moved = vim.fn.win_move_statusline(win, offset) end
      end)
      if moved ~= true and moved ~= 1 then error('a split could not be sized') end
    end
    local function shares(node)
      if node.leaf then return end
      local dimension = node.kind == 'row' and 'w' or 'h'
      local available = measure(node, dimension, false) - #node.children + 1
      local total = 0
      for _, child in ipairs(node.children) do total = total + measure(child, dimension, true) end
      local used, weight = 0, 0
      for i = 1, #node.children - 1 do
        local child = node.children[i]
        weight = weight + measure(child, dimension, true)
        local target = math.floor(available * weight / total + 0.5)
        set_size(child, dimension, math.max(1, target - used))
        used = target
      end
      for _, child in ipairs(node.children) do shares(child) end
    end
    local function apply(tab)
      if not api.nvim_tabpage_is_valid(tab.id) then return end
      local success = pcall(shares, tab.tree)
      if not success then
        pcall(api.nvim_win_call, api.nvim_tabpage_get_win(tab.id), function() vim.cmd('wincmd =') end)
      end
    end

    -- Reserve sibling rectangles before descending, so a deep child cannot split only part of its parent.
    local function build(node, win)
      if node.leaf then
        if open(node, win) then return node end
        return nil
      end
      local slots, children = { win }, {}
      local dimension = node.kind == 'row' and 'w' or 'h'
      local total = 0
      for _, child in ipairs(node.children) do total = total + measure(child, dimension, true) end
      for i = 2, #node.children do
        local previous = slots[#slots]
        local success, next_win = pcall(function()
          api.nvim_set_current_win(previous)
          local extent = dimension == 'w' and api.nvim_win_get_width(previous) or api.nvim_win_get_height(previous)
          api.nvim_cmd({ cmd = node.kind == 'row' and 'vsplit' or 'split', mods = { split = 'belowright' } }, {})
          local next_id = api.nvim_get_current_win()
          local weight = measure(node.children[i - 1], dimension, true)
          local target = math.max(1, math.floor((extent - #node.children + i - 1) * weight / total + 0.5))
          if dimension == 'w' then pcall(api.nvim_win_set_width, previous, target)
          else pcall(api.nvim_win_set_height, previous, target) end
          total = total - weight
          return next_id
        end)
        if not success then
          for j = i, #node.children do list_tree(node.children[j]) end
          break
        end
        slots[#slots + 1] = next_win
      end
      for i, slot in ipairs(slots) do
        local child = build(node.children[i], slot)
        if child then children[#children + 1] = child
        elseif api.nvim_win_is_valid(slot) and #api.nvim_tabpage_list_wins(0) > 1 then
          pcall(api.nvim_win_close, slot, true)
        end
      end
      if #children == 1 then return children[1] end
      if #children > 1 then node.children = children; return node end
    end

    local selected = tabs[1]
    for _, tab in ipairs(tabs) do
      if tab.index == input.record.tab then selected = tab end
    end
    local phase_one
    if selected then
      phase_one = current(selected.tree)
      if open(phase_one, initial_win) then view(phase_one) end
    end
    local phase_buffer = api.nvim_win_get_buf(initial_win)
    local completed = false
    local resize_id
    local function notice(kind, name)
      if not name then return end
      local prefix = root:sub(-1) == '/' and root or root .. '/'
      if name:sub(1, #prefix) == prefix then name = name:sub(#prefix + 1) end
      name = vim.fn.strcharpart(name:gsub('%c', '?'), 0, 120)
      send('N ' .. token .. ' ' .. kind .. ' ' .. name)
    end
    local function finish()
      -- Redraw clamps views to the new rectangles before the baseline is captured.
      api.nvim_cmd({ cmd = 'redraw' }, {})
      api.nvim_del_autocmd(swap)
      local state = { done = token }
      package.loaded['eitri.reopen'] = state
      local success, answer = pcall(function() return assert(loadfile(dir .. '/editor_reopen_capture.lua'))(input) end)
      if not success or not answer then state.done = nil; return end
      state.restored = answer.json
      keep()
      send(('O %s %d %d %d %d %.0f'):format(token, opened, gone, failed, squeezed,
        math.max(1, math.floor((elapsed + uv.hrtime() - started) / 1000))))
      notice('g', first_gone)
      notice('f', first_failed)
    end
    local function list_buffers()
      if not input.skip and type(input.record) == 'table' then
        for _, encoded in ipairs(input.record.bufs) do
          list({ leaf = { f = encoded } }, false)
        end
      end
    end
    -- The person is at work once phase 1's window shows another buffer, that buffer changed, a window from before
    -- went, or a window of a file came. A float, or a tool's window (a buffer with a 'buftype') that the config
    -- opened after phase 1, is not the person.
    local function untouched()
      if #api.nvim_list_tabpages() ~= 1 or not api.nvim_win_is_valid(initial_win)
        or api.nvim_win_get_buf(initial_win) ~= phase_buffer or vim.bo[phase_buffer].modified then return false end
      local before, kept = {}, 0
      for _, win in ipairs(initial_windows) do before[win] = true end
      for _, win in ipairs(layout_windows()) do
        if before[win] then kept = kept + 1
        elseif vim.bo[api.nvim_win_get_buf(win)].buftype == '' then return false end
      end
      return kept == #initial_windows
    end
    local function phase_two()
      if completed then return end
      completed = true
      started = uv.hrtime()
      if resize_id then api.nvim_del_autocmd(resize_id) end
      local restored = {}
      if not untouched() then
        local kept = phase_one and api.nvim_win_is_valid(initial_win)
          and api.nvim_win_get_buf(initial_win) == phase_buffer
          and api.nvim_buf_get_name(phase_buffer) == phase_one.name
        opened = kept and 1 or 0
        for _, tab in ipairs(tabs) do
          visit(tab.tree, function(leaf)
            if leaf ~= phase_one or not kept then list(leaf, true) end
          end)
        end
      else
        opened = 0
        for _, tab in ipairs(tabs) do visit(tab.tree, function(leaf) leaf.opened = false end) end
        -- A tool's window that opened after phase 1 may have taken the focus; the first tab builds into phase 1's.
        api.nvim_set_current_win(initial_win)
        for _, tab in ipairs(tabs) do
          local success = true
          if #restored > 0 then
            success = pcall(api.nvim_cmd, { cmd = 'tabnew' }, {})
          end
          if not success then list_tree(tab.tree)
          else
            tab.id = api.nvim_get_current_tabpage()
            local empty = api.nvim_get_current_buf()
            -- Replace each tab's empty buffer before a split can leave it listed and unshown.
            open(first(tab.tree), api.nvim_get_current_win())
            local tree = build(tab.tree, api.nvim_get_current_win())
            if tree then
              tab.tree = tree
              restored[#restored + 1] = tab
              apply(tab)
              visit(tree, view)
              api.nvim_set_current_win(current(tree).win)
            end
            if api.nvim_buf_is_valid(empty) and plain(empty) and #vim.fn.win_findbuf(empty) == 0 then
              pcall(api.nvim_buf_delete, empty, { force = true })
            end
          end
        end
        if #restored > 0 then
          local target = restored[1]
          for _, tab in ipairs(restored) do if tab == selected then target = tab end end
          api.nvim_set_current_tabpage(target.id)
          api.nvim_set_current_win(current(target.tree).win)
        end
      end
      -- Entering another tab can invalidate a non-current window's viewport; restore views after those switches.
      for _, tab in ipairs(restored) do visit(tab.tree, view) end
      list_buffers()
      if api.nvim_buf_is_valid(1) and plain(1) and #vim.fn.win_findbuf(1) == 0 then
        pcall(api.nvim_buf_delete, 1, { force = true })
      end
      finish()
      if #restored > 0 then
        local ended = uv.hrtime()
        local later
        later = api.nvim_create_autocmd('VimResized', {
          group = group,
          once = true,
          nested = true,
          callback = function()
            if uv.hrtime() - ended < 2000000000 then
              for _, tab in ipairs(restored) do apply(tab) end
            end
          end,
        })
        vim.defer_fn(function() pcall(api.nvim_del_autocmd, later) end, 2000)
      end
    end
    local function run_phase_two()
      local success, err = pcall(phase_two)
      if not success then pcall(api.nvim_del_autocmd, swap); error_line(err) end
    end
    if #tabs == 0 then
      completed = true
      list_buffers()
      finish()
      return
    end
    elapsed = uv.hrtime() - started
    local generation = 0
    resize_id = api.nvim_create_autocmd('VimResized', {
      group = group,
      nested = true,
      callback = function()
        generation = generation + 1
        local pending = generation
        vim.defer_fn(function() if generation == pending then run_phase_two() end end, 100)
      end,
    })
    vim.defer_fn(function() if generation == 0 then run_phase_two() end end, 1000)
  end,
})
