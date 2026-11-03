-- These commands launch Eitri beside this nvim. Each window installs its own editor glue,
-- so this file and its module never have to match Eitri's version.
if vim.g.loaded_eitri then
  return
end
vim.g.loaded_eitri = 1
vim.api.nvim_create_user_command("EitriPanel", function(o)
  require("eitri").panel(o.args ~= "" and o.args or nil)
end, { nargs = "?", complete = "dir", desc = "Open Eitri's agent panel beside this nvim" })
vim.api.nvim_create_user_command("EitriPage", function(o)
  if #o.fargs == 0 then
    vim.notify(":EitriPage needs a plugin id, for example :EitriPage markdown", vim.log.levels.ERROR)
  elseif #o.fargs > 2 then
    vim.notify(":EitriPage takes a plugin id and an optional directory", vim.log.levels.ERROR)
  else
    require("eitri").page(o.fargs[1], o.fargs[2])
  end
end, {
  nargs = "*",
  complete = function(lead, line, pos) return require("eitri").complete_pages(lead, line, pos) end,
  desc = "Open an Eitri page beside this nvim",
})
