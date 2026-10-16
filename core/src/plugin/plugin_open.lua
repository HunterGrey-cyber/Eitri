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
  return (uv and uv.fs_realpath(name)) or name
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
