-- A query needs no installed review module and never opens a buffer or changes a window.
local paths = ...
local shown = {}
local visible = {}
for _, win in ipairs(vim.api.nvim_list_wins()) do
  local config = vim.api.nvim_win_get_config(win)
  if config.relative == '' and not config.external then
    visible[vim.api.nvim_win_get_buf(win)] = true
  end
end
for i = 1, math.min(#paths, 256) do
  local buf = find_buf(paths[i])
  shown[i] = buf ~= nil and vim.api.nvim_buf_is_loaded(buf) and visible[buf] == true
end
return { shown = shown }
