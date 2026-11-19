-- Opens a file another process asked this window to open, at a line and column when given, and says what it did.
-- Follows open_checked.lua, which the chunk starts with: the checks and the edit are that file's.
local opts = ...
local path = opts.path
local is_path = check_path(path, 'editor_open', true)
local how, win, buf = open_checked(path, 'editor_open', is_path)

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
