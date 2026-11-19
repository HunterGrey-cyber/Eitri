-- Opens a file of a plugin's work tree (or a link a document's project holds) in the editor at a line and column,
-- and says what it did. Follows open_checked.lua, which the chunk starts with: the checks and the edit are that
-- file's. A plugin's file exists when its host checks it, so one gone since is refused, not opened empty.
local opts = ...
local path = opts.path
local is_path = check_path(path, 'plugin_open', false)
-- From the diff tab this plugin made, the file opens in the tab the diff came from; only after the checks, so a
-- refused open leaves the person where they were.
local tab = vim.api.nvim_get_current_tabpage()
local moved = false
local owned, owner = pcall(vim.api.nvim_tabpage_get_var, tab, 'eitri_plugin_diff')
if opts.owner and owned and owner == opts.owner then
  local ok, from = pcall(vim.api.nvim_tabpage_get_var, tab, 'eitri_plugin_diff_from')
  if ok and type(from) == 'number' and vim.api.nvim_tabpage_is_valid(from) then
    moved = true
    -- A handler failing on the way in still leaves the person where they were.
    local entered, why = pcall(vim.api.nvim_set_current_tabpage, from)
    if not entered then
      pcall(vim.api.nvim_set_current_tabpage, tab)
      error(why, 0)
    end
  end
end

-- The length in bytes of the character at byte `i` of `s`, counted as the page counts it: the page gets the text
-- with each maximal ill-formed UTF-8 subsequence replaced by one U+FFFD, so such a subsequence is one character.
local function char_len(s, i)
  local b = s:byte(i)
  local need, lo, hi
  if b < 0x80 then
    return 1
  elseif b >= 0xC2 and b <= 0xDF then
    need, lo, hi = 1, 0x80, 0xBF
  elseif b == 0xE0 then
    need, lo, hi = 2, 0xA0, 0xBF
  elseif b == 0xED then
    need, lo, hi = 2, 0x80, 0x9F
  elseif b >= 0xE1 and b <= 0xEF then
    need, lo, hi = 2, 0x80, 0xBF
  elseif b == 0xF0 then
    need, lo, hi = 3, 0x90, 0xBF
  elseif b >= 0xF1 and b <= 0xF3 then
    need, lo, hi = 3, 0x80, 0xBF
  elseif b == 0xF4 then
    need, lo, hi = 3, 0x80, 0x8F
  else
    return 1
  end
  for j = 1, need do
    local c = s:byte(i + j)
    if c == nil or c < lo or c > hi then
      return j
    end
    lo, hi = 0x80, 0xBF
  end
  return need + 1
end

-- The byte offset of the first `chars` characters of `s`, stopping at its end.
local function byte_col(s, chars)
  local i, n = 1, 0
  while n < chars and i <= #s do
    i = i + char_len(s, i)
    n = n + 1
  end
  return math.min(i - 1, #s)
end

local opened, how, win, buf = pcall(open_checked, path, 'plugin_open', is_path)
if not opened then
  -- A refused or failed edit leaves the person in the tab they asked from.
  if moved and vim.api.nvim_tabpage_is_valid(tab) then
    pcall(vim.api.nvim_set_current_tabpage, tab)
  end
  error(how, 0)
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
return { how = how, line = line, col = byte, path = vim.api.nvim_buf_get_name(buf), filetype = vim.bo[buf].filetype }
