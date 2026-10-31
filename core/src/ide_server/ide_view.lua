-- The proposal view in nvim: the Lua the constant chunks of view.rs run. It runs on a request nobody pressed a key for,
-- so only API calls: no keys, no `:normal`, no change of the current window, the mode or the cursor. A window is opened
-- with `enter = false`, and `diffthis` goes through `nvim_win_call`. Everything after the open runs on what the person
-- does in the view, through the buffer-local keys, commands and autocommands `hook` sets.
local MARK = 'eitri_ide_view'
local AGENT_MARK = 'eitri_agent_view'
-- As `open_file` decides: a window this wide has room beside it.
local WIDE = 160
-- One comparison of two whole texts for a burst of typing, not one for each key.
local SAME_DELAY_MS = 200
local SAME_AS_FILE = '   same as the file: accepting now rejects'
-- The accept key's own text, in a window with no room for those words and the key both.
local REJECTS_WHEN_SAME = 'a rejects: same as file'
local LABELS = { ': on disk', ": claude's proposal, editable" }
local SHORT_LABELS = { ': on disk', ': proposal' }
-- Columns a path keeps however narrow its window: the end of a file's name.
local PATH_FLOOR = 8
-- An event's size as the take counts it. An accept goes to the host in one answer whatever its size, and an answer past
-- the link's frame limit ends the link with the accept lost, so this keeps the largest accept well under that limit.
local ACCEPT_LIMIT = 8 * 1024 * 1024
local LINE_HEADER = 5

local ide = rawget(_G, '__eitri_ide')
if not ide then
  ide = { views = {}, events = {} }
  _G.__eitri_ide = ide
end

-- A window that outlives its view: the person's now, with nothing of the view left on it. 'diff' needs nothing here:
-- `diffthis` sets it for the window's current buffer alone, so it ends when the window shows another, and a diff the
-- window has by then is the person's own.
local function release(win)
  pcall(api.nvim_win_del_var, win, MARK)
  pcall(api.nvim_set_option_value, 'winbar', '', { win = win })
end

local function close_window(win, buf)
  if not api.nvim_win_is_valid(win) then
    return
  end
  if api.nvim_win_get_buf(win) ~= buf then
    release(win)
  elseif not pcall(api.nvim_win_close, win, true) then
    -- The last window of all cannot close. Its buffer goes all the same, unsaved edits included: a view's proposal is
    -- nothing to keep once its view has ended.
    release(win)
    pcall(function() vim.bo[buf].modified = false end)
    pcall(api.nvim_win_set_buf, win, api.nvim_create_buf(true, false))
  end
end

local function remove(windows, buffers)
  for _, shown in ipairs(windows) do
    close_window(shown.win, shown.buf)
  end
  -- 'bufhidden' has wiped them, unless a further window shows one.
  for _, buf in ipairs(buffers) do
    if api.nvim_buf_is_valid(buf) then
      pcall(api.nvim_buf_delete, buf, { force = true })
    end
  end
end

-- Goes before the windows do, so closing them here is not mistaken for the person closing the view.
local function unhook(view)
  if view.group then
    pcall(api.nvim_del_augroup_by_id, view.group)
  end
  if view.timer and not view.timer:is_closing() then
    view.timer:stop()
    view.timer:close()
  end
end

-- Nothing closes under the command-line window, and only the person can end that. `CmdwinLeave` runs before it is gone.
local function remove_when_free(view)
  if vim.fn.getcmdwintype() == '' then
    remove(view.windows, view.buffers)
  else
    api.nvim_create_autocmd('CmdwinLeave', {
      once = true,
      callback = vim.schedule_wrap(function() remove_when_free(view) end),
    })
  end
end

function ide.close(id)
  local view = ide.views[id]
  if not view then
    return
  end
  view.done = true
  ide.views[id] = nil
  unhook(view)
  remove_when_free(view)
end

