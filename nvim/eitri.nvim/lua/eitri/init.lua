local M = {}
local config = { cmd = "eitri", mapping = nil }

function M.setup(opts)
  config = vim.tbl_extend("force", config, opts or {})
  if config.mapping then
    vim.keymap.set("n", config.mapping, function() M.panel() end, { desc = "Eitri: agent panel" })
  end
end

function M.panel(dir)
  local addr = vim.v.servername
  if addr == nil or addr == "" then
    addr = vim.fn.serverstart()
  end
  -- Read only until the panel starts: from then on it writes to its own log, and its last line here
  -- says where, which is what a later failure is reported with.
  local stderr = {}
  local ok, job = pcall(vim.fn.jobstart, { config.cmd, "panel", "--nvim", addr, "--", dir or vim.fn.getcwd() }, {
    detach = true,
    stderr_buffered = true,
    on_stderr = function(_, data) stderr = data or {} end,
    on_exit = function(_, code)
      if code ~= 0 then
        vim.schedule(function()
          local text = vim.trim(table.concat(stderr, "\n"))
          if text == "" then
            text = "exited with code " .. code
          end
          vim.notify("eitri panel: " .. text, vim.log.levels.ERROR)
        end)
      end
    end,
  })
  if not ok or job <= 0 then
    vim.notify("eitri: could not run `" .. config.cmd .. "`", vim.log.levels.ERROR)
  end
end

-- The Eitri window's own nvim has `_G.__eitri_window` (`:Eitri` is its command form); a nvim started outside
-- one has neither it nor the companion's table.
local function window()
  local w = rawget(_G, "__eitri_window")
  if type(w) == "table" then
    return w
  end
  return nil
end

-- Runs one of the window's actions from Lua, like `:Eitri <word>...` (`require("eitri").run("split", "right",
-- "agent")`). False when this nvim is not an Eitri window's editor or the line could not be sent.
function M.run(...)
  local w = window()
  if w == nil or type(w.run) ~= "function" then
    return false
  end
  return w.run(...) == true
end

-- For a navigator plugin's own edge hook (smart-splits: `at_edge = function(ctx) if not
-- require("eitri").edge(ctx.direction) then ... end end`). Tries the attached panel first (this nvim is a
-- terminal nvim an `eitri panel` is attached to), then the Eitri window this nvim is the editor of. False when
-- neither took the move.
function M.edge(direction)
  local c = rawget(_G, "__eitri_companion")
  if type(c) == "table" and type(c.edge) == "function" and c.edge(direction) then
    return true
  end
  local w = window()
  if w ~= nil and type(w.edge) == "function" then
    return w.edge(direction) == true
  end
  return false
end

return M
