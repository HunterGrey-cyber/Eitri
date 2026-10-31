-- What the editor holds, in one pass that changes nothing: the listed, named file buffers under the root,
-- a count of the others, where the person is, and the raw mode. Rust judges every path before it is told.
local BUFFER_MAX = 500
-- The most of a live selection the answer carries, past the feed's own caps on what it copies.
local SELECTED_LINES = 400
local SELECTED_BYTES = 256 * 1024

local root = args.root
if root:sub(-1) ~= '/' then
  root = root .. '/'
end

local shown = {}
for _, win in ipairs(api.nvim_list_wins()) do
  shown[api.nvim_win_get_buf(win)] = true
end

-- A buffer not loaded holds no text, so its name is all there is to judge.
local function held_from(buf, name)
  if api.nvim_buf_is_loaded(buf) then
    return read_from(buf)
  end
  return name
end

-- The lines under the live selection as the buffer holds them now, in the same pass as its record. The feed's copy of a
-- selection can outlast them: the reload timer runs while the person stays in Visual mode, and nothing moves to tell it.
local function selected()
  local mode = vim.fn.mode()
  if mode ~= 'v' and mode ~= 'V' and mode ~= '\22' then
    return nil
  end
  local from, to = vim.fn.line('v'), vim.fn.line('.')
  local first, last = math.min(from, to), math.max(from, to)
  local lines, room = {}, SELECTED_BYTES
  for _, line in ipairs(api.nvim_buf_get_lines(0, first - 1, math.min(last, first + SELECTED_LINES - 1), false)) do
    if room <= 0 then
      break
    end
    lines[#lines + 1] = line:sub(1, room)
    room = room - #line
  end
  return { start_line = first, end_line = last, lines = lines }
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
        read_from = held_from(buf, name),
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
  current.read_from = read_from(here)
  current.selected = selected()
end

return {
  buffers = buffers,
  other = other,
  left_out = left_out,
  current = current,
  mode = api.nvim_get_mode().mode,
}