-- The first answer is the view's only one. The windows go once nvim is back in its loop: a window cannot close inside
-- `BufWriteCmd`, and a `WinClosed` runs in the middle of another close.
local function answer(view, event)
  view.done = true
  ide.events[#ide.events + 1] = event
  vim.schedule(function() ide.close(view.id) end)
end

local function accept(view)
  if view.done then
    return
  end
  local proposal = view.windows[2].buf
  local lines = api.nvim_buf_get_lines(proposal, 0, -1, false)
  local size = 0
  for _, line in ipairs(lines) do
    size = size + #line + LINE_HEADER
  end
  if size > ACCEPT_LIMIT then
    api.nvim_echo({ { 'a proposal over 8 MiB cannot be answered: cut it down first', 'WarningMsg' } }, true, {})
    return
  end
  vim.bo[proposal].modified = false
  answer(view, { view = view.id, kind = 'accept', lines = lines })
end

local function reject(view)
  if not view.done then
    answer(view, { view = view.id, kind = 'reject' })
  end
end

-- No answer: the terminal prompt that opened the view still decides.
local function closed(view)
  if not view.done then
    answer(view, { view = view.id, kind = 'closed' })
  end
end

-- The proposal as the host joins an accepted one: its own terminator, a final one only if it came with one, and so
-- nothing for the one empty line an empty text is held as.
local function proposal_text(view)
  local lines = api.nvim_buf_get_lines(view.windows[2].buf, 0, -1, false)
  local terminator = view.crlf and '\r\n' or '\n'
  local text = table.concat(lines, terminator)
  return view.eol and text .. terminator or text
end

-- 'winbar' is evaluated like 'statusline': a `%` left single in a file's name would run when the view is drawn.
local function bar_text(text)
  return (text:gsub('%%', '%%%%'))
end

-- 'winbar' cuts from the left, and the path and the accept key go first. So a bar too long for its window is built for
-- that width: a shorter label, the path cut from the left to what the rest leaves, since its end is the file's name,
-- and `%<` at the end, so what is still over goes from the right. While the proposal equals the file the warning takes
-- the accept key's place: a key that says "accepts" there would be wrong.
local function draw_bar(view, index)
  local win = view.windows[index].win
  local width = api.nvim_win_get_width(win)
  local same = index == 2 and view.same
  local rel, leader = view.rel, view.leader
  local accepts = '   ' .. leader .. 'a accepts'
  local others = ('   %sd rejects and stops claude\'s turn   %sq closes'):format(leader, leader)
  local text = rel .. LABELS[index] .. accepts .. others .. (same and SAME_AS_FILE or '')
  local bar = bar_text(text)
  if vim.fn.strdisplaywidth(text) > width then
    local head = same and '   ' .. leader .. REJECTS_WHEN_SAME or SHORT_LABELS[index] .. accepts
    local path = math.max(PATH_FLOOR, width - vim.fn.strdisplaywidth(head) - 1)
    bar = ('%%.%d(%s%%)%s%%<'):format(path, bar_text(rel), bar_text(head .. others))
  end
  api.nvim_set_option_value('winbar', bar, { win = win })
end

-- The CLI reads an accepted text equal to the file as a rejection, and stops its turn.
local function show_sameness(view)
  if view.done then
    return
  end
  local same = proposal_text(view) == view.file_text
  if same ~= view.same then
    view.same = same
    draw_bar(view, 2)
  end
end

-- `BufWriteCmd` fires between a change and `TextChanged` when both are in one command line, mapping or macro, as in
-- `:s/a/b/ | wa`. So only a buffer that is already nomodified is a save the person meant: for that one nvim writes on
-- nothing but an explicit `:w`. The flag is cleared and such a `:w` has to be repeated, which fails toward no approval.
-- `:w other` and `:saveas other` fire this too, under the other name: a copy is no approval, and none is written.
local function saved(view, proposal, own_name, written_as)
  if vim.bo[proposal].modified then
    vim.bo[proposal].modified = false
  elseif written_as == own_name then
    accept(view)
  else
    api.nvim_echo({ { 'a proposal is written nowhere: :w alone accepts it', 'WarningMsg' } }, false, {})
  end
end

-- With no undo levels the text is no change a person can undo into an empty buffer.
local function set_text(buf, lines)
  local levels = vim.go.undolevels
  vim.bo[buf].undolevels = -1
  api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].undolevels = levels
  vim.bo[buf].modified = false
