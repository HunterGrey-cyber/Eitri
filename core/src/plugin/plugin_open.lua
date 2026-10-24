-- Opens a file of a plugin's work tree in the editor at a line and column, and says what it did. Every
-- value is an argument: the path reaches nvim as a structured :edit or :split argument with file magic
-- off, never as Ex text. Runs on LuaJIT and PUC 5.1.
local opts = ...
local path = opts.path
-- An argument starting with + or ++ is read by :edit as a command or an option.
if type(path) ~= 'string' or path:sub(1, 1) ~= '/' then
  error('plugin_open: the path is not absolute')
end

local function realpath(name)
  local uv = vim.uv or vim.loop
  return uv and uv.fs_realpath(name)
end

if realpath(path) ~= path then
  error('plugin_open: the path no longer resolves to itself')
end
-- Each buffer before the edit, by its name as well: an empty unnamed buffer is reused by :edit under the new name.
local existing = {}
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  existing[buf] = { loaded = vim.api.nvim_buf_is_loaded(buf), name = vim.api.nvim_buf_get_name(buf) }
end

-- The byte offset of the first `chars` Unicode scalar values of `text`, stopping at its end.
local function byte_col(text, chars)
  local i, n = 1, 0
  while n < chars and i <= #text do
    local b = text:byte(i)
    if b >= 0xF0 then
      i = i + 4
    elseif b >= 0xE0 then
      i = i + 3
    elseif b >= 0xC0 then
      i = i + 2
    else
      i = i + 1
    end
    n = n + 1
  end
  return math.min(i - 1, #text)
end

local how
local previous_win = vim.api.nvim_get_current_win()
local current = vim.api.nvim_get_current_buf()
local current_name = vim.api.nvim_buf_get_name(current)
if current_name ~= '' and realpath(current_name) == realpath(path) then
  how = 'current'
else
  -- Over a modified buffer a plain :edit stops with "No write since last change"; a split keeps it.
  if vim.bo[current].modified then
    how = 'split'
  else
    how = 'edit'
  end
  vim.api.nvim_cmd({ cmd = how, args = { path }, magic = { file = false } }, {})
end

local win = vim.api.nvim_get_current_win()
local buf = vim.api.nvim_win_get_buf(win)
if realpath(vim.api.nvim_buf_get_name(buf)) ~= path then
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
  error('plugin_open: the opened file no longer resolves to the checked path')
end
local count = vim.api.nvim_buf_line_count(buf)
local line = tonumber(opts.line) or 1
if line < 1 then
  line = 1
end
if line > count then
  line = count
end
local text = vim.api.nvim_buf_get_lines(buf, line - 1, line, false)[1] or ''
local col = tonumber(opts.col) or 1
if col < 1 then
  col = 1
end
local byte = byte_col(text, col - 1)
vim.api.nvim_win_set_cursor(win, { line, byte })
return { how = how, line = line, col = byte }
