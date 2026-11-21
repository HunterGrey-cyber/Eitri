local M = {}

-- Every child process the checks run is bounded: `:checkhealth` may wait a moment, never forever.
local TIMEOUT_MS = 4000
local SHOWN_MAX = 200

--- Text that came from outside (a path, a client's name, a program's output) as it is shown: every hidden character
--- written out as `\u{<hex>}` (`eitri.remote.text`), so nothing in it can move the cursor, recolour the buffer, end the
--- line or read as other text, and it is cut short.
local function shown(text)
  text = tostring(text)
  local cut = #text > SHOWN_MAX
  if cut then
    text = text:sub(1, SHOWN_MAX)
  end
  text = require("eitri.remote.text").escape(text)
  return cut and text .. "..." or text
end

--- Runs `argv` and returns its trimmed output and exit code, or nil and a reason (it could not start, or it did not
--- finish in time and was killed).
local function run(argv)
  local started, job = pcall(vim.system, argv, { text = true })
  if not started then
    return nil, tostring(job)
  end
  local result = job:wait(TIMEOUT_MS)
  if result.code == 124 and result.signal ~= 0 then
    return nil, "no answer within " .. TIMEOUT_MS / 1000 .. " seconds"
  end
  return vim.trim((result.stdout or "") .. (result.stderr or "")), result.code
end

local function launcher(cmd)
  local health = vim.health
  health.start("Eitri launcher")
  health.info("Opening a recorded remote project starts its sync watch. Use :EitriSync to inspect it.")
  local path = vim.fn.exepath(cmd)
  if path == "" then
    health.error("Could not find program: " .. shown(cmd))
    return
  end
  health.ok("Program: " .. shown(path))
  local out, code = run({ cmd, "--version" })
  if out and code == 0 then
    health.ok(shown(out))
  else
    health.warn("Could not read " .. shown(cmd) .. " --version")
  end
  local ids, listed = run({ cmd, "page", "--list" })
  if ids and listed == 0 then
    health.ok("eitri page --list works")
    health.info("Page ids: " .. (ids ~= "" and shown(table.concat(vim.split(ids, "\n"), ", ")) or "(none)"))
  else
    health.warn("eitri page: this Eitri has no page windows yet; update Eitri")
  end
end

-- The buffer `:checkhealth` was typed in: the report buffer is current while a check runs, and the buffer the person
-- came from is its alternate.
local function checked_buffer()
  local bufnr = vim.api.nvim_get_current_buf()
  if vim.api.nvim_buf_get_name(bufnr):find("^health://") then
    local alternate = vim.fn.bufnr("#")
    return alternate > 0 and vim.api.nvim_buf_is_valid(alternate) and alternate or nil
  end
  return bufnr
end

-- The path that says which project the check is about: the buffer's own, else the working directory (an unnamed buffer
-- or one with a URI for a name has no path).
local function checked_path()
  local bufnr = checked_buffer()
  local name = bufnr and vim.api.nvim_buf_get_name(bufnr) or ""
  if name == "" or name:find("^%a[%w+.-]*://") then
    return vim.fn.getcwd(), bufnr
  end
  return name, bufnr
end