end

local function hook(view)
  local proposal = view.windows[2].buf
  local own_name = api.nvim_buf_get_name(proposal)
  local group = api.nvim_create_augroup('eitri_ide_view_' .. view.id, { clear = true })
  view.group = group
  view.timer = vim.uv.new_timer()
  local check = vim.schedule_wrap(function() show_sameness(view) end)
  local on_close = function() closed(view) end
  local settle = function()
    if not view.done then
      vim.bo[proposal].modified = false
      view.timer:start(SAME_DELAY_MS, 0, check)
    end
  end

  for _, shown in ipairs(view.windows) do
    local buf = shown.buf
    api.nvim_create_autocmd('WinClosed', { group = group, pattern = tostring(shown.win), callback = on_close })
    api.nvim_create_autocmd('BufWipeout', { group = group, buffer = buf, callback = on_close })
    local keys = { buffer = buf, nowait = true, silent = true }
    vim.keymap.set('n', '<localleader>a', function() accept(view) end, keys)
    vim.keymap.set('n', '<localleader>d', function() reject(view) end, keys)
    vim.keymap.set('n', '<localleader>q', on_close, keys)
    api.nvim_buf_create_user_command(buf, 'EitriDiffAccept', function() accept(view) end, {})
    api.nvim_buf_create_user_command(buf, 'EitriDiffReject', function() reject(view) end, {})
  end

  api.nvim_create_autocmd('BufWriteCmd', {
    group = group,
    buffer = proposal,
    callback = function(event) saved(view, proposal, own_name, event.match) end,
  })
  -- A modified `acwrite` buffer is written by `:wa`, `:wqa`, an autosave and "Yes" at a quit prompt, each of which would
  -- be an approval nobody meant, and `:q` would refuse with E37.
  api.nvim_create_autocmd({ 'TextChanged', 'TextChangedI', 'TextChangedP' }, {
    group = group,
    buffer = proposal,
    callback = settle,
  })
  -- A change made from elsewhere runs none of those for this buffer: `dp` in the file's window, or a plugin's call. The
  -- flag goes once nvim is back in its loop, never inside the change: until the command line that made it is over,
  -- `saved` has to read it.
  local settling = false
  local function watch()
    api.nvim_buf_attach(proposal, false, {
      on_lines = function()
        if view.done then
          return true
        end
        if not settling then
          settling = true
          vim.schedule(function()
            settling = false
            settle()
          end)
        end
      end,
    })
  end
  watch()
  -- Kept nomodified, the buffer does not stop `:e` on E37, and nothing can be read under its name: the text would go, and
  -- an accept would send an empty file. Unloading comes first, so the text is held there and put back, with what
  -- unloading ends: the syntax, the place in the diff and the watch above.
  local held
  api.nvim_create_autocmd('BufUnload', {
    group = group,
    buffer = proposal,
    callback = function() held = api.nvim_buf_get_lines(proposal, 0, -1, false) end,
  })
  api.nvim_create_autocmd('BufReadCmd', {
    group = group,
    buffer = proposal,
    callback = function()
      if held then
        set_text(proposal, held)
        held = nil
        vim.bo[proposal].syntax = vim.bo[proposal].syntax
        pcall(api.nvim_win_call, view.windows[2].win, function() vim.cmd('diffthis') end)
        watch()
      end
      api.nvim_echo({ { 'a proposal has no file to read: it stays as it is', 'WarningMsg' } }, false, {})
    end,
  })
  api.nvim_create_autocmd('WinResized', {
    group = group,
    callback = function()
      if not view.done then
        draw_bar(view, 1)
        draw_bar(view, 2)
      end
    end,
  })
  show_sameness(view)
end

local function is_float(win)
  return api.nvim_win_get_config(win).relative ~= ''
end

