-- What the editor's diagnostics say, for one loaded buffer or every loaded buffer under the root. Loads nothing and
-- changes nothing. The answer is the most severe first, cut at args.limit or args.answer_max with the rest counted;
-- Rust judges every path before it is told. Lines and columns are vim's own: 0-based.
local MESSAGE_MAX = 2048
local FIELD_MAX = 256
-- An entry's keys and numbers as JSON, at most.
local ENTRY_ROOM = 256
local WORD = { 'error', 'warning', 'info', 'hint' }
local NUMBER = { error = 1, warning = 2, info = 3, hint = 4 }

local least = NUMBER[args.min_severity] or 4
local limit = math.max(math.floor(tonumber(args.limit) or 0), 0)

local function wanted(buf)
  return api.nvim_buf_is_loaded(buf) and api.nvim_buf_get_name(buf) ~= '' and vim.bo[buf].buftype == ''
end

local function slashed(path)
  return path:sub(-1) == '/' and path or path .. '/'
end

local function under(prefix, name)
  return name:sub(1, #prefix) == prefix
end

-- Where a name leads: through its nearest part that exists, so a new file under a link goes where the link does.
local function leads_to(name)
  local at = name
  while at ~= '/' and at ~= '.' do
    local real = vim.uv.fs_realpath(at)
    if real then
      return real
    end
    at = vim.fs.dirname(at)
  end
  return '/'
end

-- Whether a buffer's text came from under `real`: a buffer with a name on disk and no record may hold anything.
local function read_under(real, buf, name)
  local record = read_from(buf)
  if record then
    return under(real, slashed(leads_to(record)))
  end
  return not vim.uv.fs_lstat(name)
end

local buffers = {}
if args.path then
  local buf = find_buf(args.path)
  if buf and wanted(buf) then
    buffers[1] = buf
  end
else
  -- A name under the root that leads out of it through a link, or a buffer read from elsewhere, stays out here: Rust
  -- would drop its entries, after they had taken the places of the project's own under the limit.
  local root = slashed(args.root)
  local real = slashed(vim.uv.fs_realpath(args.root) or args.root)
  for _, buf in ipairs(api.nvim_list_bufs()) do
    if wanted(buf) then
      local name = api.nvim_buf_get_name(buf)
      if under(root, name) and under(real, slashed(leads_to(name))) and read_under(real, buf, name) then
        buffers[#buffers + 1] = buf
      end
    end
  end
end

local found, modified = {}, {}
for _, buf in ipairs(buffers) do
  local name = api.nvim_buf_get_name(buf)
  local any = false
  for _, d in ipairs(live_diagnostics(buf)) do
    local severity = d.severity or 1
    if severity <= least then
      found[#found + 1] = { buf = buf, name = name, d = d, severity = severity, order = #found }
      any = true
    end
  end
  if any and vim.bo[buf].modified then
    modified[#modified + 1] = name
  end
end

table.sort(found, function(a, b)
  if a.severity ~= b.severity then return a.severity < b.severity end
  if a.name ~= b.name then return a.name < b.name end
  if a.d.lnum ~= b.d.lnum then return a.d.lnum < b.d.lnum end
  if a.d.col ~= b.d.col then return a.d.col < b.d.col end
  return a.order < b.order
end)

-- A cut that does not end inside a multi-byte character.
local function cut(text, max)
  if #text <= max then
    return text
  end
  local at = max
  while at > 0 do
    local after = text:byte(at + 1)
    if after and after >= 0x80 and after < 0xC0 then at = at - 1 else break end
  end
  return text:sub(1, at)
end

-- UTF-16 units before byte `col` of line `lnum`: nvim 0.11 takes the encoding, 0.10 answers both counts.
local function units(buf, lnum, col)
  local line = api.nvim_buf_get_lines(buf, lnum, lnum + 1, false)[1] or ''
  col = math.min(col, #line)
  local ok, count = pcall(vim.str_utfindex, line, 'utf-16', col)
  if ok and type(count) == 'number' then
    return count
  end
  local old, _, utf16 = pcall(vim.str_utfindex, line, col)
  if old and type(utf16) == 'number' then
    return utf16
  end
  return col
end

local function whole(value, fallback)
  return math.max(math.floor(tonumber(value) or fallback), 0)
end

-- What a string can come to as JSON in a reply to the other window. As a JSON string a control byte is six bytes. A
-- byte above 0x7F may mean it is not UTF-8, and such a string travels as a list of numbers, four bytes for each of its
-- own; whether it is UTF-8 is not known here, so it counts as the longer of the two.
local function wire_len(text)
  local _, quoted = text:gsub('["\\]', '')
  local _, control = text:gsub('%c', '')
  local len = #text + quoted + 5 * control + 2
  if text:find('[\128-\255]') then
    return math.max(len, 4 * #text + 12)
  end
  return len
end

local function entry_len(entry)
  local len = ENTRY_ROOM + wire_len(entry.path) + wire_len(entry.message)
  if entry.read_from then len = len + wire_len(entry.read_from) end
  if entry.source then len = len + wire_len(entry.source) end
  if entry.code then len = len + wire_len(entry.code) end
  return len
end

local room = tonumber(args.answer_max) or 0
for _, name in ipairs(modified) do
  room = room - wire_len(name)
end

local entries = {}
for i = 1, math.min(#found, limit) do
  local item = found[i]
  local d = item.d
  local lnum, col = whole(d.lnum, 0), whole(d.col, 0)
  local end_lnum, end_col = whole(d.end_lnum, lnum), whole(d.end_col, col)
  local entry = {
    path = item.name,
    read_from = read_from(item.buf),
    lnum = lnum,
    col = col,
    end_lnum = end_lnum,
    end_col = end_col,
    severity = WORD[item.severity] or 'hint',
    message = cut(tostring(d.message or ''), MESSAGE_MAX),
  }
  if d.source ~= nil then entry.source = cut(tostring(d.source), FIELD_MAX) end
  if d.code ~= nil then entry.code = cut(tostring(d.code), FIELD_MAX) end
  if args.utf16 then
    entry.start_character = units(item.buf, lnum, col)
    entry.end_character = units(item.buf, end_lnum, end_col)
  end
  room = room - entry_len(entry)
  if room < 0 then break end
  entries[#entries + 1] = entry
end

return { entries = entries, modified = modified, left_out = #found - #entries }
