-- Files of a remote project's server that the mirror does not hold (a dependency's source, the standard library, a
-- generated file): `eitri-remote://<host>/<path>` buffers, read from the server and never writable here.
--
-- The bridge (`eitri host lsp`) turns a language server's `file://` URI for such a file into this name, the panel opens
-- one for a path outside the mirror, and `:EitriRemote open <path>` opens one by hand. The text comes through
-- `eitri host read`, asynchronously: the buffer shows "reading from <host>…" until it lands, so nothing here waits on
-- the network. What arrives is put into the buffer and nothing more: the file is never run or sourced, `modeline`
-- is off for the buffer, and `filetype` is detected from the path alone.
local M = {}
local api = vim.api
local unpack = table.unpack or unpack

local SCHEME = "eitri-remote://"
local PATTERN = SCHEME .. "*"
-- A jump's wish that was never claimed by a read (the buffer was wiped first) is forgotten after this long.
local WISH_LIFETIME_MS = 60000
-- While the text is on its way the buffer holds this many lines (the notice and blank ones), more when a jump wants a
-- later line: code that puts the cursor on a line of the buffer the moment it is loaded (Neovim's own `gd`, a quickfix
-- entry) fails for a line the buffer does not have yet.
local PLACEHOLDER_LINES = 1000
local PLACEHOLDER_LINES_MAX = 200000
-- At most this many `eitri host read` run at once (a list of references can name many files that are not read yet);
-- the others wait their turn in the order they were asked for.
local READS_MAX = 4

