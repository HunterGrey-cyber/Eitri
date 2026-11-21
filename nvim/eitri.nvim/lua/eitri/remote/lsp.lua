-- Language servers of a remote project run on its server: `vim.lsp.start` is wrapped so that a server whose root is
-- inside a recorded remote project is started as `eitri host lsp --root <root> -- <cmd...>`, the bridge that speaks LSP
-- on stdio to Neovim and to the helper on the server. One config serves local
-- and remote projects: a root outside every recorded project, a server pinned local, and a `cmd` that is a function are
-- left exactly as they are.
--
-- The idea of putting the server's command behind a wrapper that Neovim runs as its own `cmd` follows remote-ssh.nvim
-- (MIT, b0fc5c8), whose licence notes ship with Eitri.
local M = {}
local api = vim.api
local unpack = table.unpack or unpack

local DEFAULT_GRACE = 1800
local GRACE_MIN, GRACE_MAX = 60, 86400

-- The lookups of the remote module: `recorded(path) -> root|nil`, `program() -> string`, `project_of(path)`.
local host
local settings = { grace = DEFAULT_GRACE, pinned = {} }
-- What `:checkhealth eitri` reports: server name -> the recorded root a server was left local in, for a server that
-- would have run remotely and could not (its `cmd` is a function); name -> true for the ones pinned local; name -> true
-- for the local servers refused on an `eitri-remote://` buffer.
M.function_cmds = {}
M.pinned = {}
M.refused = {}
-- `vim.lsp.start` as it was before the wrap (nil until installed).
M.originals = {}

local function clamp_grace(value)
  if type(value) ~= "number" or value ~= value then
    return DEFAULT_GRACE
  end
  return math.min(math.max(math.floor(value), GRACE_MIN), GRACE_MAX)
end

--- `opts`: `{ grace = <seconds>, local_servers = { <name>, ... } }`; `remote_host` the remote module's lookups. Installs
--- the wrap once; later calls change the options.
function M.setup(opts, remote_host)
  host = remote_host or host
  opts = type(opts) == "table" and opts or {}
  local pinned = {}
  for _, name in ipairs(type(opts.local_servers) == "table" and opts.local_servers or {}) do
    if type(name) == "string" then
      pinned[name] = true
    end
  end
  settings = { grace = clamp_grace(opts.grace), pinned = pinned }
  M.install()
end

-- Whether `word` names the program the bridge is started through: the same string, or the same file on `PATH`. Any
-- other program given `host lsp --root ...` is a command of this machine, whatever its arguments say.
local function is_program(word)
  if type(word) ~= "string" or not host then
    return false
  end
  local ok, program = pcall(host.program)
  if not ok or type(program) ~= "string" then
    return false
  end
  if word == program then
    return true
  end
  local found = vim.fn.exepath(word)
  return found ~= "" and found == vim.fn.exepath(program)
end

--- Whether `cmd` is a bridge's command (`<eitri> host lsp --root <root> ... -- ...`, `<eitri>` the program the remote
--- modules run); the root it names, or nil.
function M.bridge_root(cmd)
  if type(cmd) ~= "table" or cmd[2] ~= "host" or cmd[3] ~= "lsp" or not is_program(cmd[1]) then
    return nil
  end
  for i = 4, #cmd - 1 do
    if cmd[i] == "--" then
      return nil
    elseif cmd[i] == "--root" then
      return type(cmd[i + 1]) == "string" and cmd[i + 1] or nil
    end
  end
  return nil
end

--- The host of a bridge's command: the host of the recorded project its root is in. Nil for any other command.
function M.bridge_host(cmd)
  local root = M.bridge_root(cmd)
  if not root or not host then
    return nil
  end
  local ok, project = pcall(host.project_of, root)
  return ok and type(project) == "table" and project.host or nil
end

local function buffer_of(opts)
  local bufnr = type(opts) == "table" and opts.bufnr or nil
  if bufnr == nil or bufnr == 0 then
    bufnr = api.nvim_get_current_buf()
  end
  return api.nvim_buf_is_valid(bufnr) and bufnr or nil
end

-- Whether `bufnr` is an `eitri-remote://` buffer, and its host. A buffer not read yet has the name only; a name with
-- the scheme that does not parse is still one (it is never a file of this machine), with no host, so no server is
-- allowed for it.
local function remote_buffer(bufnr)
  if not bufnr then
    return false, nil
  end
  local mark = vim.b[bufnr].eitri_remote
  if type(mark) == "table" and type(mark.host) == "string" then
    return true, mark.host
  end
  local name = api.nvim_buf_get_name(bufnr)
  if not name:find("^eitri%-remote:") then
    return false, nil
  end
  local ok, buffers = pcall(require, "eitri.remote.buffers")
  local named = ok and buffers.parse(name) or nil
  return true, named
