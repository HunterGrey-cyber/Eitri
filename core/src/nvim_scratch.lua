-- Eitri's scratch buffers. Loaded by one
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
-- A name that starts with `+` or `++` is read by `:edit` as a command or an option even with the structured
-- argument, so only an absolute path is accepted: whoever builds a request has to qualify the name, and a
-- relative one fails here instead of reaching nvim.
local function absolute(path)
  if type(path) ~= 'string' or path:sub(1, 1) ~= '/' then
    error('scratch path is not absolute: ' .. tostring(path))
  end
  return path
end

local function run(cmd, path, mods)
  path = absolute(path)
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

-- A scratch file's whole text, read here rather than by `:edit`, so no modeline in it is ever looked at.
local function read_text(path)
  local f = assert(io.open(absolute(path), 'rb'))
  local text = f:read('a')
  f:close()
  return text
end

-- How `text` ends its lines, by the rule `:split` would have applied with 'fileformats' holding `dos`:
-- `crlf` when every line ends in CR-LF (the CRs are then line ends, not text; a single bare LF anywhere
-- keeps them all), and `eol` when the last line has an end.
local function line_ends(text)
  local dos = vim.tbl_contains(vim.split(vim.o.fileformats, ',', { plain = true }), 'dos')
  local crlf = dos and text:find('\n', 1, true) ~= nil and not text:find('^\n') and not text:find('[^\r]\n')
  return crlf, text:sub(-1) == '\n'
end

-- `text` as buffer lines: CR-LF read as line ends when `crlf`, and the last line's end dropped when `eol`.
local function to_lines(text, crlf, eol)
  if crlf then text = (text:gsub('\r\n', '\n')) end
  if eol and text:sub(-1) == '\n' then text = text:sub(1, -2) end
  return vim.split(text, '\n', { plain = true })
end

-- A scratch file's lines, read as `:split` would read them.
local function read_lines(path)
  local text = read_text(path)
  return to_lines(text, line_ends(text))
end

-- A listed, read-only buffer named after its scratch file, holding `lines`. 'modeline' is off before
-- any text is in it.
local function read_only_buffer(path, lines)
  local buf = vim.api.nvim_create_buf(true, false)
  vim.bo[buf].modeline = false
  vim.bo[buf].swapfile = false
  vim.bo[buf].bufhidden = 'wipe'
  vim.api.nvim_buf_set_name(buf, path)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modified = false
  vim.bo[buf].readonly = true
  vim.bo[buf].modifiable = false
  return buf
end

-- A view holds text the user never chose to treat as a file (a tool's output, a reply), so it is
-- not read the way `:split` reads a file: a modeline in it would be obeyed at read time. The text is
-- read here and put into a buffer whose 'modeline' is already off. The buffer still carries the
-- file's name, so `:ls` says what it is and the editor-context feed knows it as a scratch buffer.
local function view(path)
  path = absolute(path)
  local buf = read_only_buffer(path, read_lines(path))
  vim.api.nvim_open_win(buf, true, { split = 'below', win = -1 })
  -- After the window shows it: a filetype plugin sets window options on the current window.
  vim.bo[buf].filetype = 'markdown'
end

-- A pending edit's proposal: the file as it is on disk (left) against the file as the edit would leave
-- it (right), in a tab page of their own, both read-only, so nothing here can write the real file or
-- touch a buffer the user has. `q` in either closes the tab page. The real name only picks the filetype.
local function diff(req)
  local base_path, proposed_path = absolute(req.base), absolute(req.proposed)
  local base_text, proposed_text = read_text(base_path), read_text(proposed_path)
  local base_crlf, base_eol = line_ends(base_text)
  local proposed_crlf, proposed_eol = line_ends(proposed_text)
  -- Line ends are read away only where both sides agree on them, so a proposal that changes nothing but
  -- its line ends still shows a difference: CR-LF made LF leaves each line's CR on the left (`^M`), and a
  -- final line end added or removed shows as an empty last line on the side that has it.
  -- An empty side (a new file, or everything removed) has no line ends to disagree about.
  if base_text == '' then base_crlf, base_eol = proposed_crlf, proposed_eol end
  if proposed_text == '' then proposed_crlf, proposed_eol = base_crlf, base_eol end
  local crlf, eol = base_crlf and proposed_crlf, base_eol and proposed_eol
  local base = read_only_buffer(base_path, to_lines(base_text, crlf, eol))
  local proposed = read_only_buffer(proposed_path, to_lines(proposed_text, crlf, eol))
  local ft = type(req.name) == 'string' and vim.filetype.match({ filename = req.name }) or nil
  local previous = vim.api.nvim_get_current_tabpage()
  -- A new tab page with one window on `base`; the buffer number is nvim's own integer, never the
  -- request's text.
  vim.cmd(('tab sbuffer %d'):format(base))
  local tab = vim.api.nvim_get_current_tabpage()
  if tab == previous then error('no tab page was opened for the proposal') end
  local left = vim.api.nvim_get_current_win()
  local right = vim.api.nvim_open_win(proposed, true, { split = 'right', win = left })
  for _, pair in ipairs({ { left, base }, { right, proposed } }) do
    local win, buf = pair[1], pair[2]
    vim.api.nvim_win_call(win, function()
      -- After the window shows the buffer: a filetype plugin sets window options on the current window.
      if ft then vim.bo[buf].filetype = ft end
      vim.cmd('diffthis')
    end)
  end
  local function close()
    if not vim.api.nvim_tabpage_is_valid(tab) then return end
    if #vim.api.nvim_list_tabpages() > 1 then
      vim.api.nvim_set_current_tabpage(tab)
      vim.cmd('tabclose')
    else
      -- The proposal's tab is the only one left: closing it would quit nvim, so its buffers go instead.
      -- Diff mode is a window's option and the window that survives shows another buffer next; it is
      -- turned off first, so that buffer is never left diffed against nothing, whichever one nvim picks
      -- and however it carries window options over ('diffoff' also restores the scroll and fold options).
      for _, win in ipairs(vim.api.nvim_tabpage_list_wins(tab)) do
        local shown = vim.api.nvim_win_get_buf(win)
        if shown == base or shown == proposed then
          vim.api.nvim_win_call(win, function() vim.cmd('diffoff') end)
        end
      end
      for _, buf in ipairs({ base, proposed }) do
        if vim.api.nvim_buf_is_valid(buf) then vim.api.nvim_buf_delete(buf, { force = true }) end
      end
    end
  end
  for _, buf in ipairs({ base, proposed }) do
    vim.keymap.set('n', 'q', close, { buffer = buf, nowait = true, silent = true, desc = 'close the proposal' })
  end
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
    elseif req.op == 'diff' then
      diff(req)
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
