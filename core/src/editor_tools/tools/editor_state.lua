-- What the editor holds, in one pass that changes nothing: the listed, named file buffers under the root,
-- a count of the others, where the person is, and the raw mode. Rust judges every path before it is told.
local BUFFER_MAX = 500

local root = args.root
if root:sub(-1) ~= '/' then
  root = root .. '/'
end

local shown = {}
for _, win in ipairs(api.nvim_list_wins()) do
  shown[api.nvim_win_get_buf(win)] = true
end

local buffers, other, left_out = {}, 0, 0
for _, buf in ipairs(api.nvim_list_bufs()) do
  local name = api.nvim_buf_get_name(buf)
  -- A terminal, help, quickfix or prompt buffer is not a file the person has open; it is not counted either.
  if vim.bo[buf].buflisted and name ~= '' and vim.bo[buf].buftype == '' then
    if name:sub(1, #root) ~= root then
      other = other + 1
    elseif #buffers >= BUFFER_MAX then
      left_out = left_out + 1
    else
      buffers[#buffers + 1] = {
        path = name,
        modified = vim.bo[buf].modified,
        lines = api.nvim_buf_is_loaded(buf) and api.nvim_buf_line_count(buf) or 0,
        filetype = vim.bo[buf].filetype,
        shown = shown[buf] == true,
      }
    end
  end
end

local here = api.nvim_get_current_buf()
local current = { line = api.nvim_win_get_cursor(0)[1] }
local here_name = api.nvim_buf_get_name(here)
if here_name ~= '' and vim.bo[here].buftype == '' then
  current.path = here_name
end

return {
  buffers = buffers,
  other = other,
  left_out = left_out,
  current = current,
  mode = api.nvim_get_mode().mode,
}
