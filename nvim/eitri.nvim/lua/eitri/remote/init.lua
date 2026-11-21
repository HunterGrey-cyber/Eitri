local M = {}
local api, uv = vim.api, vim.uv or vim.loop
local sessions, options = {}, nil
local active_shim, previous_env, supplied

local function under(root, path)
  return path == root or path:sub(1, #root + 1) == root .. "/" or root == "/"
end

local records = {}
local function root_paths(path)
  return { path = path, real = uv.fs_realpath(path) }
end
local function project_roots(hosts)
  local dir = uv.fs_stat(hosts)
  if not dir then
    return {}
  end
  local stamp = { dir.ino, dir.mtime.sec, dir.mtime.nsec }
  if records.path == hosts and vim.deep_equal(records.stamp, stamp) then
    return records.roots
  end
  local roots = {}
  local listing = uv.fs_scandir(hosts)
  while listing do
    local name = uv.fs_scandir_next(listing)
    if not name then
      break
    end
    if #name == 21 and name:match("^%x+%.json$") then
      local file = hosts .. "/" .. name
      local st = uv.fs_lstat(file)
      if st and st.type == "file" and st.size <= 16 * 1024 * 1024 then
        local ok, project = pcall(function()
          return vim.json.decode(table.concat(vim.fn.readfile(file), "\n"))
        end)
        if
          ok
          and type(project) == "table"
          and project.version == 1
          and type(project.host) == "string"
          and type(project.root) == "string"
          and project.root:sub(1, 1) == "/"
        then
          local root = root_paths(project.root)
          root.host = project.host
          table.insert(roots, root)
        end
      end
    end
  end
  records = { path = hosts, stamp = stamp, roots = roots }
  return roots
end

local function matched_length(root, path)
  if under(root.path, path) then return #root.path end
  if root.real and under(root.real, path) then return #root.real end
end

local function hosts_dir()
  local state = options.state_home or vim.env.XDG_STATE_HOME or ((vim.env.HOME or "") .. "/.local/state")
  return state .. "/eitri/hosts"
end

local configured_root
local function recorded(path)
  if configured_root then
    return matched_length(configured_root, path) and configured_root.path
  end
  local best, length = nil, -1
  for _, root in ipairs(project_roots(hosts_dir())) do
    local n = matched_length(root, path)
    if n and n > length then
      best, length = root.path, n
    end
  end
  return best
end

--- The recorded remote project `path` belongs to: `{ root = <its root>, host = <the host as typed> }`, or nil.
function M.project_of(path)
  local root = recorded(path)
  if not root then
    return nil
  end
  -- A recorded project's own host wins; `options.host` names the host of a configured root no record holds.
  local host
  for _, recorded_root in ipairs(project_roots(hosts_dir())) do
    if recorded_root.path == root then
      host = recorded_root.host
      break
    end
  end
  return { root = root, host = host or options.host }
end

local keys =
  { "EITRI_REAL_GIT", "EITRI_GIT_SHIM_EXE", "EITRI_GIT_SHIM_OPTIONS", "EITRI_GIT_SHIM_OPTIONS_FILE" }
local function shim(env)
  if options.embedded or active_shim == env then
    return
  end
  if active_shim then
    local paths = vim.split(vim.env.PATH or "", ":", { plain = true })
    for i, p in ipairs(paths) do
      if p == active_shim.PATH then
        table.remove(paths, i)
        break
      end
    end
    vim.env.PATH = table.concat(paths, ":")
    for _, key in ipairs(keys) do
      vim.env[key] = previous_env[key]
    end
  end
  active_shim = env
  if not env then
    return
  end
  previous_env = {}
  for _, key in ipairs(keys) do
    if supplied then
      previous_env[key] = (options.previous_env or {})[key]
    else
      previous_env[key] = vim.env[key]
    end
    vim.env[key] = env[key]
  end
  vim.env.PATH = env.PATH .. ":" .. (vim.env.PATH or "")
end

function M.current()
  local b = api.nvim_get_current_buf()
  local root = vim.b[b].eitri_sync_root
  if not root then
    local name = api.nvim_buf_get_name(b)
    root = recorded(name ~= "" and name or vim.fn.getcwd())
  end
  return sessions[root]
end

function M.attach(root, retry)
  local s = sessions[root]
  if not s then
    s = { root = root, real_root = uv.fs_realpath(root), cmd = options.cmd or require("eitri").program(), notices = {} }
    sessions[root] = s
  end
  if not s.job and (retry or not s.failed_at or uv.now() - s.failed_at >= 60000) then
    local sync = require("eitri.remote.sync")
    sync.start(s, function(env)
      if not active_shim or M.current() == s then
        shim(env)
      end
    end, function()
      s.failed_at = uv.now()
      local expired = s.env
      s.env = nil
      if expired and active_shim == expired then
        for _, other in pairs(sessions) do
          if other.env then
            shim(other.env)
            return
          end
        end
        shim(nil)
      end
    end)
  end
  return sessions[root]
end

function M.status()
  local s = M.current()
  if not s or not s.status then
    return ""
  end
  return require("eitri.remote.sync").label(s.status)
end

function M.open()
  local s = M.current()
  local name = api.nvim_buf_get_name(0)
  local root = s and s.root or recorded(name ~= "" and name or vim.fn.getcwd())
  if root then
    s = M.attach(root, true)
  end
  if s then
    require("eitri.remote.sync").open(s)
  else
    vim.notify("This buffer is outside a recorded remote project")
  end
end

--- The recorded remote root `path` belongs to, or nil: the lookup the sync, the exec wrappers and the language-server
--- wrap all use.
function M.root_of(path)
  if not options then
    M.setup()
  end
  return recorded(path)
end

--- The `eitri` program the remote modules run: setup's `cmd`, else the launcher's.
function M.program()
  if not options then
    M.setup()
  end
  return options.cmd or require("eitri").program()
end

-- The wrappers need only the recorded-root lookup, the program and the project's host; passing them keeps their modules
-- free of state.
local function lookup_host()
  if not options then
    M.setup()
  end
  return {
    recorded = recorded,
    project_of = M.project_of,
    program = M.program,
  }
end

function M.cmd(argv, opts)
  return require("eitri.remote.exec").cmd(lookup_host(), argv, opts)
end

-- `:EitriRemote {name} ...`: each module adds its own entry, so the command exists once however many modules have
-- something to offer it. An entry is { run = function(words, rest), complete = function(arglead) -> list|nil }: `words`
-- are the words after the name, `rest` the rest of the line as typed (for an argument that may hold blanks).
M.subcommands = {}
M.subcommands.makeprg = {
  run = function(args)
    require("eitri.remote.exec").makeprg(lookup_host(), args)
  end,
}

M.subcommands.term = {
  run = function()
    require("eitri.remote.exec").term(lookup_host())
  end,
}

local command_defined = false
local function define_command()
  if command_defined then
    return
  end
  command_defined = true
  local function names(lead)
    local out = {}
    for name in pairs(M.subcommands) do
      if vim.startswith(name, lead) then
        table.insert(out, name)
      end
    end
    table.sort(out)
    return out
  end
  api.nvim_create_user_command("EitriRemote", function(o)
    local name, rest = o.args:match("^%s*(%S+)%s*(.-)%s*$")
    local sub = name and M.subcommands[name]
    if not sub then
      error(
        ("EitriRemote: unknown subcommand %s; expected one of: %s"):format(
          name or "(none)",
          table.concat(names(""), ", ")
        ),
        0
      )
    end
    -- `vim.cmd.EitriRemote("open /a b")` passes the whole line as one argument: then the words come from the line.
    local words = o.fargs[1] == name and vim.list_slice(o.fargs, 2) or vim.split(rest, "%s+", { trimempty = true })
    sub.run(words, rest)
  end, {
    nargs = "*",
    desc = "Eitri remote project commands",
    complete = function(lead, line)
      local words = vim.split(vim.trim(line), "%s+")
      if #words <= 1 or (#words == 2 and line:sub(-1) ~= " ") then
        return names(lead)
      end
      local sub = M.subcommands[words[2]]
      return sub and sub.complete and sub.complete(lead) or {}
    end,
  })
end

function M.setup(opts)
  if options and not opts then
    return
  end
  if options and options.embedded then
    -- In Eitri's window the window's own call came first: a shared configuration's call keeps everything it set (the
    -- project root, the program, `embedded`, the language-server options from Eitri's init.lua) and adds the rest.
    opts = vim.tbl_extend("keep", options, type(opts) == "table" and opts or {})
  end
  supplied = opts ~= nil
  options = opts or {}
  configured_root = options.root and root_paths(options.root)
  if vim.fn.exists(":EitriSync") ~= 2 then
    api.nvim_create_user_command("EitriSync", M.open, { desc = "Show project sync and resolve conflicts" })
  end
  local loaded, buffers = pcall(require, "eitri.remote.buffers")
  if loaded then
    buffers.setup({ cmd = options.cmd })
    M.subcommands.open = {
      run = function(_, rest)
        buffers.open(rest)
      end,
    }
  else
    vim.notify("eitri.remote.buffers could not be loaded: " .. tostring(buffers), vim.log.levels.WARN)
  end
  local lsp_loaded, lsp = pcall(require, "eitri.remote.lsp")
  if lsp_loaded then
    lsp.setup(options.lsp, lookup_host())
  else
    vim.notify("eitri.remote.lsp could not be loaded: " .. tostring(lsp), vim.log.levels.WARN)
  end
  define_command()
  local function enter()
    local b = api.nvim_get_current_buf()
    local path = api.nvim_buf_get_name(b)
    local root = vim.b[b].eitri_sync_root or recorded(path ~= "" and path or vim.fn.getcwd())
    if root then
      local s = M.attach(root)
      if s then
        if s.env then shim(s.env) end
        require("eitri.remote.sync").mark(s)
      end
    end
  end
  local group = api.nvim_create_augroup("EitriRemote", { clear = true })
  api.nvim_create_autocmd({ "BufEnter", "VimEnter" }, { group = group, callback = enter })
  api.nvim_create_autocmd(
    {
      "BufModifiedSet",
      "BufWritePost",
      "BufReadPost",
      "BufUnload",
      "BufFilePost",
      "TextChanged",
      "TextChangedI",
    },
    {
      group = group,
      callback = function()
        vim.schedule(function()
          for _, s in pairs(sessions) do
            require("eitri.remote.sync").report(s, false)
          end
        end)
      end,
    }
  )
  api.nvim_create_autocmd("VimLeavePre", {
    group = group,
    callback = function()
      for _, s in pairs(sessions) do
        if s.job and s.job > 0 then
          pcall(vim.fn.chanclose, s.job, "stdin")
        end
      end
    end,
  })
  enter()
end
return M
