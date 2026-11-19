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
          table.insert(roots, root_paths(project.root))
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

local configured_root
local function recorded(path)
  if configured_root then
    return matched_length(configured_root, path) and configured_root.path
  end
  local state = options.state_home or vim.env.XDG_STATE_HOME or ((vim.env.HOME or "") .. "/.local/state")
  local best, length = nil, -1
  for _, root in ipairs(project_roots(state .. "/eitri/hosts")) do
    local n = matched_length(root, path)
    if n and n > length then
      best, length = root.path, n
    end
  end
  return best
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
      if M.current() == s then
        shim(env)
      end
    end, function()
      s.failed_at = uv.now()
      if active_shim and active_shim == s.env then
        shim(nil)
      end
      s.env = nil
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

function M.setup(opts)
  if options and not opts then
    return
  end
  supplied = opts ~= nil
  options = opts or {}
  configured_root = options.root and root_paths(options.root)
  if vim.fn.exists(":EitriSync") ~= 2 then
    api.nvim_create_user_command("EitriSync", M.open, { desc = "Show project sync and resolve conflicts" })
  end
  local function enter()
    local b = api.nvim_get_current_buf()
    local path = api.nvim_buf_get_name(b)
    local root = vim.b[b].eitri_sync_root or recorded(path ~= "" and path or vim.fn.getcwd())
    if root then
      local s = M.attach(root)
      if s then
        shim(s.env)
        require("eitri.remote.sync").mark(s)
      end
    elseif not options.embedded then
      shim(nil)
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