local function marked(win)
  local ok, value = pcall(api.nvim_win_get_var, win, AGENT_MARK)
  return ok and value == true
end

-- The pair takes the agent window's place, unless the person is in it. A window that is the only one on unsaved text
-- stays: with 'hidden' on, closing it would hide that text, and with 'confirm' on it would stop on a prompt.
local function close_agent_window(wins, current)
  for _, win in ipairs(wins) do
    if win ~= current and marked(win) then
      local buf = api.nvim_win_get_buf(win)
      local keeps_changes = vim.bo[buf].modified and #vim.fn.win_findbuf(buf) == 1
      if not keeps_changes then
        pcall(api.nvim_win_close, win, false)
      end
      return
    end
  end
end

-- A window that can be split: a floating one cannot.
local function split_from(current)
  if not is_float(current) then
    return current
  end
  for _, win in ipairs(api.nvim_tabpage_list_wins(api.nvim_get_current_tabpage())) do
    if not is_float(win) then
      return win
    end
  end
  error('no window to split')
end

-- 'modeline' and 'swapfile' go off before any text is in, and the name is one nvim leaves as it is.
local function filled_buffer(buffers, name, lines, buftype)
  local buf = api.nvim_create_buf(false, false)
  buffers[#buffers + 1] = buf
  vim.bo[buf].modeline = false
  vim.bo[buf].swapfile = false
  vim.bo[buf].buftype = buftype
  vim.bo[buf].bufhidden = 'wipe'
  api.nvim_buf_set_name(buf, name)
  set_text(buf, lines)
  return buf
end

local function leader_text()
  local leader = vim.g.maplocalleader
  if type(leader) ~= 'string' or leader == '' then
    leader = '\\'
  end
  return vim.fn.keytrans(leader)
end

local function build(view)
  local file = filled_buffer(view.buffers, args.file_name, args.file_lines, 'nofile')
  vim.bo[file].readonly = true
  vim.bo[file].modifiable = false
  local proposal = filled_buffer(view.buffers, args.proposal_name, args.proposal_lines, 'acwrite')

  -- 'syntax' fires no `FileType`: no ftplugin of the person's runs for a request nobody pressed a key for.
  local known, filetype = pcall(vim.filetype.match, { filename = args.real })
  if known and type(filetype) == 'string' and filetype:match('^[%w_.+-]+$') then
    vim.bo[file].syntax = filetype
    vim.bo[proposal].syntax = filetype
  end

  local anchor = split_from(api.nvim_get_current_win())
  local side = api.nvim_win_get_width(anchor) >= WIDE and 'right' or 'below'
  local file_win = api.nvim_open_win(file, false, { split = side, win = anchor })
  view.windows[1] = { win = file_win, buf = file }
  local proposal_win = api.nvim_open_win(proposal, false, { split = 'right', win = file_win })
  view.windows[2] = { win = proposal_win, buf = proposal }

  for _, shown in ipairs(view.windows) do
    api.nvim_win_set_var(shown.win, MARK, args.view)
    api.nvim_win_call(shown.win, function() vim.cmd('diffthis') end)
  end

  draw_bar(view, 1)
  hook(view)
end

-- Diff mode is per tabpage: `diffthis` would join every window here that has 'diff' set, the person's own diff or
-- another connection's view among them.
local wins = api.nvim_tabpage_list_wins(api.nvim_get_current_tabpage())
for _, win in ipairs(wins) do
  if api.nvim_get_option_value('diff', { win = win }) then
    return { refused = 'a diff is already open in this tabpage' }
  end
end

close_agent_window(wins, api.nvim_get_current_win())

local view = {
  id = args.view,
  windows = {},
  buffers = {},
  rel = args.rel,
  leader = leader_text(),
  file_text = args.file_text,
  crlf = args.crlf,
  eol = args.eol,
}
local built, why = pcall(build, view)
if not built then
  unhook(view)
  remove(view.windows, view.buffers)
  return { refused = tostring(why) }
end
ide.views[args.view] = view
return { opened = true }
