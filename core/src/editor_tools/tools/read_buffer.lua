-- Reads the text of a buffer that is already loaded, as the editor holds it. Never loads, never changes a
-- buffer or a window. The answer holds whole lines only: at most LINE_MAX lines and BYTE_MAX bytes of file
-- text, read in batches so a huge buffer is not copied out to be thrown away.
local LINE_MAX = 4000
-- The CLI keeps an answer whole only up to 50,000 characters of its text, and this answer is JSON around the text,
-- so the file text stays well under that; past it the CLI puts the answer in a file and shows the model a preview.
local BYTE_MAX = 32 * 1024
local BATCH = 250

local buf = find_buf(args.path)
if not (buf and api.nvim_buf_is_loaded(buf)) then
  return { not_loaded = true }
end
local name = api.nvim_buf_get_name(buf)
if name == '' then
  return { refused = 'unnamed' }
end
local kind = vim.bo[buf].buftype
if kind ~= '' then
  return { refused = kind }
end

local count = api.nvim_buf_line_count(buf)
local first = math.max(args.start or 1, 1)
local last = math.min(args['end'] or count, count)
local format = vim.bo[buf].fileformat
local answer = {
  -- find_buf can reach another file's buffer through a link: the host judges this name before it tells the lines.
  name = name,
  lines = {},
  first = first,
  line_count = count,
  modified = vim.bo[buf].modified,
  fileformat = format,
  eol = vim.bo[buf].eol,
}

local limit = math.min(last, first + LINE_MAX - 1)
local terminator = format == 'dos' and 2 or 1
local bytes, at, full = 0, first, false
while at <= limit and not full do
  local upto = math.min(limit, at + BATCH - 1)
  for _, line in ipairs(api.nvim_buf_get_lines(buf, at - 1, upto, false)) do
    bytes = bytes + #line + terminator
    if bytes > BYTE_MAX then
      full = true
      break
    end
    answer.lines[#answer.lines + 1] = line
  end
  at = upto + 1
end

local given = first + #answer.lines - 1
if given < last then
  answer.cut = given
end
return answer
