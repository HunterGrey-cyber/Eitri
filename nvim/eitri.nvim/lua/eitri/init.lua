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

-- For a navigator plugin's own edge hook (smart-splits: `at_edge = function(ctx) if not
-- require("eitri").edge(ctx.direction) then ... end end`). False when no panel is attached.
function M.edge(direction)
  local c = rawget(_G, "__eitri_companion")
  if type(c) ~= "table" or type(c.edge) ~= "function" then
    return false
  end
  return c.edge(direction)
end

return M
