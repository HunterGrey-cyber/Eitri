-- Opens a file another process asked this window to open, at a line and column when given, and says what it did.
-- Every value is an argument: the path reaches nvim as a structured :edit or :split argument with file magic off,
-- never as Ex text. Runs on LuaJIT and PUC 5.1.
local opts = ...
local path = opts.path
-- An argument starting with + or ++ is read by :edit as a command or an option.
if type(path) ~= 'string' or path:sub(1, 1) ~= '/' then
  error('editor_open: the path is not absolute')
end

local uv = vim.uv or vim.loop

-- Whether `name` is the file `path` names. A name with nothing behind it is a file not written yet: it counts only
-- as `path` itself, directly under a parent that resolves to itself. A dangling link has an entry, so it does not.
local function is_path(name)
  local real = uv.fs_realpath(name)
  if real then
    return real == path
  end
  if name ~= path or uv.fs_lstat(name) then
    return false
  end
  local parent, base = name:match('^(.*)/([^/]+)$')
  if not parent or base == '.' or base == '..' then
    return false
  end
  parent = parent == '' and '/' or parent
  return uv.fs_realpath(parent) == parent
end

if not is_path(path) then
  error('editor_open: the path no longer resolves to itself')
end
-- What was checked before this request waited for the editor can have changed; nvim blocks reading a FIFO.
local stat = uv.fs_stat(path)
if stat and stat.type ~= 'file' then
  error('editor_open: the path is not a regular file')
end

-- Each buffer before the edit, by its name as well: an empty unnamed buffer is reused by :edit under the new name.
local existing = {}
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  existing[buf] = { loaded = vim.api.nvim_buf_is_loaded(buf), name = vim.api.nvim_buf_get_name(buf) }
end

local how
local previous_win = vim.api.nvim_get_current_win()
local current = vim.api.nvim_get_current_buf()
local current_name = vim.api.nvim_buf_get_name(current)
local special = vim.bo[current].buftype ~= ''
if not special and current_name ~= '' and is_path(current_name) then
  how = 'current'
else
  -- Over a modified buffer a plain :edit stops with "No write since last change", and a terminal or a plugin's
  -- buffer is not the user's to replace; a split keeps either.
  if special or vim.bo[current].modified then
    how = 'split'
  else
    how = 'edit'
  end
  vim.api.nvim_cmd({ cmd = how, args = { path }, magic = { file = false } }, {})
end

local win = vim.api.nvim_get_current_win()
local buf = vim.api.nvim_win_get_buf(win)
if not is_path(vim.api.nvim_buf_get_name(buf)) then
  local before = existing[buf]
  -- A buffer that did not exist, or that :edit took over and renamed, holds only what was just read.
  local fresh = before == nil or before.name ~= vim.api.nvim_buf_get_name(buf)
  -- The window goes back to its buffer, or to a new empty one when that buffer was taken over or wiped on
  -- leaving it; a failure here must not keep the buffer below from being removed.
  pcall(function()
    if how == 'edit' then
      if buf ~= current and vim.api.nvim_buf_is_valid(current) then
        vim.api.nvim_win_set_buf(previous_win, current)
      else
        vim.api.nvim_win_set_buf(previous_win, vim.api.nvim_create_buf(true, false))
      end
    elseif how == 'split' then
      vim.api.nvim_win_close(win, true)
    end
  end)
  if fresh then
    vim.api.nvim_buf_delete(buf, { force = true })
  elseif not before.loaded then
    vim.api.nvim_buf_delete(buf, { unload = true, force = true })
  end
  error('editor_open: the opened file no longer resolves to the checked path')
end
-- An explicitly opened preload belongs to the user, even if it is hidden again later.
vim.b[buf].eitri_plugin_preload = nil

local cursor = vim.api.nvim_win_get_cursor(win)
local line, byte = cursor[1], cursor[2]
if opts.line ~= nil or opts.col ~= nil then
  local count = vim.api.nvim_buf_line_count(buf)
  line = math.max(1, math.min(tonumber(opts.line) or line, count))
  local text = vim.api.nvim_buf_get_lines(buf, line - 1, line, false)[1] or ''
  byte = math.min(math.max(tonumber(opts.col) or 1, 1) - 1, math.max(#text - 1, 0))
  -- A byte inside a character is that character, as the cursor can stand only on its first byte.
  while byte > 0 and text:byte(byte + 1) >= 0x80 and text:byte(byte + 1) < 0xC0 do
    byte = byte - 1
  end
  vim.api.nvim_win_set_cursor(win, { line, byte })
end
return { how = how, line = line, col = byte }
