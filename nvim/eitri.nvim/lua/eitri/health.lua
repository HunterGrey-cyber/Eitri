local M = {}

function M.check()
  local health = vim.health
  health.start("Eitri launcher")
  health.info("Nothing of Eitri runs in this nvim until a command is used.")
  local cmd = require("eitri").program()
  local path = vim.fn.exepath(cmd)
  if path == "" then
    health.error("Could not find program: " .. cmd)
    return
  end
  health.ok("Program: " .. path)
  local ok, version = pcall(vim.fn.systemlist, { cmd, "--version" })
  if ok and vim.v.shell_error == 0 then
    health.ok(table.concat(version, "\n"))
  else
    health.warn("Could not read " .. cmd .. " --version")
  end
  local listed, ids = pcall(vim.fn.systemlist, { cmd, "page", "--list" })
  if listed and vim.v.shell_error == 0 then
    health.ok("eitri page --list works")
    health.info("Page ids: " .. (#ids > 0 and table.concat(ids, ", ") or "(none)"))
  else
    health.warn("eitri page: this Eitri has no page windows yet; update Eitri")
  end
end

return M
