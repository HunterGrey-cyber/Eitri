-- What the user's nvim holds of one file: whether a loaded buffer shows it, whether any such
-- buffer has unsaved changes, and optionally a range of the first one's lines.
--
-- Arguments: the file's absolute path, and nil or {first line (from 1), number of lines}. The
-- path is only ever an argument here: it is compared, never opened, and never becomes Ex or Lua
-- source. A buffer shows the file when it names the same (device, inode), which covers a hard
-- link and a name reached through a symlink; a file that does not exist (one about to be
-- restored) is matched by its resolved name.
local abs_path, range = ...
if range == vim.NIL then
  range = nil
end

local wanted = vim.uv.fs_stat(abs_path)
local wanted_name = vim.fn.resolve(abs_path)

local function shows_the_file(name)
  local st = vim.uv.fs_stat(name)
  if wanted and st then
    return st.dev == wanted.dev and st.ino == wanted.ino
  end
  return vim.fn.resolve(vim.fn.fnamemodify(name, ':p')) == wanted_name
end

local first, modified = nil, false
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) then
    local name = vim.api.nvim_buf_get_name(buf)
    if name ~= '' and shows_the_file(name) then
      first = first or buf
      if vim.bo[buf].modified then
        modified = true
      end
    end
  end
end

if not first then
  return { found = false, modified = false }
end

local line_count = vim.api.nvim_buf_line_count(first)
local answer = {
  found = true,
  modified = modified,
  fileformat = vim.bo[first].fileformat,
  eol = vim.bo[first].eol,
  line_count = line_count,
}
if range then
  local start, count = range[1], range[2]
  if type(start) == 'number' and type(count) == 'number' and start >= 1 and count >= 0
    and start - 1 + count <= line_count then
    answer.lines = vim.api.nvim_buf_get_lines(first, start - 1, start - 1 + count, false)
  end
end
return answer
