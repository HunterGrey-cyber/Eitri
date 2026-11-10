if type(args.id) ~= 'string' or #args.id < 1 or #args.id > 40 or args.id:find('[^a-z0-9-]') then
  return { refused = 'the LSP id has the wrong shape' }
end
local stored, stored_client
if args.kind == 'action_edit' then
  stored = type(args.action) == 'string' and lsp.actions[__stamp.scope .. ':' .. args.action] or nil
  if not stored or not api.nvim_buf_is_loaded(stored.asked_buf)
    or api.nvim_buf_get_changedtick(stored.asked_buf) ~= stored.asked_tick then
    return { action_gone = true }
  end
  stored_client = vim.lsp.get_client_by_id(stored.client_id)
  if not stored_client or stored_client:is_stopped() then return { action_gone = true } end
end
local buf, loaded_now = stored and stored.asked_buf or nil, false
if stored then
  if args.path and find_buf(args.path) ~= buf then return { action_gone = true } end
elseif args.path then
  buf = find_buf(args.path)
  if not buf or not api.nvim_buf_is_loaded(buf) then
    if not args.load then return { needs_load = true } end
    local why
    buf, why = load_file(args.path)
    if why == 'swap' then return { swap = true } end
    if not buf then return { refused = 'the file could not be loaded' } end
    loaded_now = true
  end
end
if now_ms() > __stamp.deadline_ms then return { expired = true } end
-- Whether a line or a symbol is there, and whatever a server says of this buffer, come from its text, and so does
-- getting as far as asking for a client: the host judges the buffer before it takes any answer from here on.
local about = buf and { name = api.nvim_buf_get_name(buf), read_from = read_from(buf) } or nil
local row, byte = (args.line or 1) - 1, 0
if args.line and buf then
  local count = api.nvim_buf_line_count(buf)
  if args.line < 1 or args.line > count then return { no_line = true, line_count = count, buffer = about } end
end
if args.symbol and buf then
  local line = api.nvim_buf_get_lines(buf, row, row + 1, true)[1] or ''
  local function word(b)
    return b and (b >= 128 or b == 95 or b >= 48 and b <= 57 or b >= 65 and b <= 90 or b >= 97 and b <= 122)
  end
  local symbol, at, found = args.symbol, 1, nil
  if #symbol == 0 then return { no_symbol = true, buffer = about } end
  while at <= #line do
    local first, last = line:find(symbol, at, true)
    if not first then break end
    if not (word(symbol:byte(1)) and word(line:byte(first - 1)))
      and not (word(symbol:byte(-1)) and word(line:byte(last + 1))) then
      found = first - 1
      break
    end
    at = first + 1
  end
  if not found then return { no_symbol = true, buffer = about } end
  byte = found
