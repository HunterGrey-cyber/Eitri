-- Eitri's scratch buffers: the read-only row view and the draft editor. Loaded by one
-- `--cmd` like editor_context.lua. shell calls EitriScratch.call('<hex>') through nvim_input's
-- <Cmd>; the hex is a JSON request, so nothing the request carries is ever read as keys.
-- Injected over RPC, it accepts an options table and returns the teardown for its windows, buffers and global.
local opts = ...
local M = {}
local torn = false
local buffers, windows = {}, {}
local syntax_before, syntax_set

local function unhex(h)
  return (h:gsub('..', function(c) return string.char(tonumber(c, 16)) end))
end

-- Written beside and renamed into place: the panel polls `done` every tick, and an `io.open(done,
-- 'w')` would let it read the file empty between the open and the close.
local function mark(done, text, body)
  if not done then return end
  local tmp = done .. '.tmp'
  local f, err = io.open(tmp, 'w')
  if f then
    local wrote, write_err = f:write(text)
    local closed, close_err = f:close()
    if not wrote then err = write_err
    elseif not closed then err = close_err
    else
      local renamed, rename_err = os.rename(tmp, done)
      if renamed then return end
      err = rename_err
    end
  end
  os.remove(tmp)
  vim.notify('draft could not be handed back: ' .. tostring(err) ..
    '; saved body: ' .. tostring(body), vim.log.levels.ERROR)
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

