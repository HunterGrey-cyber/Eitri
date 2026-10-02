-- :EitriPanel opens Eitri's agent panel beside this nvim, or attaches the one already open for this
-- project. Everything the panel needs inside nvim it installs itself, so this file and its module
-- never have to match Eitri's version.
if vim.g.loaded_eitri then
  return
end
vim.g.loaded_eitri = 1
vim.api.nvim_create_user_command("EitriPanel", function(o)
  require("eitri").panel(o.args ~= "" and o.args or nil)
end, { nargs = "?", complete = "dir", desc = "Open Eitri's agent panel beside this nvim" })
