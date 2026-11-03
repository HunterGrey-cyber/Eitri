local M = {}
local config = { cmd = "eitri", mapping = nil, page_mappings = {} }
local torn = false
local page_ids, completion_leave

function M.teardown()
  torn = true
  page_ids = nil
  if completion_leave then
    pcall(vim.api.nvim_del_autocmd, completion_leave)
    completion_leave = nil
  end
end

function M.program()
  return config.cmd
end

function M.setup(opts)
  config = vim.tbl_extend("force", config, opts or {})
  if config.mapping then
    vim.keymap.set("n", config.mapping, function() M.panel() end, { desc = "Eitri: agent panel" })
  end
  for lhs, id in pairs(config.page_mappings) do
    vim.keymap.set("n", lhs, function() M.page(id) end, { desc = "Eitri: page " .. id })
  end
end

-- Completion asks the installed program, so the launcher keeps no plugin catalogue of its own.
function M.complete_pages(lead, line, pos)
  if torn then return {} end
  if line:sub(1, pos):match("^%s*EitriPage%s+%S+%s+") then
    return vim.fn.getcompletion(lead, "dir")
  end
  if page_ids == nil then
    local ok, ids = pcall(vim.fn.systemlist, { config.cmd, "page", "--list" })
    page_ids = ok and vim.v.shell_error == 0 and ids or {}
    completion_leave = vim.api.nvim_create_autocmd("CmdlineLeave", {
      once = true,
      callback = function()
        if torn then return end
        page_ids, completion_leave = nil, nil
      end,
    })
  end
  local matches = {}
  for _, id in ipairs(page_ids) do
    if id:sub(1, #lead) == lead then table.insert(matches, id) end
  end
  return matches
end

function M.page(id, dir)
  if type(rawget(_G, "__eitri_window")) == "table" then
    vim.notify("this window opens pages itself: press the prefix and the page's key", vim.log.levels.ERROR)
    return
  end
  local addr = vim.v.servername
  if addr == nil or addr == "" then
    addr = vim.fn.serverstart()
  end
  local stderr = {}
  local ok, job = pcall(vim.fn.jobstart, { config.cmd, "page", id, "--nvim", addr, "--", dir or vim.fn.getcwd() }, {
    detach = true,
    stderr_buffered = true,
    on_stderr = function(_, data)
      if torn then return end
      stderr = data or {}
    end,
    on_exit = function(_, code)
      if torn then return end
      if code ~= 0 then
        vim.schedule(function()
          if torn then return end
          local text = vim.trim(table.concat(stderr, "\n"))
          if code == 64 and text:find("unknown option --nvim", 1, true) then
            vim.notify("eitri page: this Eitri has no page windows yet; update Eitri", vim.log.levels.ERROR)
            return
          end
          text = text:match("([^\n]+)$") or ("exited with code " .. code)
          if not text:find("^eitri page:") then text = "eitri page: " .. text end
          vim.notify(text, vim.log.levels.ERROR)
        end)
      end
    end,
  })
  if not ok or job <= 0 then
    vim.notify("eitri: could not run `" .. config.cmd .. "`", vim.log.levels.ERROR)
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
    on_stderr = function(_, data)
      if torn then return end
      stderr = data or {}
    end,
    on_exit = function(_, code)
      if torn then return end
      if code ~= 0 then
        vim.schedule(function()
          if torn then return end
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

-- Asks Eitri to put the keys in the chat with the Visual selection attached and the edit prompt ready, as
-- `<leader>ae` does in Visual mode where nothing of yours holds that key (`vim.keymap.set("x", "<leader>k",
-- function() require("eitri").edit_selection() end)`). False when no Eitri window or panel is attached to this nvim.
function M.edit_selection()
  local ask = rawget(_G, "__eitri_edit_selection")
  if type(ask) ~= "function" then
    return false
  end
  return ask() == true
end

return M
