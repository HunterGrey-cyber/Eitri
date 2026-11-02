-- The open file buffers under the root as the editor holds them now, text included: unsaved changes are in it. The
-- current buffer first, then the rest by when they were last used, so a bound cuts the oldest. Loads nothing and
-- changes nothing. Whole lines only. Rust judges every name and record before a line is told.
local LISTED_MAX = args.listed_max
local LINES_MAX = args.lines_max
local BYTES_MAX = args.bytes_max
local TOTAL_MAX = args.total_max
local BATCH = 250

local root = args.root
if root:sub(-1) ~= '/' then
  root = root .. '/'
end

local here = api.nvim_get_current_buf()
local shown = {}
for _, win in ipairs(api.nvim_list_wins()) do
  shown[api.nvim_win_get_buf(win)] = true
end

local infos, other = {}, 0
for _, info in ipairs(vim.fn.getbufinfo({ buflisted = 1 })) do
  local name = info.name
  -- A terminal, help, quickfix or prompt buffer is not a file the person has open.
  if name ~= '' and vim.bo[info.bufnr].buftype == '' then
    if name:sub(1, #root) == root then
      infos[#infos + 1] = info
    else
      other = other + 1
    end
  end
end
table.sort(infos, function(a, b)
  if (a.bufnr == here) ~= (b.bufnr == here) then
    return a.bufnr == here
  end
  if a.lastused ~= b.lastused then
    return a.lastused > b.lastused
  end
  return a.bufnr < b.bufnr
end)

local buffers, left_out, total = {}, 0, 0
for _, info in ipairs(infos) do
  if #buffers >= LISTED_MAX then
    left_out = left_out + 1
  else
    local buf = info.bufnr
    local loaded = api.nvim_buf_is_loaded(buf)
    local entry = {
      path = info.name,
      -- Set below: a loaded buffer's record may be none, and then the host keeps its text out.
      read_from = nil,
      modified = vim.bo[buf].modified,
      loaded = loaded,
      current = buf == here,
      shown = shown[buf] == true,
      lines = loaded and api.nvim_buf_line_count(buf) or 0,
      filetype = vim.bo[buf].filetype,
      text = {},
    }
    if loaded then
      entry.read_from = read_from(buf)
      local format = vim.bo[buf].fileformat
      local terminator = format == 'dos' and 2 or 1
      local count = entry.lines
      local limit = math.min(count, LINES_MAX)
      local bytes, at, full = 0, 1, false
      while at <= limit and not full do
        local upto = math.min(limit, at + BATCH - 1)
        for _, line in ipairs(api.nvim_buf_get_lines(buf, at - 1, upto, false)) do
          -- What the host tells of a line is the line, its number and a tab, and a line break.
          local next_bytes = bytes + #line + terminator + 5
          if next_bytes > BYTES_MAX or total + next_bytes > TOTAL_MAX then
            full = true
            entry.budget = total + next_bytes > TOTAL_MAX
            break
          end
          bytes = next_bytes
          entry.text[#entry.text + 1] = line
        end
        at = upto + 1
      end
      total = total + bytes
      if #entry.text < count then
        entry.cut = #entry.text
      end
    end
    if not loaded then
      entry.read_from = info.name
    end
    buffers[#buffers + 1] = entry
  end
end

return { buffers = buffers, other = other, left_out = left_out }