local options = {}
local installed = false
-- The jump functions this module wrapped, by name (nil until wrapped).
M.originals = {}
-- bufnr -> { host, path, read = <system object>, id = <read counter>, loading = bool, want, clients }
local states = {}
-- bufnr -> { want, clients, at }: what a jump into a buffer that is not read yet asked for
local wishes = {}
-- The reads running, and the ones waiting: { buf, id } in the order they were asked for.
local reading = 0
local waiting = {}
-- The client whose `window/showDocument` is being handled, while it is (nvim's handler knows it; the jump does not).
local producer

-- Hidden characters are written out wherever the host or a path is shown: a name the server chose must not move the
-- cursor, retitle the terminal or reorder the line (`eitri.remote.text`).
M.escape = require("eitri.remote.text").escape

local function notify(text, level)
  vim.notify(text, level or vim.log.levels.ERROR)
end

--- The name of the buffer for `path` (absolute) on `host`, as `eitri_core::remote_lsp::uris::remote_uri` writes it.
function M.uri(host, path)
  local encoded_host = host:gsub("[^A-Za-z0-9%-%._~]", function(c)
    return ("%%%02X"):format(c:byte())
  end)
  return SCHEME .. encoded_host .. vim.uri_encode(path, "rfc3986")
end

--- `host, path` of a buffer name, both decoded; nil for a name that is not one of these.
function M.parse(name)
  local host, path = tostring(name):match("^eitri%-remote://([^/]+)(/.*)$")
  if not host then
    return nil
  end
  host, path = vim.uri_decode(host), vim.uri_decode(path)
  if host:find("\0", 1, true) or path:find("\0", 1, true) or host == "" then
    return nil
  end
  return host, path
end

local function program()
  local cmd = options.cmd
  if type(cmd) == "function" then
    cmd = cmd()
  end
  if cmd then
    return cmd
  end
  local ok, eitri = pcall(require, "eitri")
  if ok and type(eitri) == "table" and type(eitri.program) == "function" then
    return eitri.program()
  end
  return "eitri"
end

-- The text of a buffer that is read-only here: set with 'modifiable' on for the moment, never 'modified'.
local function put(buf, lines)
  local bo = vim.bo[buf]
  -- 'readonly' goes too: Neovim warns (W10) of a change to a readonly buffer, and this one is ours to change.
  bo.readonly = false
  bo.modifiable = true
  local ok, why = pcall(api.nvim_buf_set_lines, buf, 0, -1, false, lines)
  bo.modified = false
  bo.modifiable = false
  bo.readonly = true
  return ok, why
end

local function split_lines(text)
  local lines = vim.split(text, "\n", { plain = true })
  if lines[#lines] == "" then
    table.remove(lines)
  end
  return lines
end

-- The byte column of `character`, counted in `encoding`, on `line`; one past the end lands on the line's end.
local function byte_column(line, character, encoding)
  if encoding == "utf-8" then
    return math.min(character, #line)
  end
  local units, at, size = 0, 1, #line
  while at <= size and units < character do
    local b = line:byte(at)
    local width = b < 0x80 and 1 or b >= 0xF0 and 4 or b >= 0xE0 and 3 or b >= 0xC0 and 2 or 1
    units = units + ((encoding ~= "utf-32" and width == 4) and 2 or 1)
    at = at + width
  end
  return math.min(at - 1, size)
end

local function place(buf, want)
  if not want then
    return
  end
  local row = math.max(1, math.min(want.line, api.nvim_buf_line_count(buf)))
  local text = api.nvim_buf_get_lines(buf, row - 1, row, false)[1] or ""
  local col = want.character and byte_column(text, want.character, want.encoding) or 0
  for _, win in ipairs(vim.fn.win_findbuf(buf)) do
    if pcall(api.nvim_win_set_cursor, win, { row, col }) then
      pcall(api.nvim_win_call, win, function()
        vim.cmd("normal! zv")
      end)
    end
  end
end

-- The host of a bridge client (`eitri host lsp --root <root> ...`): the host of the recorded project its root is in.
-- Nil for any other client.
local function bridge_host(client)
  local cmd = type(client) == "table" and type(client.config) == "table" and client.config.cmd
  local ok, lsp = pcall(require, "eitri.remote.lsp")
  return ok and lsp.bridge_host(cmd) or nil
end

-- Only a bridge to the buffer's own host is attached: a client of another host's server would be sent this file's
-- text, which is not its server's to see.
local function attach(buf, clients)
  local host = M.parse(api.nvim_buf_get_name(buf))
  for _, id in ipairs(clients or {}) do
    local client = vim.lsp.get_client_by_id(id)
    if host and client and bridge_host(client) == host then
      pcall(vim.lsp.buf_attach_client, buf, id)
    end
  end
end

local function failed(buf, st, why)
  local text = ("could not read %s:%s"):format(M.escape(st.host), M.escape(st.path))
  local lines = { text }
  for line in vim.gsplit(M.escape((why or ""):sub(1, 2000)), "\n", { plain = true }) do
    if line ~= "" then
      lines[#lines + 1] = line
    end
  end
  put(buf, lines)
  notify(table.concat(lines, ": "))
end

local function landed(buf, st, text)
  -- Where code that moved the cursor into the buffer while it was a placeholder left it.
  local cursors = {}
  for _, win in ipairs(vim.fn.win_findbuf(buf)) do
    cursors[win] = api.nvim_win_get_cursor(win)
  end
  local ok = put(buf, split_lines(text))
  if not ok then
    put(buf, { ("%s:%s holds bytes this buffer cannot show"):format(M.escape(st.host), M.escape(st.path)) })
    return
  end
  local found, filetype = pcall(vim.filetype.match, { buf = buf, filename = st.path })
  if found and filetype then
    vim.bo[buf].filetype = filetype
  end
  attach(buf, st.clients)
  if st.want then
    place(buf, st.want)
  else
    for win, cursor in pairs(cursors) do
      pcall(api.nvim_win_set_cursor, win, { math.min(cursor[1], api.nvim_buf_line_count(buf)), cursor[2] })
    end
  end
  st.want, st.clients = nil, nil
end

local pump

local function finished(buf, id, result)
  local st = states[buf]
  if not api.nvim_buf_is_valid(buf) or not api.nvim_buf_is_loaded(buf) or not st or st.id ~= id then
    return
  end
  st.read, st.loading = nil, false
  if result.code ~= 0 or (result.signal or 0) ~= 0 then
    local why = vim.trim(result.stderr or "")
    failed(buf, st, why ~= "" and why or ("eitri host read ended with " .. tostring(result.code)))
    return
  end
  landed(buf, st, result.stdout or "")
end

local function cancel(buf)
  local st = states[buf]
  if st and (st.read or st.queued) then
    if st.read then
      pcall(st.read.kill, st.read, 15)
    end
    st.read, st.queued = nil, false
    -- Whatever the stopped read still reports is no longer this buffer's, and its turn in the queue is gone.
    st.id = st.id + 1
  end
end

-- Starts `eitri host read` for the buffer; the count of running reads goes down when the process has ended, however
-- it ended, and the next waiting read starts then.
local function start_read(buf, st)
  local id = st.id
  local started, handle = pcall(
    vim.system,
    { program(), "host", "read", st.host .. ":" .. st.path },
    { text = false },
    vim.schedule_wrap(function(result)
      reading = reading - 1
      finished(buf, id, result)
      pump()
    end)
  )
  if started then
    reading = reading + 1
    st.read = handle
  else
    st.loading = false
    failed(buf, st, tostring(handle))
  end
end

pump = function()
  while reading < READS_MAX and #waiting > 0 do
    local next_read = table.remove(waiting, 1)
    local st = states[next_read.buf]
    if st and st.queued and st.id == next_read.id and api.nvim_buf_is_loaded(next_read.buf) then
      st.queued = false
      start_read(next_read.buf, st)
    end
  end
end

-- BufReadCmd: the options first, so nothing is written to disk for the buffer, then the read starts.
local function load(buf)
  local host, path = M.parse(api.nvim_buf_get_name(buf))
  local bo = vim.bo[buf]
  bo.swapfile = false
  bo.modeline = false
  bo.buftype = ""
  -- The placeholder and the text are not edits: nothing to undo back to.
  bo.undolevels = -1
  if not host then
    put(buf, { "this is not a name eitri-remote:// buffers have: eitri-remote://<host>/<absolute path>" })
    return
  end
  cancel(buf)
  local wish = wishes[buf]
  wishes[buf] = nil
  local st = { host = host, path = path, id = ((states[buf] or {}).id or 0) + 1, loading = true }
  if wish then
    st.want, st.clients = wish.want, wish.clients
  end
  states[buf] = st
  vim.b[buf].eitri_remote = { host = host, path = path }
  local notice = { ("reading from %s…"):format(M.escape(host)) }
  local count = math.min(math.max(PLACEHOLDER_LINES, st.want and st.want.line + 1 or 0), PLACEHOLDER_LINES_MAX)
  for i = 2, count do
    notice[i] = ""
  end
  put(buf, notice)
  st.queued = true
  table.insert(waiting, { buf = buf, id = st.id })
  pump()
end

local function refuse_write(buf)
  local host = M.parse(api.nvim_buf_get_name(buf))
  notify(("this file is on %s and read-only here"):format(M.escape(host or "another machine")))
end

--- Puts the cursor on `line` (one-based) of an `eitri-remote://` buffer once its text is here, or now if it is.
function M.want(buf, line, character, encoding)
  local st = states[buf]
  if not st then
    return
  end
  local want = { line = line, character = character, encoding = encoding }
  if st.loading then
    st.want = want
  else
    place(buf, want)
  end
end

-- The clients to attach to the file at `path` on `host` that a location named: the client that sent it when that is
-- known (a `window/showDocument`), else the bridges of the buffer the jump leaves that reach `host` and serve the
-- file's language (a client that names its filetypes and not this one is a server for other files). `attach` checks
-- the host again.
local function location_clients(host, path)
  if producer then
    return { producer }
  end
  local found, filetype = pcall(vim.filetype.match, { filename = path })
  filetype = found and filetype or nil
  local ids = {}
  local ok, clients = pcall(vim.lsp.get_clients, { bufnr = api.nvim_get_current_buf() })
  for _, client in ipairs(ok and clients or {}) do
    local filetypes = client.config and client.config.filetypes
    local serves = type(filetypes) ~= "table" or (filetype ~= nil and vim.tbl_contains(filetypes, filetype))
    if serves and bridge_host(client) == host then
      ids[#ids + 1] = client.id
    end
  end
  return ids
end

-- The `--root` of a bridge client's command; nil for any other client.
local function bridge_root(client)
  local cmd = type(client) == "table" and type(client.config) == "table" and client.config.cmd
  local ok, lsp = pcall(require, "eitri.remote.lsp")
  return ok and lsp.bridge_root(cmd) or nil
end

-- How deep `root` holds `path` (its length), or nil when it does not hold it.
local function holding_depth(root, path)
  if type(root) ~= "string" then
    return nil
  end
  local trimmed = root:gsub("/+$", "")
  if path == trimmed or path:sub(1, #trimmed + 1) == trimmed .. "/" then
    return #trimmed
  end
  return nil
end

-- The clients of a file a person or the panel opens by name: the ones a jump from the current buffer would give it, and
-- when the current buffer has none for the file, a bridge to `host` already running whose `config.filetypes` names
-- the file's language (a client that names no filetypes would serve every file, so it is not taken from elsewhere).
-- Of those, the bridges whose root holds the file (the deepest such root); when no root holds it (a dependency outside
-- every root), the one bridge there is, and none when there are several: two projects' servers would both report on
-- the file, and a server whose root lacks it would be sent it.
local function open_clients(host, path)
  local ids = location_clients(host, path)
  if #ids > 0 then
    return ids
  end
  local found, filetype = pcall(vim.filetype.match, { filename = path })
  if not found or not filetype then
    return ids
  end
  local holding, deepest = {}, -1
  local ok, clients = pcall(vim.lsp.get_clients)
  for _, client in ipairs(ok and clients or {}) do
    local filetypes = client.config and client.config.filetypes
    if type(filetypes) == "table" and vim.tbl_contains(filetypes, filetype) and bridge_host(client) == host then
      ids[#ids + 1] = client.id
      local depth = holding_depth(bridge_root(client), path)
      if depth and depth > deepest then
        holding, deepest = { client.id }, depth
      elseif depth and depth == deepest then
        holding[#holding + 1] = client.id
      end
    end
  end
  if #holding > 0 then
    return holding
  end
  return #ids == 1 and ids or {}
end

-- The ids of `first` and `second`, each once: a second jump into a buffer that is still loading adds its clients.
local function merged(first, second)
  local out, seen = {}, {}
  for _, list in ipairs({ first or {}, second or {} }) do
    for _, id in ipairs(list) do
      if not seen[id] then
        seen[id] = true
        out[#out + 1] = id
      end
    end
  end
  return out
end

-- What a jump to a location in an `eitri-remote://` buffer needs of it, noted before the jump opens the buffer: the
-- clients to attach (`chosen`, else `location_clients`), and the position (counted in the client's offset encoding)
-- that waits for the text. Gives the buffer, and whether it is still to be read (a placeholder, or not loaded at all).
local function note(location, offset_encoding, chosen)
  local uri = type(location) == "table" and (location.uri or location.targetUri)
  local host, path = M.parse(uri)
  if not host then
    return nil
  end
  local buf = vim.uri_to_bufnr(uri)
  local clients = chosen or location_clients(host, path)
  local range = location.range or location.targetSelectionRange
  local want = range
    and type(range.start) == "table"
    and { line = (range.start.line or 0) + 1, character = range.start.character or 0, encoding = offset_encoding }
  local st = states[buf]
  if st and st.loading then
    st.want, st.clients = want or st.want, merged(st.clients, clients)
    return buf, true
  end
  if api.nvim_buf_is_loaded(buf) then
    attach(buf, clients)
    return buf, false
  end
  local now = (vim.uv or vim.loop).now()
  for key, wish in pairs(wishes) do
    if now - wish.at > WISH_LIFETIME_MS then
      wishes[key] = nil
    end
  end
  local earlier = wishes[buf] or {}
  wishes[buf] = { want = want or earlier.want, clients = merged(earlier.clients, clients), at = now }
  return buf, true
end

-- `show_document` and `jump_to_location` open a location with its cursor set at once: for a buffer still to be read the
-- jump goes to the first line, and the position waits for the text.
local HELD = "eitri_remote_range_held"

local function wrapped_jump(original)
  return function(location, offset_encoding, ...)
    if type(location) ~= "table" or location[HELD] then
      return original(location, offset_encoding, ...)
    end
    -- A server's `window/showDocument` opens a file of its own host only: the name of another host's file would have
    -- this machine read it there.
    local host = M.parse(location.uri or location.targetUri)
    if host and producer and bridge_host(vim.lsp.get_client_by_id(producer)) ~= host then
      notify(("a language server asked to show a file on %s, which is not its host; refused"):format(M.escape(host)))
      return false
    end
    local _, unread = note(location, offset_encoding)
    if not unread then
      return original(location, offset_encoding, ...)
    end
    local top = { start = { line = 0, character = 0 }, ["end"] = { line = 0, character = 0 } }
    local held = vim.deepcopy(location)
    held.range, held.targetSelectionRange, held[HELD] = top, top, true
    return original(held, offset_encoding, ...)
  end
end

-- Neovim 0.11 and later open the one location of a `gd` themselves, from the items `locations_to_items` makes: the
-- single location is noted there, and the cursor they set on the placeholder is replaced by the wanted position.
local function wrapped_items(original)
  return function(locations, offset_encoding, ...)
    if type(locations) == "table" and #locations == 1 then
      note(locations[1], offset_encoding)
    end
    return original(locations, offset_encoding, ...)
  end
end

-- Neovim's handler of `window/showDocument` knows which client asked; the jump it calls is told who for as long as
-- it runs.
local function wrapped_show_document_handler(original)
  return function(err, result, ctx, ...)
    local before = producer
    producer = type(ctx) == "table" and ctx.client_id or nil
    local ok, answer, more = pcall(original, err, result, ctx, ...)
    producer = before
    if not ok then
      error(answer, 0)
    end
    return answer, more
  end
end

local function wrap_jumps()
  local ok, util = pcall(function()
    return vim.lsp.util
  end)
  if not ok or type(util) ~= "table" then
    return
  end
  local handlers = vim.lsp.handlers
  local show = type(handlers) == "table" and handlers["window/showDocument"]
  if type(show) == "function" and not M.originals.show_document_handler then
    M.originals.show_document_handler = show
    handlers["window/showDocument"] = wrapped_show_document_handler(show)
  end
  for name, wrap in pairs({
    show_document = wrapped_jump,
    jump_to_location = wrapped_jump,
    locations_to_items = wrapped_items,
  }) do
    local original = rawget(util, name)
    if type(original) == "function" and not M.originals[name] then
      M.originals[name] = original
      util[name] = wrap(original)
    end
  end
end

--- Before the `:edit` of the `eitri-remote://` name `name` that a person or the panel asked for: the clients to attach
--- (`open_clients`, chosen from the buffer the open leaves) are noted for when its text is here, or attached now when
--- it is. Nothing for any other name.
function M.opening(name)
  local host, path = M.parse(name)
  if host then
    note({ uri = name }, nil, open_clients(host, path))
  end
end

-- The host `:EitriRemote open` reads from: an `eitri-remote://` buffer's own, else the host of the recorded project the
-- buffer (or, for a buffer with no name, the working directory) belongs to.
local function host_here()
  local mark = vim.b.eitri_remote
  if type(mark) == "table" and type(mark.host) == "string" then
    return mark.host
  end
  local name = api.nvim_buf_get_name(0)
  local named = M.parse(name)
  if named then
    return named
  end
  local project = require("eitri.remote").project_of(name ~= "" and name or vim.fn.getcwd())
  return project and project.host or nil
end

--- `:EitriRemote open <path>`: the file at the absolute `path` on the server of the project this buffer belongs to (in
--- an `eitri-remote://` buffer, on that buffer's host).
function M.open(path)
  path = vim.trim(path or "")
  local host = host_here()
  if not host then
    notify("this buffer is outside a recorded remote project")
    return
  end
  if path:sub(1, 1) ~= "/" then
    notify(("%s needs an absolute path on %s"):format(M.escape(path ~= "" and path or "(nothing)"), M.escape(host)))
    return
  end
  local name = M.uri(host, path)
  M.opening(name)
  api.nvim_cmd({ cmd = "edit", args = { name }, magic = { file = false } }, {})
end

function M.setup(opts)
  options = opts or options
  if installed then
    return
  end
  installed = true
  local group = api.nvim_create_augroup("EitriRemoteBuffers", { clear = true })
  api.nvim_create_autocmd("BufReadCmd", {
    group = group,
    pattern = PATTERN,
    callback = function(args)
      load(args.buf)
    end,
  })
  api.nvim_create_autocmd({ "BufWriteCmd", "FileWriteCmd" }, {
    group = group,
    pattern = PATTERN,
    callback = function(args)
      refuse_write(args.buf)
    end,
  })
  api.nvim_create_autocmd({ "BufUnload", "BufWipeout" }, {
    group = group,
    pattern = PATTERN,
    callback = function(args)
      cancel(args.buf)
      wishes[args.buf] = nil
      if args.event == "BufWipeout" then
        states[args.buf] = nil
      end
    end,
  })
  wrap_jumps()
end

return M
