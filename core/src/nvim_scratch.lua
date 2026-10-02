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

-- Every file name reaches nvim as a structured argument, never spliced into an Ex command line: in
-- a command string a newline in the name ends the command and the rest runs as Ex, and escaping
-- does not prevent that. `magic.file = false` also keeps `%`, `#` and wildcards in a name literal.
local function run(cmd, path, mods)
  vim.api.nvim_cmd({ cmd = cmd, args = { path }, mods = mods or {}, magic = { file = false } }, {})
end

local function split(path)
  -- `noswapfile` on the command itself: setting 'swapfile' after the split is too late, the swap
  -- file (and the user's swap directory) would already have been created for a throwaway file.
  run('split', path, { split = 'botright', noswapfile = true })
  local buf = vim.api.nvim_get_current_buf()
  vim.bo[buf].swapfile = false
  vim.bo[buf].bufhidden = 'wipe'
  vim.bo[buf].filetype = 'markdown'
  return buf
end

-- A view holds text the user never chose to treat as a file (a tool's output, a reply), so it is
-- not read the way `:split` reads a file: a modeline in it would be obeyed at read time. The text is
-- read here and put into a buffer whose 'modeline' is already off. The buffer still carries the
-- file's name, so `:ls` says what it is and the editor-context feed knows it as a scratch buffer.
local function view(path)
  local f = assert(io.open(path, 'rb'))
  local text = f:read('a')
  f:close()
  -- The line-end rule `:split` would have applied with 'fileformats' holding `dos`: when every line
  -- ends in CR-LF the CRs are line ends, not text; a single bare LF anywhere keeps them all.
  local dos = vim.tbl_contains(vim.split(vim.o.fileformats, ',', { plain = true }), 'dos')
  if dos and text:find('\n', 1, true) and not text:find('^\n') and not text:find('[^\r]\n') then
    text = (text:gsub('\r\n', '\n'))
  end
  if text:sub(-1) == '\n' then text = text:sub(1, -2) end
  local buf = vim.api.nvim_create_buf(true, false)
  vim.bo[buf].modeline = false
  vim.bo[buf].swapfile = false
  vim.bo[buf].bufhidden = 'wipe'
  vim.api.nvim_buf_set_name(buf, path)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, vim.split(text, '\n', { plain = true }))
  vim.bo[buf].modified = false
  vim.bo[buf].readonly = true
  vim.bo[buf].modifiable = false
  vim.api.nvim_open_win(buf, true, { split = 'below', win = -1 })
  -- After the window shows it: a filetype plugin sets window options on the current window.
  vim.bo[buf].filetype = 'markdown'
end

function M.call(hex)
  local ok, req = pcall(vim.json.decode, unhex(hex))
  if not ok or type(req) ~= 'table' then return end
  local ran, err = pcall(function()
    if req.op == 'open' then
      run('edit', req.path)
      if type(req.line) == 'number' and req.line > 0 then
        pcall(vim.api.nvim_win_set_cursor, 0, { req.line, 0 })
      end
    elseif req.op == 'view' then
      view(req.path)
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
