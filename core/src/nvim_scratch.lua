-- Eitri's scratch buffers (keymap/tabs spec §4.2, R3 and C5; phase 3 ruling 18). Loaded by one
-- `--cmd` like editor_context.lua. shell calls EitriScratch.call('<hex>') through nvim_input's
-- <Cmd>; the hex is a JSON request, so nothing the request carries is ever read as keys.
-- Injected over RPC, it is given no options and returns the function that removes the global.
local M = {}

local function unhex(h)
  return (h:gsub('..', function(c) return string.char(tonumber(c, 16)) end))
end

-- Written beside and renamed into place: the panel polls `done` every tick, and an `io.open(done,
-- 'w')` would let it read the file empty between the open and the close.
local function mark(done, text)
  if not done then return end
  local tmp = done .. '.tmp'
  local f = io.open(tmp, 'w')
  if not f then return end
  f:write(text)
  f:close()
  if not os.rename(tmp, done) then os.remove(tmp) end
end

local function split(path)
  -- `noswapfile` on the command itself: setting 'swapfile' after the split is too late, the swap
  -- file (and the user's swap directory) would already have been created for a throwaway file.
  vim.cmd('botright noswapfile split ' .. vim.fn.fnameescape(path))
  local buf = vim.api.nvim_get_current_buf()
  vim.bo[buf].swapfile = false
  vim.bo[buf].bufhidden = 'wipe'
  vim.bo[buf].filetype = 'markdown'
  return buf
end

function M.call(hex)
  local ok, req = pcall(vim.json.decode, unhex(hex))
  if not ok or type(req) ~= 'table' then return end
  local ran, err = pcall(function()
    if req.op == 'open' then
      vim.cmd('edit ' .. vim.fn.fnameescape(req.path))
      if type(req.line) == 'number' and req.line > 0 then
        pcall(vim.api.nvim_win_set_cursor, 0, { req.line, 0 })
      end
    elseif req.op == 'view' then
      local buf = split(req.path)
      vim.bo[buf].readonly = true
      vim.bo[buf].modifiable = false
    elseif req.op == 'edit' then
      local buf = split(req.path)
      local written = false
      vim.api.nvim_create_autocmd('BufWritePost', { buffer = buf, callback = function() written = true end })
      vim.api.nvim_create_autocmd('BufWipeout', {
        buffer = buf,
        once = true,
        callback = function() mark(req.done, written and 'written' or 'discarded') end,
      })
    else
      error('unknown scratch op: ' .. tostring(req.op))
    end
  end)
  -- Known limit: `open` and `view` carry no `done`, so a failure of theirs (E37 from `:edit` over a
  -- modified buffer with 'nohidden', say) reaches only nvim's message area; the panel said `ok`.
  if not ran then mark(req.done, 'error: ' .. tostring(err)) end
end

EitriScratch = M

return function()
  if rawget(_G, "EitriScratch") == M then
    EitriScratch = nil
  end
end