end
local clients = {}
if args.kind == 'workspace_symbols' then
  local seen = {}
  local prefix = args.root:gsub('/+$', '') .. '/'
  for _, b in ipairs(api.nvim_list_bufs()) do
    if api.nvim_buf_is_loaded(b) and api.nvim_buf_get_name(b):sub(1, #prefix) == prefix then
      for _, client in ipairs(vim.lsp.get_clients({ bufnr = b })) do
        if not seen[client.id] then
          clients[#clients + 1] = { client = client, buf = b }
          seen[client.id] = true
        end
      end
    end
  end
elseif buf then
  for _, client in ipairs(vim.lsp.get_clients({ bufnr = buf })) do
    clients[#clients + 1] = { client = client, buf = buf }
  end
end
if #clients == 0 then return { no_client = true, loaded_now = loaded_now, buffer = about } end
local methods
if args.kind == 'usages' then
  methods = {
    { 'definitions', 'textDocument/definition' },
    { 'references', 'textDocument/references' },
    { 'implementations', 'textDocument/implementation' },
  }
  if args.include and #args.include > 0 then
    local wanted, selected = {}, {}
    for _, name in ipairs(args.include) do wanted[name] = true end
    for _, method in ipairs(methods) do if wanted[method[1]] then selected[#selected + 1] = method end end
    methods = selected
  end
elseif args.kind == 'hover' then methods = { { 'hover', 'textDocument/hover' } }
elseif args.kind == 'document_symbols' then methods = { { 'document_symbols', 'textDocument/documentSymbol' } }
elseif args.kind == 'workspace_symbols' then methods = { { 'workspace_symbols', 'workspace/symbol' } }
elseif args.kind == 'rename' then methods = { { 'rename', 'textDocument/rename' } }
elseif args.kind == 'format' then methods = { { 'format', 'textDocument/formatting' } }
elseif args.kind == 'code_actions' then methods = { { 'code_actions', 'textDocument/codeAction' } }
elseif args.kind == 'action_edit' then
  -- A bare Command is not a CodeAction: `codeAction/resolve` takes only the latter.
  local bare_command = not has_edit(stored.action) and type(stored.action.command) == 'string'
  if not has_edit(stored.action) and (bare_command or not supports(stored_client, 'codeAction/resolve', buf)) then
    return { no_edit = true, has_command = has_command(stored.action) }
  end
  clients = { { client = stored_client, buf = stored.asked_buf } }
  methods = { { 'action_edit', 'codeAction/resolve' } }
else return { refused = 'unknown LSP request kind' } end
local unsupported, jobs, parts, order = {}, {}, {}, {}
for _, method in ipairs(methods) do
  local name, verb = method[1], method[2]
  local part = { pending = 0, items = {}, total = 0 }
  for _, target in ipairs(clients) do
    if stored and has_edit(stored.action) or supports(target.client, verb, target.buf) then
      part.pending = part.pending + 1
      jobs[#jobs + 1] = { name = name, verb = verb, target = target }
      if args.kind == 'rename' or args.kind == 'format' then break end
    end
  end
  if part.pending == 0 then unsupported[#unsupported + 1] = name
  else parts[name] = part; order[#order + 1] = name end
end
if #jobs == 0 then return { no_client = true, loaded_now = loaded_now, buffer = about } end
-- The clients a request went to, by name (a client without the method was not asked), and those still reporting
-- unfinished work (a server loading its workspace holds or empties its answers meanwhile), so the caller can say
-- which server it waited on.
local client_names, busy, named = {}, {}, {}
for _, job in ipairs(jobs) do
  local client = job.target.client
  local name = type(client.name) == 'string' and cut(client.name, 64) or ''
  if #client_names < 8 and not named[name] then
    named[name] = true
    client_names[#client_names + 1] = name
    local progress = type(client.progress) == 'table' and client.progress.pending
    if type(progress) == 'table' and next(progress) ~= nil then busy[#busy + 1] = name end
  end
end
-- A caller may word the query per server: a client named in `query_for` is asked its own string.
local function query_of(client)
  local per = args.query_for
  local own = type(per) == 'table' and type(client.name) == 'string' and per[client.name] or nil
  if type(own) == 'string' then return own end
  return args.query or ''
end
-- An id is single-use until collected or expired: a second ask under it would drop the first's pending parts.
if lsp.entries[key] then return { refused = 'the LSP id is already in use' } end
lsp.order = (lsp.order or 0) + 1
local phase_c = args.kind == 'rename' or args.kind == 'format' or args.kind == 'code_actions' or stored
local entry = {
  deadline_ms = now_ms() + args.budget_ms, parts = parts, order = lsp.order, names = order,
  ticks = stored and stored.ticks or phase_c and ticks_at_ask(args.root) or {},
  asked_ms = stored and stored.asked_ms or asked_now(),
  asked_tick = buf and api.nvim_buf_get_changedtick(buf), action = stored and action_facts(stored.action) or nil,
}
lsp.entries[key] = entry
lsp.n = lsp.n + 1
clear_past(now_ms())
for _, job in ipairs(jobs) do
  local target = job.target
  local params
  if stored then params = stored.action
  elseif job.verb == 'workspace/symbol' then params = { query = query_of(target.client) }
  else
    params = { textDocument = { uri = vim.uri_from_bufnr(target.buf) } }
    if job.verb ~= 'textDocument/documentSymbol' and job.verb ~= 'textDocument/formatting' and job.verb ~= 'textDocument/codeAction' then
      params.position = position(target.client, target.buf, row, byte)
    end
    if job.verb == 'textDocument/references' then params.context = { includeDeclaration = true } end
    if job.verb == 'textDocument/rename' then params.newName = args.new_name end
    if job.verb == 'textDocument/formatting' then
      local bo = vim.bo[target.buf]
      params.options = { tabSize = bo.shiftwidth == 0 and bo.tabstop or bo.shiftwidth, insertSpaces = bo.expandtab }
    end
    if job.verb == 'textDocument/codeAction' then
      local line = api.nvim_buf_get_lines(target.buf, row, row + 1, true)[1] or ''
      params.range = { start = position(target.client, target.buf, row, 0), ['end'] = position(target.client, target.buf, row, #line) }
      local diagnostics = {}
      for _, diagnostic in ipairs(live_diagnostics(target.buf, { lnum = row })) do
        if type(diagnostic.user_data) == 'table' and type(diagnostic.user_data.lsp) == 'table' then
          diagnostics[#diagnostics + 1] = diagnostic.user_data.lsp
        end
      end
      params.context = { diagnostics = diagnostics, triggerKind = 1 }
    end
  end
  local settled = false
  local function handler(err, result)
    local current = lsp.entries[key]
    if settled or current ~= entry then return end
    settled = true
    local part = current.parts[job.name]
    part.pending = part.pending - 1
    if err then return end
    -- A malformed server response ends only this client's part.
    pcall(function()
      if args.kind == 'rename' then current.edit = normal_edit(result, target.client)
      elseif args.kind == 'format' then current.edit = normal_edit(result, target.client, target.buf)
      elseif args.kind == 'code_actions' then keep_actions(current, target.client, target.buf, result)
      elseif args.kind == 'action_edit' then
        current.edit = normal_edit(type(result) == 'table' and result.edit or nil, target.client)
        -- A command that only the resolved action carries is still one Eitri does not run.
        if type(result) == 'table' and current.action and has_command(result) then current.action.has_command = true end
      elseif args.kind == 'hover' then
        local text = hover_text(type(result) == 'table' and result.contents or nil)
        if text ~= '' then
          local combined = current.hover and current.hover .. '\n\n' .. text or text
          local clipped
          current.hover, clipped = cut(combined, 16384)
          current.cut = current.cut or clipped
        end
      elseif args.kind == 'document_symbols' or args.kind == 'workspace_symbols' then
        symbols(part, result, vim.uri_from_bufnr(target.buf))
      else locations(part, result) end
    end)
  end
  if stored and has_edit(stored.action) then handler(nil, stored.action)
  else
    local ok, sent = pcall(request, target.client, job.verb, params, handler, target.buf)
    if not ok or not sent then handler(true, nil) end
  end
end
return { started = true, unsupported = unsupported, action = entry.action, buffer = about, clients = client_names,
  busy = busy }
