if type(args.paths) ~= 'table' or #args.paths > 64 then
  return { refused = 'at most 64 LSP targets may be checked' }
end
local out = {}
for _, path in ipairs(args.paths) do
  local buf = find_buf(path)
  if buf and api.nvim_buf_is_loaded(buf) then
    out[#out + 1] = {
      found = true, buf = buf, changedtick = api.nvim_buf_get_changedtick(buf),
      modified = vim.bo[buf].modified, fileformat = vim.bo[buf].fileformat, eol = vim.bo[buf].eol,
      sha256 = vim.fn.sha256(table.concat(api.nvim_buf_get_lines(buf, 0, -1, true), '\n')),
    }
  else
    out[#out + 1] = { found = false }
  end
end
return out
