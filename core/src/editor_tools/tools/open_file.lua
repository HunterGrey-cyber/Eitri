-- Shows `args.path` at `args.line` in a window that is not the person's current one. Only `nvim_*` calls and
-- `winrestview` inside `nvim_win_call`: no keys, no `:normal`, no `:edit`, no change of the current window,
-- the mode or the cursor, and a buffer that is already loaded is never read again. Putting a buffer in a window
-- runs the person's hooks with no load at all, so that happens under `:silent` too: see `load_file`.
local MARK = 'eitri_agent_view'
local ns = api.nvim_create_namespace('eitri_agent_view')

local buf = find_buf(args.path)
if not (buf and api.nvim_buf_is_loaded(buf)) then
  if not args.load then
    return { needs_load = true }
  end
  local loaded, why = load_file(args.path)
  if why == 'swap' then
    return { swap = true }
  end
  if not loaded then
    return { refused = 'the editor could not load the file' }
  end
  buf = loaded
  -- A load can outlast the budget; the caller was told by then that nothing was shown.
  local sec, usec = vim.uv.gettimeofday()
  if sec * 1000 + usec / 1000 > __stamp.deadline_ms then
    return { expired = true }
  end
end

local function is_float(win)
  return api.nvim_win_get_config(win).relative ~= ''
end

local function marked(win)
  local ok, value = pcall(api.nvim_win_get_var, win, MARK)
  return ok and value == true
end

local function clear_view(view)
  if view.cleared then
    return
  end
  view.cleared = true
  pcall(api.nvim_buf_del_extmark, view.buf, ns, view.mark)
  pcall(api.nvim_del_autocmd, view.autocmd)
  pcall(function()
    if not view.timer:is_closing() then
      view.timer:stop()
      view.timer:close()
    end
  end)
end

local function show()
  local cur = api.nvim_get_current_win()
  local wins = api.nvim_tabpage_list_wins(api.nvim_get_current_tabpage())
  local count = api.nvim_buf_line_count(buf)
  local line = args.line and math.min(math.max(args.line, 1), count) or nil

  local win, where
  for _, w in ipairs(wins) do
    if w ~= cur and not is_float(w) and api.nvim_win_get_buf(w) == buf then
      win, where = w, 'existing'
      break
    end
  end
  if not win then
    for _, w in ipairs(wins) do
      if w ~= cur and not is_float(w) and marked(w) then
        local old = api.nvim_win_get_buf(w)
        local keeps_changes = vim.bo[old].modified and #vim.fn.win_findbuf(old) == 1
        if not keeps_changes and pcall(api.nvim_win_set_buf, w, buf) then
          win, where = w, 'agent_window'
        end
        break
      end
    end
  end
  if not win then
    local anchor = cur
    if is_float(anchor) then
      anchor = nil
      for _, w in ipairs(wins) do
        if not is_float(w) then
          anchor = w
          break
        end
      end
    end
    if not anchor then
      error('no window to split')
    end
    local side = api.nvim_win_get_width(anchor) >= 160 and 'right' or 'below'
    win = api.nvim_open_win(buf, false, { split = side, win = anchor })
    where = 'new_split'
    for _, w in ipairs(wins) do
      if marked(w) then
        pcall(api.nvim_win_del_var, w, MARK)
      end
    end
    api.nvim_win_set_var(win, MARK, true)
  end

  if line then
    api.nvim_win_call(win, function()
      local height = api.nvim_win_get_height(win)
      local top = math.min(line - math.floor(height / 2), count - height + 1)
      vim.fn.winrestview({ lnum = line, topline = math.max(1, top), col = 0, leftcol = 0 })
    end)
  end
  local shown_line = line or api.nvim_win_get_cursor(win)[1]

  if line then
    local last = math.min(math.max(args.end_line or line, line), count)
    local previous = __state.view
    if previous then
      clear_view(previous)
    end
    local view = { buf = buf }
    view.mark = api.nvim_buf_set_extmark(buf, ns, line - 1, 0, {
      end_row = last,
      end_col = 0,
      hl_group = 'Visual',
      hl_eol = true,
      strict = false,
    })
    view.autocmd = api.nvim_create_autocmd('WinEnter', {
      group = api.nvim_create_augroup(MARK, { clear = false }),
      callback = function()
        if api.nvim_get_current_win() == win or not api.nvim_win_is_valid(win) then
          clear_view(view)
          return true
        end
      end,
    })
    view.timer = vim.defer_fn(function()
      clear_view(view)
    end, 4000)
    __state.view = view
  end
  return { shown = true, where = where, line = shown_line }
end

local ok, result
silently(function()
  ok, result = pcall(show)
end)
if not ok then
  return { refused = 'the editor could not show the file: ' .. tostring(result):sub(1, 200) }
end
return result