-- A path that is not on this machine: `eitri-remote://<host>/<path>`, which `eitri.nvim` reads from the server into a
-- read-only buffer. Only `open` takes one, and only as the scheme and nothing before it (so never `+cmd` or `++opt`).
local REMOTE = 'eitri-remote://'
local function remote(path)
  return type(path) == 'string' and path:sub(1, #REMOTE) == REMOTE
end

local function run(cmd, path, mods)
  path = absolute(path)
  vim.api.nvim_cmd({ cmd = cmd, args = { path }, mods = mods or {}, magic = { file = false } }, {})
end

local function split(path)
  -- `noswapfile` on the command itself: setting 'swapfile' after the split is too late, the swap
  -- file (and the user's swap directory) would already have been created for a throwaway file.
  -- Disable modelines for the read itself, then restore the person's option even if opening fails.
  local previous = vim.api.nvim_get_current_buf()
  local modeline, global_modeline = vim.bo[previous].modeline, vim.go.modeline
  vim.o.modeline = false
  local ok, err = pcall(run, 'split', path, { split = 'botright', noswapfile = true })
  vim.go.modeline = global_modeline
  if vim.api.nvim_buf_is_valid(previous) then vim.bo[previous].modeline = modeline end
  if not ok then error(err) end
  local buf = vim.api.nvim_get_current_buf()
  vim.bo[buf].modeline = false
  vim.bo[buf].swapfile = false
  vim.bo[buf].bufhidden = 'wipe'
  vim.bo[buf].filetype = 'markdown'
  buffers[buf] = true
  windows[vim.api.nvim_get_current_win()] = buf
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
  buffers[buf] = true
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

-- The window and buffer of the view shown last. A new view takes that window while it still shows that view, as
-- vim's preview window does, so views do not pile up as splits; a window the person has since used for something
-- else, or one in another tab page, is not reused.
local last_view = nil

-- Tool output and replies must not execute modelines at read time: put their text into a buffer
-- whose 'modeline' is already off. Keep the file's name for `:ls` and scratch-buffer recognition
-- by the editor-context feed.
local function view(path)
  path = absolute(path)
  local buf = read_only_buffer(path, read_lines(path))
  local win = last_view and last_view.win
  if win and vim.api.nvim_win_is_valid(win) and vim.api.nvim_win_get_buf(win) == last_view.buf
      and vim.api.nvim_win_get_tabpage(win) == vim.api.nvim_get_current_tabpage() then
    -- The previous view's buffer is wiped when the window stops showing it.
    vim.api.nvim_win_set_buf(win, buf)
    vim.api.nvim_set_current_win(win)
  else
    win = vim.api.nvim_open_win(buf, true, { split = 'below', win = -1 })
  end
  -- After the window shows it: a filetype plugin sets window options on the current window.
  vim.bo[buf].filetype = 'markdown'
  windows[win] = buf
  last_view = { win = win, buf = buf }
end

-- A conversation snapshot gets a fresh tab and no answer keys. Closing its last tab wipes only
-- the snapshot buffer, leaving nvim running.
local function view_tab(path)
  path = absolute(path)
  local buf = read_only_buffer(path, read_lines(path))
  local previous = vim.api.nvim_get_current_tabpage()
  vim.cmd(('tab sbuffer %d'):format(buf))
  local tab = vim.api.nvim_get_current_tabpage()
  if tab == previous then error('no tab page was opened for the view') end
  windows[vim.api.nvim_get_current_win()] = buf
  vim.bo[buf].filetype = 'markdown'
  vim.keymap.set('n', 'q', function()
    if not vim.api.nvim_tabpage_is_valid(tab) then return end
    if #vim.api.nvim_list_tabpages() > 1 then
      vim.api.nvim_set_current_tabpage(tab)
      vim.cmd('tabclose')
    elseif vim.api.nvim_buf_is_valid(buf) then
      vim.api.nvim_buf_delete(buf, { force = true })
    end
  end, { buffer = buf, nowait = true, silent = true, desc = 'close the conversation view' })
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
  windows[left], windows[right] = base, proposed
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

-- The units a language server counts a column in: bytes, UTF-16 code units, code points.
local units = { ['utf-8'] = true, ['utf-16'] = true, ['utf-32'] = true }
local byteindex_takes_encoding = vim.fn.has('nvim-0.11') == 1

-- The byte column on `text` of the one-based column `col` counted in `unit`. The column names the character holding
-- its unit: one inside a character (the second half of a surrogate pair, a byte inside a multibyte character) lands
-- on that character's first byte, and one past the line's end on its last character. vim.str_byteindex answers with
-- the byte just past that character; before 0.11 it took (s, index, use_utf16) and raised an error past the end,
-- since 0.11 it takes (s, encoding, index, strict_indexing) and, not strict, answers the line's length there.
local function byte_column(text, col, unit)
  if #text == 0 then return 0 end
  local after = #text
  if unit == 'utf-8' then
    after = math.min(col, #text)
  else
    local found, at
    if byteindex_takes_encoding then
      found, at = pcall(vim.str_byteindex, text, unit, col, false)
    else
      found, at = pcall(vim.str_byteindex, text, col, unit == 'utf-16')
    end
    if found and type(at) == 'number' and at >= 1 then after = math.min(at, #text) end
  end
  -- `after` is the one-based index of the character's last byte; str_utf_start steps back to its first.
  return after - 1 + vim.str_utf_start(text, after)
end

-- The column comes from the page by way of the host, which judged it already; it is judged again here, as data
-- only: a whole number from 1 below 2^31 and a unit of the three, else the cursor stays at the line's start.
local function place_column(line, col, unit)
  if type(col) ~= 'number' or col < 1 or col >= 2 ^ 31 or col ~= math.floor(col) then return end
  if type(unit) ~= 'string' or not units[unit] then return end
  local text = vim.api.nvim_buf_get_lines(0, line - 1, line, false)[1]
  if text == nil then return end
  pcall(vim.api.nvim_win_set_cursor, 0, { line, byte_column(text, col, unit) })
end

function M.call(hex)
  if torn then return end
  local ok, req = pcall(vim.json.decode, unhex(hex))
  if not ok or type(req) ~= 'table' then return end
  local previous_syntax = vim.go.syntax
  local ran, err = pcall(function()
    if req.op == 'open' and remote(req.path) then
      local buffers = package.loaded['eitri.remote.buffers']
      -- The language servers the buffer gets are chosen from the buffer the open leaves, so before it leaves it.
      if buffers and type(buffers.opening) == 'function' then pcall(buffers.opening, req.path) end
      vim.api.nvim_cmd({ cmd = 'edit', args = { req.path }, magic = { file = false } }, {})
      -- The buffer is still being read: the line and column are kept for when its text arrives. The column is
      -- judged as for a file (a whole number from 1 below 2^31, a unit of the three); the buffers count from 0.
      if buffers and type(req.line) == 'number' and req.line > 0 then
        local col = req.col
        local column = type(col) == 'number' and col >= 1 and col < 2 ^ 31 and col == math.floor(col)
          and type(req.unit) == 'string' and units[req.unit]
        if column then
          pcall(buffers.want, vim.api.nvim_get_current_buf(), req.line, col - 1, req.unit)
        else
          pcall(buffers.want, vim.api.nvim_get_current_buf(), req.line)
        end
      end
    elseif req.op == 'open' then
      run('edit', req.path)
      if type(req.line) == 'number' and req.line > 0 then
        -- A line past the buffer's end moves nothing, and then no column is placed either.
        if pcall(vim.api.nvim_win_set_cursor, 0, { req.line, 0 }) then place_column(req.line, req.col, req.unit) end
      end
    elseif req.op == 'view' then
      if req.tab then view_tab(req.path) else view(req.path) end
    elseif req.op == 'diff' then
      diff(req)
    elseif req.op == 'edit' then
      local buf = split(req.path)
      local written = false
      vim.api.nvim_create_autocmd('BufWritePost', { buffer = buf, callback = function() if torn then return end; written = true end })
      vim.api.nvim_create_autocmd('BufWipeout', {
        buffer = buf,
        once = true,
        callback = function() if torn then return end; mark(req.done, written and 'written' or 'discarded', req.path) end,
      })
    else
      error('unknown scratch op: ' .. tostring(req.op))
    end
  end)
  if req.op ~= 'open' and vim.go.syntax ~= previous_syntax then
    if syntax_set == nil or syntax_set ~= previous_syntax then syntax_before = previous_syntax end
    syntax_set = vim.go.syntax
  end
  -- Known limit: `open` and `view` carry no `done`, so a failure of theirs (E37 from `:edit` over a
  -- modified buffer with 'nohidden', say) reaches only nvim's message area; the panel said `ok`.
  if not ran then mark(req.done, 'error: ' .. tostring(err), req.path) end
end

EitriScratch = M

return function()
  if torn then return end
  torn = true
  -- A buffer with unwritten changes (a draft the person is still writing) stays open, with its window: its text
  -- exists nowhere else.
  local function unsaved(buf)
    return vim.api.nvim_buf_is_valid(buf) and vim.bo[buf].modified
  end
  local current = vim.api.nvim_get_current_win()
  for win, buf in pairs(windows) do
    if vim.api.nvim_win_is_valid(win) and vim.api.nvim_win_get_buf(win) == buf and not unsaved(buf) then
      pcall(vim.api.nvim_win_close, win, true)
    end
  end
  for buf in pairs(buffers) do
    if vim.api.nvim_buf_is_valid(buf) and not unsaved(buf) then
      pcall(vim.api.nvim_buf_delete, buf, {force=true})
    end
  end
  if vim.api.nvim_win_is_valid(current) then vim.api.nvim_set_current_win(current) end
  if syntax_set ~= nil and vim.go.syntax == syntax_set then vim.go.syntax = syntax_before end
  if rawget(_G, "EitriScratch") == M then
    EitriScratch = nil
  end
end
