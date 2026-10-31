local entry = lsp.entries[key]
if not entry then return { status = 'gone' } end
local missing = {}
for _, name in ipairs(entry.names) do
  if entry.parts[name].pending > 0 then missing[#missing + 1] = name end
end
if #missing > 0 and not args.last then return { status = 'pending' } end
local parts, buffers = {}, {}
for _, name in ipairs(entry.names) do
  local part = entry.parts[name]
  for _, item in ipairs(part.items) do
    local buf = buffers[item.path]
    if buf == nil then
      buf = find_buf(item.path)
      if not buf or not api.nvim_buf_is_loaded(buf) then buf = false end
      buffers[item.path] = buf
    end
    if buf and item.line >= 1 and item.line <= api.nvim_buf_line_count(buf) then
      local text = api.nvim_buf_get_lines(buf, item.line - 1, item.line, true)[1]
      item.text = cut(text, 500)
      item.text_from = api.nvim_buf_get_name(buf)
    end
  end
  parts[#parts + 1] = { name = name, items = part.items, total = part.total }
end
local ticks = {}
for buf, tick in pairs(entry.ticks or {}) do ticks[#ticks + 1] = { buf = buf, tick = tick } end
remove_entry(key)
return { status = 'done', parts = parts, hover = entry.hover, missing = missing, cut = entry.cut,
  edit = entry.edit, actions = entry.actions or {}, action = entry.action, ticks = ticks, asked_ms = entry.asked_ms }