end

-- A person's switch to keep every server of this buffer (`vim.b.eitri_remote_lsp = false`) or of the session
-- (`vim.g.eitri_remote_lsp = false`) local.
local function told_to_keep_local(bufnr)
  if vim.g.eitri_remote_lsp == false then
    return true
  end
  return bufnr ~= nil and vim.b[bufnr].eitri_remote_lsp == false
end

-- The root `vim.lsp.start` itself would find for `markers`, or nil, searched the way that release's `start` searches.
-- 0.11.2 alone looks for each item in turn upward from the buffer (an item that is a list: the nearest directory
-- holding any of its names), the first found winning. Every other release hands the whole list to `vim.fs.root` in one
-- call: from 0.11.3 that searches each item in turn the same way, and on 0.11.0 and 0.11.1 it takes the nearest
-- directory holding any marker. Nothing but the version tells 0.11.2's loop apart from the call that replaced it.
-- Only for a buffer of a file: nvim starts the search of any other buffer from the working directory, which says
-- nothing of where the buffer's text belongs.
local function marker_root(bufnr, markers)
  if type(markers) ~= "table" or vim.fn.has("nvim-0.11") ~= 1 or vim.bo[bufnr].buftype ~= "" then
    return nil
  end
  local searches = { markers }
  if vim.fn.has("nvim-0.11.2") == 1 and vim.fn.has("nvim-0.11.3") ~= 1 then
    searches = markers
  end
  for _, marker in ipairs(searches) do
    local ok, found = pcall(vim.fs.root, bufnr, marker)
    if ok and type(found) == "string" and found ~= "" then
      return found
    end
  end
  return nil
end

-- The recorded remote root a server of `config` would run in, and the root found by `markers` when that decided it.
-- `config.root_dir` when it is a string; else (nvim 0.11 and later hand `vim.lsp.start` a config whose `root_markers`
-- are not resolved yet) the root those markers find, which must itself lie in a recorded project, and when they find
-- none, the buffer's path.
local function recorded_root(config, bufnr, markers)
  if not host then
    return nil
  end
  local path, found = config.root_dir, nil
  if type(path) ~= "string" or path == "" then
    local name = bufnr and api.nvim_buf_get_name(bufnr) or ""
    if name == "" or name:find("^%a[%w+.-]*://") then
      return nil
    end
    found = marker_root(bufnr, markers)
    path = found or name
  end
  local ok, root = pcall(host.recorded, path)
  if not ok or not root then
    return nil
  end
  return root, found
end

local function strings_only(cmd)
  for _, word in ipairs(cmd) do
    if type(word) ~= "string" then
      return false
    end
  end
  return #cmd > 0
end

--- `config` for a server whose root is inside a recorded remote project, with its `cmd` run through the bridge; any
--- other config is returned as it is. `bufnr` is the buffer the server is started for (default: the current one);
--- `markers` the `root_markers` nvim resolves when `config.root_dir` is unset. Pure: nothing is started, and `config`
--- is not changed (the wrapped config is a copy). Idempotent.
function M.wrap(config, bufnr, markers)
  if type(config) ~= "table" or not host then
    return config
  end
  local cmd = config.cmd
  if M.bridge_root(cmd) then
    return config
  end
  bufnr = bufnr or buffer_of(nil)
  local root, found = recorded_root(config, bufnr, markers)
  if not root then
    return config
  end
  local name = config.name
  if type(name) == "string" and settings.pinned[name] then
    M.pinned[name] = true
    return config
  end
  if told_to_keep_local(bufnr) then
    return config
  end
  if type(cmd) == "function" then
    M.function_cmds[type(name) == "string" and name or "(unnamed)"] = root
    return config
  end
  if type(cmd) ~= "table" or not strings_only(cmd) then
    return config
  end
  local wrapped = {}
  for key, value in pairs(config) do
    wrapped[key] = value
  end
  wrapped.cmd = { host.program(), "host", "lsp", "--root", root, "--grace", tostring(settings.grace), "--" }
  vim.list_extend(wrapped.cmd, cmd)
  -- The environment and working directory of a local command describe this machine, not the server.
  wrapped.cmd_env, wrapped.cmd_cwd = nil, nil
  -- The root the decision was made by is the one the server is given.
  if found then
    wrapped.root_dir = found
  end
  return wrapped