-- Whether `eitri host` lets a project be opened, as `eitri host status` says it: its last line of that form is
-- `remote development: on` or `off` (the verb reads only this machine's records and never connects).
local function remote_dev_enabled(cmd)
  local out, why = run({ cmd, "host", "status" })
  if not out then
    return nil, why
  end
  local said
  for line in vim.gsplit(out, "\n", { plain = true }) do
    said = line:match("^remote development: (%a+)$") or said
  end
  if said == "on" or said == "off" then
    return said == "on"
  end
  return nil, out
end

-- Whether `version` is older than 0.18.0, the oldest Mutagen Eitri accepts (a prerelease of 0.18.0 is older than it).
local function mutagen_too_old(version)
  local major, minor, patch, rest = version:match("^v?(%d+)%.(%d+)%.(%d+)(.*)$")
  if not major then
    return false
  end
  major, minor, patch = tonumber(major), tonumber(minor), tonumber(patch)
  if major == 0 and minor == 18 and patch == 0 then
    return rest:sub(1, 1) == "-"
  end
  return major == 0 and minor < 18
end

local function language_servers(health, remote_lsp, root, bufnr)
  local attached = bufnr and vim.lsp.get_clients({ bufnr = bufnr }) or {}
  if #attached == 0 then
    health.info("No language server is attached to this buffer")
  end
  local named = {}
  for _, client in ipairs(attached) do
    local name = shown(client.name or "(unnamed)")
    named[client.name or "(unnamed)"] = true
    local cmd = client.config and client.config.cmd
    local bridge = remote_lsp.bridge_root(cmd)
    if bridge then
      health.ok(name .. ": remote, runs on the server through the bridge (root " .. shown(bridge) .. ")")
    elseif type(cmd) == "function" then
      health.warn(
        name .. ": local, its cmd is a function that builds its own transport, so Eitri cannot send it to the server",
        { "Give the server a cmd that is a list of strings, or accept that it works on the laptop's copy" }
      )
    elseif remote_lsp.pinned[client.name or ""] then
      health.info(name .. ": local, pinned (it is named in the local servers option)")
    else
      health.info(name .. ": local (not started through the bridge)")
    end
  end
  -- A server left local for its function cmd that is not attached to this buffer is named too.
  local left = {}
  for name, where in pairs(remote_lsp.function_cmds) do
    if where == root and not named[name] then
      table.insert(left, name)
    end
  end
  table.sort(left)
  for _, name in ipairs(left) do
    health.warn(shown(name) .. ": local, its cmd is a function, so Eitri cannot send it to the server")
  end
  local refused = vim.tbl_keys(remote_lsp.refused)
  table.sort(refused)
  if #refused > 0 then
    health.info("Local servers refused on server-only buffers: " .. shown(table.concat(refused, ", ")))
  end
end

local function remote()
  local health = vim.health
  health.start("Eitri remote project")
  local remote_module = require("eitri.remote")
  local cmd = remote_module.program()
  local path, bufnr = checked_path()
  local root = remote_module.root_of(path)
  if not root then
    health.info("This buffer is outside every recorded remote project (open one with eitri host open <host>:<path>)")
    return
  end
  local project = remote_module.project_of(path)
  health.ok("Remote project: " .. shown(project and project.host or "(host unknown)") .. ":" .. shown(root))
  local exe = vim.fn.exepath(cmd)
  if exe == "" then
    health.error("eitri program not found: " .. shown(cmd))
  else
    local version, code = run({ cmd, "--version" })
    if version and code == 0 then
      health.ok("eitri: " .. shown(exe) .. ", " .. shown(version))
    else
      health.warn("eitri: " .. shown(exe) .. ", but its version could not be read")
    end
    local enabled, why = remote_dev_enabled(cmd)
    if enabled == true then
      health.ok("remote_dev.enabled is on (as eitri host reads it)")
    elseif enabled == false then
      health.warn("remote_dev.enabled is off: eitri host open refuses", {
        'Set eitri.config.set("remote_dev.enabled", true) in Eitri\'s init.lua',
      })
    else
      health.warn("could not tell whether remote_dev.enabled is on: " .. shown(why))
    end
  end
  local loaded, remote_lsp = pcall(require, "eitri.remote.lsp")
  if loaded then
    language_servers(health, remote_lsp, root, bufnr)
  else
    health.warn("eitri.remote.lsp could not be loaded: " .. shown(remote_lsp))
  end
  local mutagen = vim.fn.exepath("mutagen")
  if mutagen == "" then
    health.warn("mutagen was not found on PATH", {
      "Install Mutagen 0.18.0 or newer, or set remote_dev.mutagen to its absolute path in Eitri's init.lua",
    })
    return
  end
  local out, code = run({ mutagen, "version" })
  if not out or code ~= 0 then
    health.warn("mutagen: " .. shown(mutagen) .. ", but its version could not be read")
  elseif mutagen_too_old(out) then
    health.warn("mutagen: " .. shown(mutagen) .. ", version " .. shown(out) .. " is older than the 0.18.0 Eitri needs")
  else
    health.ok("mutagen: " .. shown(mutagen) .. ", version " .. shown(out))
  end
end

function M.check()
  launcher(require("eitri").program())
  remote()
end

return M
