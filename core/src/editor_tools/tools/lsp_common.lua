if not __state.lsp then __state.lsp = { entries = {}, n = 0 } end
local lsp = __state.lsp
lsp.actions = lsp.actions or {}
local function now_ms()
  local sec, usec = vim.uv.gettimeofday()
  return sec * 1000 + usec / 1000
end
-- Rounded up, so a file saved before this moment is never later than it: a file's time is on the same clock.
local function asked_now()
  return math.ceil(now_ms())
end
local modern = vim.fn.has('nvim-0.11') == 1
-- An entry dropped with a request still out cancels it: a server that never answers keeps nothing waiting in nvim.
local function remove_entry(key)
  local entry = lsp.entries[key]
  if entry then
    lsp.entries[key] = nil
    lsp.n = lsp.n - 1
    for _, sent in ipairs(entry.sent or {}) do
      if not sent.done then
        if modern then pcall(sent.client.cancel_request, sent.client, sent.id)
        else pcall(sent.client.cancel_request, sent.id) end
      end
    end
  end
end
local function clear_past(now)
  local actions = {}
  for key, action in pairs(lsp.actions) do
    if now > action.deadline_ms then lsp.actions[key] = nil
    else actions[#actions + 1] = { key = key, order = action.order } end
  end
  table.sort(actions, function(a, b) return a.order < b.order end)
  for i = 1, #actions - 256 do lsp.actions[actions[i].key] = nil end
  local ordered = {}
  for key, entry in pairs(lsp.entries) do
    if now > entry.deadline_ms then
      remove_entry(key)
    else
      ordered[#ordered + 1] = { key = key, order = entry.order }
    end
  end
  table.sort(ordered, function(a, b) return a.order < b.order end)
  for i = 1, #ordered - 32 do remove_entry(ordered[i].key) end
end
clear_past(now_ms())

-- str_utf_start returns the offset from a one-based byte index to that character's first byte.
local function cut(text, n)
  if #text <= n then return text, false end
  return text:sub(1, n + vim.str_utf_start(text, n + 1)), true
end
local function supports(client, method, buf)
  if modern then return client:supports_method(method, buf) end
  return client.supports_method(method, { bufnr = buf })
end
local function request(client, method, params, handler, buf)
  if modern then return client:request(method, params, handler, buf) end
  return client.request(method, params, handler, buf)
end
local function position(client, buf, row, byte)
  return { line = row, character = vim.lsp.util.character_offset(buf, row, byte, client.offset_encoding) }
end
local function path_of(uri)
  if type(uri) ~= 'string' then return nil end
  local path = uri:sub(1, 5) == 'file:' and vim.uri_to_fname(uri) or uri
  if #path > 4096 then return nil end
  return path
end
local function whole(n)
  return type(n) == 'number' and n >= 0 and n <= 4294967294 and n == math.floor(n)
end
local function add_item(part, uri, range, name, kind, container)
  part.total = part.total + 1
  if #part.items >= 200 then return end
  local path = path_of(uri)
  if not path then return end
  local start = range and range.start
  if type(start) ~= 'table' then start = { line = 0, character = 0 } end
  -- A position must be a whole number a u32 holds, with room for the one-based line; a server's other
  -- values would fail the whole answer, so such an item is dropped (it stays counted in `total`).
  local line, character = start.line or 0, start.character or 0
  if not whole(line) or not whole(character) then return end
  local item = { path = path, line = line + 1, character = character }
  if type(name) == 'string' then item.name = cut(name, 1024) end
  if type(container) == 'string' then item.container = cut(container, 1024) end
  if type(kind) == 'number' then item.kind = vim.lsp.protocol.SymbolKind[kind] end
  part.items[#part.items + 1] = item
end
local function locations(part, result)
  if type(result) ~= 'table' then return end
  if result.uri or result.targetUri then result = { result } end
  for _, loc in ipairs(result) do
    if type(loc) == 'table' then
      add_item(part, loc.targetUri or loc.uri, loc.targetSelectionRange or loc.range)
    end
  end
end
local function symbols(part, result, uri, container)
  if type(result) ~= 'table' then return end
  for _, sym in ipairs(result) do
    if type(sym) == 'table' then
      if sym.location then
        add_item(part, sym.location.uri, sym.location.range, sym.name, sym.kind, sym.containerName)
      else
        add_item(part, uri, sym.selectionRange or sym.range, sym.name, sym.kind, container)
        symbols(part, sym.children, uri, sym.name)
      end
    end
  end
end
local function hover_text(contents)
  if type(contents) == 'string' then return contents end
  if type(contents) ~= 'table' then return '' end
  if type(contents.value) == 'string' then return contents.value end
  local out = {}
  for _, item in ipairs(contents) do out[#out + 1] = hover_text(item) end
  return table.concat(out, '\n\n')
end
local key = __stamp.scope .. ':' .. args.id

-- Positions stay tied to the loaded buffers at the moment of the ask, even if a later save matches disk again.
local function ticks_at_ask(root)
  local ticks, prefix = {}, root:gsub('/+$', '') .. '/'
  for _, buf in ipairs(api.nvim_list_bufs()) do
    if api.nvim_buf_is_loaded(buf) and api.nvim_buf_get_name(buf):sub(1, #prefix) == prefix then
      ticks[buf] = api.nvim_buf_get_changedtick(buf)
    end
  end
  return ticks
end
local function has_command(action)
  return type(action.command) == 'string' or type(action.command) == 'table' and action.command ~= vim.NIL
end
local function has_edit(action)
  return type(action.edit) == 'table' and action.edit ~= vim.NIL
end
local function action_facts(action)
  return { title = cut(type(action.title) == 'string' and action.title or '', 1024), has_command = has_command(action) }
end
local function keep_actions(entry, client, buf, result)
  if type(result) ~= 'table' then return end
  entry.actions = entry.actions or {}
  for _, action in ipairs(result) do
    if #entry.actions >= 64 then break end
    if type(action) == 'table' and type(action.title) == 'string' then
      local id = args.id .. '-' .. (#entry.actions + 1)
      lsp.action_order = (lsp.action_order or 0) + 1
      lsp.actions[__stamp.scope .. ':' .. id] = {
        client_id = client.id, action = action, asked_buf = buf, ticks = entry.ticks,
        asked_tick = entry.asked_tick, asked_ms = entry.asked_ms,
        deadline_ms = now_ms() + 600000, order = lsp.action_order,
      }
      local summary = { id = id, title = cut(action.title, 1024), has_edit = has_edit(action), has_command = has_command(action) }
      if type(action.kind) == 'string' then summary.kind = cut(action.kind, 1024) end
      if type(action.disabled) == 'table' and type(action.disabled.reason) == 'string' then
        summary.disabled = cut(action.disabled.reason, 1024)
      end
      entry.actions[#entry.actions + 1] = summary
    end
  end
  clear_past(now_ms())
end

-- Keep document entries separate: the caller must reject two sets of positions for the same file.
local function normal_edit(result, client, format_buf)
  if type(result) ~= 'table' or result == vim.NIL then return nil end
  local edit = { encoding = client.offset_encoding or 'utf-16', files = {}, resource_ops = {}, too_large = false }
  local count, bytes, entries = 0, 0, 0
  -- A document entry or a resource operation counts toward the same limit as an edit, so empty ones are bounded too.
  local function entry()
    entries = entries + 1
    if entries > 20000 then edit.too_large = true end
  end
  -- Names share the text's byte limit: the whole edit goes back in one frame of the editor's connection, which ends
  -- at a frame over 16 MiB and takes every other request with it.
  local function spend(n)
    bytes = bytes + n
    if bytes > 8388608 then edit.too_large = true end
  end
  local function path(uri)
    if type(uri) ~= 'string' then error('invalid edit URI') end
    local name = uri:sub(1, 5) == 'file:' and vim.uri_to_fname(uri) or uri
    if #name > 4096 then edit.too_large = true end
    spend(#name)
    return name
  end
  local function file(uri, edits)
    local item = { path = path(uri), edits = {} }
    entry()
    if edit.too_large then return end
    for _, raw in ipairs(edits) do
      count = count + 1
      if type(raw.newText) ~= 'string' then error('invalid edit text') end
      spend(#raw.newText)
      if count > 20000 then edit.too_large = true end
      if edit.too_large then return end
      local range = raw.range
      for _, pos in ipairs({ range.start, range['end'] }) do
        if type(pos.line) ~= 'number' or pos.line < 0 or pos.line > 4294967295 or pos.line ~= math.floor(pos.line)
          or type(pos.character) ~= 'number' or pos.character < 0 or pos.character > 4294967295
          or pos.character ~= math.floor(pos.character) then error('invalid edit position') end
      end
      item.edits[#item.edits + 1] = { start = range.start, ['end'] = range['end'], new_text = raw.newText }
    end
    edit.files[#edit.files + 1] = item
  end
  if format_buf then
    file(vim.uri_from_bufnr(format_buf), result)
  elseif result.documentChanges ~= nil and result.documentChanges ~= vim.NIL then
    for _, change in ipairs(result.documentChanges) do
      if change.kind then
        entry()
        local name = path(change.uri or change.oldUri or change.newUri)
        if change.newUri then name = name .. ' -> ' .. path(change.newUri) end
        local kind = cut(tostring(change.kind), 1024)
        spend(#kind)
        edit.resource_ops[#edit.resource_ops + 1] = kind .. ' ' .. name
      else file(change.textDocument.uri, change.edits) end
      if edit.too_large then break end
    end
  elseif result.changes ~= nil and result.changes ~= vim.NIL then
    for uri, edits in pairs(result.changes) do
      file(uri, edits)
      if edit.too_large then break end
    end
  end
  if edit.too_large then edit.files = {}; edit.resource_ops = {} end
  return edit
end