end

--- Whether a server of `config` may be started for the `eitri-remote://` buffer of `buffer_host`: only a bridge to that
--- same host. A server of this machine would read a file of the server, and another host's bridge would be told of
--- it. A buffer whose host cannot be read allows none.
function M.allowed_for_remote_buffer(config, buffer_host)
  return type(buffer_host) == "string" and type(config) == "table" and M.bridge_host(config.cmd) == buffer_host
end

local function note_refused(name)
  M.refused[type(name) == "string" and name or "(unnamed)"] = true
end

-- nvim's own test for reusing a running client, which it does not export: the same name, and (0.11 and later) every
-- workspace folder of the config among the client's, or neither having any; (0.10) the config's `root_dir` among the
-- client's folders.
local function default_reuse(client, config)
  if client.name ~= config.name then
    return false
  end
  local folders_of = vim.lsp._get_workspace_folders
  if type(folders_of) ~= "function" then
    for _, dir in ipairs(config.root_dir and client.workspace_folders or {}) do
      if config.root_dir == dir.name then
        return true
      end
    end
    return false
  end
  if client:is_stopped() then
    return false
  end
  local wanted = folders_of(config.workspace_folders or config.root_dir)
  if not wanted or not next(wanted) then
    local held = folders_of(client.config.workspace_folders or client.config.root_dir)
    return not held or not next(held)
  end
  for _, folder in ipairs(wanted) do
    local found = false
    for _, client_folder in ipairs(client.workspace_folders or {}) do
      if folder.uri == client_folder.uri then
        found = true
        break
      end
    end
    if not found then
      return false
    end
  end
  return true
end

-- `opts` with a `reuse_client` that never hands a buffer to a running client on the other side of the bridge: nvim's
-- test (or the caller's own) compares names and folders, never the command, so a root-less server of a remote project
-- would otherwise take a file of this machine, a local one a file of the project, and a switch or pin that keeps a
-- server local would find its name already running remotely.
local function same_side(opts)
  local out = {}
  for key, value in pairs(type(opts) == "table" and opts or {}) do
    out[key] = value
  end
  local caller = type(out.reuse_client) == "function" and out.reuse_client or nil
  out.reuse_client = function(client, config, ...)
    local held = type(client.config) == "table" and client.config.cmd or nil
    if M.bridge_root(held) ~= M.bridge_root(config.cmd) then
      return false
    end
    if caller then
      return caller(client, config, ...)
    end
    return default_reuse(client, config)
  end
  return out
end

-- After a client attaches to an `eitri-remote://` buffer by a path the wrapper does not see
-- (`vim.lsp.buf_attach_client` after `vim.lsp.start_client`, a `vim.lsp.start` kept from before `setup`), one it would
-- have refused is detached.
-- `LspAttach` fires after the client was sent the buffer: this limits what follows, it does not undo that.
local function guard_attached(args)
  local remote, on = remote_buffer(args.buf)
  local id = type(args.data) == "table" and args.data.client_id or nil
  local client = remote and id and vim.lsp.get_client_by_id(id) or nil
  if not client or M.allowed_for_remote_buffer(client.config, on) then
    return
  end
  note_refused(client.name)
  vim.schedule(function()
    if api.nvim_buf_is_valid(args.buf) and vim.lsp.buf_is_attached(args.buf, id) then
      pcall(vim.lsp.buf_detach_client, args.buf, id)
    end
  end)
end

--- The wrapper of `vim.lsp.start(config, opts)`, installed once.
function M.install()
  if M.originals.start or type(vim.lsp) ~= "table" or type(vim.lsp.start) ~= "function" then
    return
  end
  local original = vim.lsp.start
  M.originals.start = original
  vim.lsp.start = function(config, opts, ...)
    local bufnr = buffer_of(opts)
    local remote, on = remote_buffer(bufnr)
    if remote then
      if not M.allowed_for_remote_buffer(config, on) then
        note_refused(type(config) == "table" and config.name)
        return nil
      end
      return original(config, same_side(opts), ...)
    end
    local markers = type(opts) == "table" and opts._root_markers or nil
    return original(M.wrap(config, bufnr, markers), same_side(opts), ...)
  end
  api.nvim_create_autocmd("LspAttach", {
    group = api.nvim_create_augroup("EitriRemoteLspGuard", { clear = true }),
    callback = guard_attached,
  })
end

return M
