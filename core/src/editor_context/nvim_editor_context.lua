-- Eitri editor-context feed (wire 1). Loaded by `shell` with `--cmd`, before the user's own
-- config, so it only installs autocommands and a timer; it changes no setting and no keymap.
--
-- It writes one JSON line per update to EITRI_EDITOR_SOCKET: which file is focused, where the
-- cursor is, and -- only while a visual selection is actually live -- the selected lines and their
-- text. It also keeps, in each buffer's `b:eitri_read_from`, what that buffer's text was read from,
-- and in `b:eitri_diagnostics_stood` which diagnostics are about the text before that.

-- Called two ways. Under `--cmd` (`dofile`, no arguments) the socket comes from the environment the
-- host set at spawn. Injected into an already running nvim, the chunk gets a table instead and must
-- not look at the environment at all: an nvim started inside another Eitri window inherits that
-- window's variables, and reading them would feed the wrong panel. An injected chunk returns a
-- function that undoes it.
local opts = ...
local socket
if type(opts) == "table" then
  socket = opts.socket
else
  socket = vim.env.EITRI_EDITOR_SOCKET
end
if not socket or socket == "" then
  return
end

-- Send at most this often. CursorMoved fires on every cursor motion, so a write per event means a
-- connect/write/close per keypress while a key repeats -- the theme feed has no equivalent problem
-- because ColorScheme is rare. The first change after a send arms a one-shot timer and later ones
-- ride along without moving it, which bounds the rate no matter how fast the user types; 150ms is
-- far below the time it takes to move a hand to the panel. Nothing runs while nothing changes.
local INTERVAL_MS = 150

-- Never fetch more than this many lines of a selection: fetching a 100,000-line selection would
-- be work done only to be thrown away -- measured at ~33ms on nvim's own loop, which is where the
-- editor's input latency lives. The reported line range is always the TRUE one. A selection this
-- cap shortens can still be well under CONTENT_LIMIT characters (short lines), so this cap marks
-- the text truncated on its own -- it does not rely on the character cap below to notice for it.
local MAX_SELECTION_LINES = 400
-- Same Unicode-scalar cap and marker as editor_context::compose. Apply it before JSON encoding:
-- even one valid selected line can be larger than the host's bounded socket reader permits.
local CONTENT_LIMIT = 2000
local TRUNCATION_MARKER = "\n... (truncated)"
-- A character is at most 4 bytes, so this many bytes of selected text hold more than CONTENT_LIMIT
-- characters (with 8 to spare for a cut inside a character, or an exclusive 'selection' leaving out
-- the last one): fetching further would copy text only to cut it off.
local CONTENT_BYTES = 4 * (CONTENT_LIMIT + 1) + 8

-- Whether something changed since the last send; the timer is armed exactly while it is set.
local dirty = false
-- Set first thing by the teardown: a send the timer queued just before it must write nothing.
local torn = false

-- A position's first and last display column, 1-based, as getregion's own blockwise rule reads
-- them: virtcol() asks the same getvvcol(), with the position's 'virtualedit' offset (its fourth
-- field). (getregion also turns 'linebreak' off while it asks, virtcol() does not; the two can
-- differ only at a wrap point with 'wrap' and 'linebreak' both on.)
local function display_span(pos)
  local span = vim.fn.virtcol({ pos[2], pos[3], pos[4] }, 1)
  return span[1], span[2]
end

-- The display-column range a blockwise getregion takes from two positions: nvim's getregionpos()
-- (src/nvim/eval/funcs.c), which orders them, then starts at the leftmost first column and ends at
-- the rightmost last one -- or, with 'selection' exclusive and the lower position to the right,
-- just before the lower one.
local function block_columns(a, a_start, a_end, b, b_start, b_end)
  local b_first = b[2] < a[2] or (b[2] == a[2] and (b[3] < a[3] or (b[3] == a[3] and b[4] < a[4])))
  if b_first then
    a_start, a_end, b_start, b_end = b_start, b_end, a_start, a_end
  end
  local finish = math.max(a_end, b_end)
  if vim.o.selection:sub(1, 1) == "e" and a_end < b_start and b_start > 1 and b_end > a_end then
    finish = b_start - 1
  end
  return math.min(a_start, b_start), finish
end

-- The position on line `lnum` at display column `col`, the way the cursor would sit there: the
-- character covering it, plus 'virtualedit''s offset into that character (a <Tab>) or past the end
-- of the line. Where virtual editing is not active, or the character is a wide one, getvvcol()
-- ignores the offset and the caller's check sees the whole character instead.
local function position_at(lnum, col)
  local text = vim.fn.getline(lnum)
  local past_end = vim.fn.virtcol({ lnum, #text + 1 })
  if col >= past_end then
    return { 0, lnum, #text + 1, col - past_end }
  end
  local byte = vim.fn.virtcol2col(0, lnum, col)
  return { 0, lnum, byte, col - vim.fn.virtcol({ lnum, byte }, 1)[1] }
end

-- Where a blockwise selection over the line cap ends instead. A block is a
-- DISPLAY-column range applied to every line, taken from its two corners. Moving the lower corner
-- up to the cap line and keeping its byte column is not that range once the lines differ: a byte
-- column 2 that was `b` on the true end line is the `X` of `\tXYZ` at display column 9 on a capped
-- line, silently widening the block to columns the user never selected: a selection of `ab`
-- becomes `abcdefghi`.
--
-- So the lower corner is placed on the lowest line within the cap, at the display column the real
-- one had, and kept only if getregion's own rule then yields exactly the original block. A line
-- where no position can (a wide character or, with 'virtualedit' off, a <Tab> straddling the edge,
-- or a line too short) gives way to the one above it: fewer lines, never other columns. Only the
-- first line can be left, and nil means not even it could -- the caller then sends only the
-- truncation marker. At most MAX_SELECTION_LINES lines are tried, a few calls each.
local function capped_block_end(first, last)
  local first_start, first_end = display_span(first)
  local last_start, last_end = display_span(last)
  local want_start, want_end = block_columns(first, first_start, first_end, last, last_start, last_end)
  local columns = { last_start, last_end, want_start, want_end }
  for lnum = first[2] + MAX_SELECTION_LINES - 1, first[2], -1 do
    for _, col in ipairs(columns) do
      local corner = position_at(lnum, col)
      local corner_start, corner_end = display_span(corner)
      local got_start, got_end = block_columns(first, first_start, first_end, corner, corner_start, corner_end)
      if got_start == want_start and got_end == want_end then
        return corner
      end
    end
  end
  return nil
end

-- Where a charwise or linewise fetch from `first` to `last` can end instead: the position at which
-- the text fetched so far is CONTENT_BYTES long, or nil when it never is before `last`'s own line
-- (that line is never cut: 'selection' and the cursor decide where it ends). Only line lengths are
-- read, so nothing is copied to find it. The text up to it holds more than CONTENT_LIMIT characters,
-- so the result is cut at the same character as the whole selection's would be.
local function content_end(mode, first, last)
  local fetched = 0
  for lnum = first[2], last[2] - 1 do
    local length = vim.fn.col({ lnum, "$" }) - 1
    local from = (mode == "v" and lnum == first[2]) and first[3] or 1
    local remaining = CONTENT_BYTES - fetched
    local here = math.max(length - from + 1, 0)
    if here >= remaining then
      return { 0, lnum, from + remaining - 1, 0 }
    end
    -- The newline that joins it to the next line counts as one character.
    fetched = fetched + here + 1
  end
  return nil
end

local function utf16_units(line, bytes)
  bytes = math.min(math.max(bytes, 0), #line)
  if vim.fn.has("nvim-0.11") == 1 then
    return vim.str_utfindex(line, "utf-16", bytes, false)
  end
  local _, units = vim.str_utfindex(line, bytes)
  return units
end

local function selection()
  local mode = vim.fn.mode()
  -- Visual (v), visual-line (V) and visual-block (\22) only. `'<` and `'>` are deliberately not
  -- consulted: they are a side effect of LEAVING visual mode, and in Eitri the user never leaves
  -- it -- they click the WebView, and nvim is never told it lost focus. Measured: the marks stay
  -- [0,0,0,0] for the whole life of a live selection.
  if mode ~= "v" and mode ~= "V" and mode ~= "\22" then
    return nil
  end
  local anchor = vim.fn.getpos("v")
  local cursor = vim.fn.getpos(".")
  local first, last = anchor, cursor
  if first[2] > last[2] or (first[2] == last[2] and first[3] > last[3]) then
    first, last = cursor, anchor
  end
  local fetch_last = { last[1], last[2], last[3], last[4] }
  local line_capped = false
  if fetch_last[2] - first[2] >= MAX_SELECTION_LINES then
    line_capped = true
    if mode == "\22" then
      -- Blockwise selects a COLUMN RANGE that applies to every included line, so neither the far
      -- right (the block grew to full-line width) nor the real corner's byte column on another
      -- line (it grew to wherever that byte sits there) will do: see capped_block_end.
      local ok_end, corner = pcall(capped_block_end, first, last)
      fetch_last = ok_end and corner or nil
    else
      -- Charwise ("v") and linewise ("V") have no such column meaning: getregion ignores this
      -- column for "V", and for "v" widening it is what makes a capped selection end at end-of-line
      -- rather than mid-character on the original (now out-of-range) column.
      fetch_last[2] = first[2] + MAX_SELECTION_LINES - 1
      fetch_last[3] = 2147483647
    end
  end
  local fetch_first, fetch_type = first, mode
  if fetch_last and mode ~= "\22" then
    local cut = content_end(mode, first, fetch_last)
    if cut then
      -- Fetched charwise up to the cut, from the first line's start for a linewise selection: the
      -- same text as the selection's, ending sooner.
      if mode == "V" then
        fetch_first = { first[1], first[2], 1, 0 }
      end
      fetch_last, fetch_type = cut, "v"
    end
  end
  local lines = {}
  if fetch_last then
    local ok, got = pcall(vim.fn.getregion, fetch_first, fetch_last, { type = fetch_type })
    if not ok or type(got) ~= "table" then
      return nil
    end
    lines = got
  end
  -- Keep one extra character to distinguish an exact-length selection from a truncated one.
  -- strcharpart/strchars count composing characters separately by default, matching Rust chars().
  local text = vim.fn.strcharpart(table.concat(lines, "\n"), 0, CONTENT_LIMIT + 1)
  local char_capped = vim.fn.strchars(text) > CONTENT_LIMIT
  if char_capped then
    text = vim.fn.strcharpart(text, 0, CONTENT_LIMIT)
  end
  -- One marker for either reason, never two: a selection can lose content to the line cap, the
  -- character cap, or both at once (a long enough run within the first 400 lines), and each case
  -- must read as truncated exactly once.
  if line_capped or char_capped then
    text = text .. TRUNCATION_MARKER
  end
  local result = { start_line = first[2], end_line = last[2], text = text }
  -- Optional columns use the true range, even when the selected text is capped. A conversion
  -- failure costs only these columns, so the selection's lines and text still reach the host.
  local ok_cols, start_col, end_col = pcall(function()
    if mode == "V" then
      return 0, 0
    end
    local first_line = vim.fn.getline(first[2])
    local last_line = vim.fn.getline(last[2])
    local end_bytes = math.min(last[3] - 1, #last_line)
    if end_bytes < #last_line and vim.o.selection ~= "exclusive" then
      end_bytes = end_bytes + vim.str_utf_end(last_line, end_bytes + 1) + 1
    end
    return utf16_units(first_line, first[3] - 1), utf16_units(last_line, end_bytes)
  end)
  if ok_cols then
    result.start_col, result.end_col = start_col, end_col
  end
  return result
end

local function send()
  if torn then
    return
  end
  local ok, payload = pcall(function()
    local buf = vim.api.nvim_get_current_buf()
    return vim.json.encode({
      v = 1,
      file = vim.api.nvim_buf_get_name(buf),
      line = vim.fn.line("."),
      selection = selection(),
    })
  end)
  if not ok or not payload then
    return
  end
  -- Connect/write/close are asynchronous. The host may accept before this connect callback writes
  -- any bytes; it retains incomplete lines across polls instead of waiting on the GTK thread.
  local pipe = vim.uv.new_pipe(false)
  pipe:connect(socket, function(err)
    if err then
      pipe:close()
      return
    end
    pipe:write(payload .. "\n", function()
      pipe:close()
    end)
  end)
end

local timer = vim.uv.new_timer()

local function fire()
  dirty = false
  -- `vim.fn.*` and `vim.api` are not allowed in a libuv callback (a "fast event"); hop to the
  -- main loop first. Getting this wrong fails with E5560 at runtime, never at load.
  vim.schedule(send)
end

-- The first change after a send arms the timer; later ones wait for the same send.
local function changed()
  if dirty or torn then
    return
  end
  dirty = true
  timer:start(INTERVAL_MS, 0, fire)
end

local group = vim.api.nvim_create_augroup("EitriEditorContext", { clear = true })
vim.api.nvim_create_autocmd(
  { "CursorMoved", "CursorMovedI", "ModeChanged", "BufEnter", "BufFilePost", "VimEnter" },
  {
    group = group,
    callback = changed,
  }
)
-- A buffer's name says what the disk holds under it now, and nvim keeps nothing of where the text
-- came from: a buffer opened through a link keeps that file's text after the link is replaced. So the
-- real path is recorded here at load, once per load, and goes nowhere until a tool asks for it. `:w
-- other` leaves the record, since the buffer still holds what it held, and `:file` fires nothing here:
-- it changes a name and no text. A reload fires BufReadPost, so the record follows what was read.
--
-- `vim.diagnostic` keeps its entries through a load, and an entry can quote the text it was set about.
-- So a load that moves the record notes the namespaces standing on the buffer, and a tool leaves
-- those out. A record that is missing has moved too: `:bdelete` drops it and leaves the entries. A write
-- replaces no text, so it notes nothing. nvim loads `vim.diagnostic` on first use, and until then
-- nothing can stand.
local function note_what_stood(buf)
  local diagnostic = package.loaded["vim.diagnostic"]
  if not diagnostic then
    return
  end
  local stood = {}
  for _, entry in ipairs(diagnostic.get(buf)) do
    stood[tostring(entry.namespace)] = true
  end
  vim.b[buf].eitri_diagnostics_stood = next(stood) and stood or nil
end

vim.api.nvim_create_autocmd({ "BufReadPost", "BufNewFile", "BufWritePost" }, {
  group = group,
  callback = function(args)
    local name = vim.api.nvim_buf_get_name(args.buf)
    if name == "" or (args.event == "BufWritePost" and args.match ~= name) then
      return
    end
    local record = vim.uv.fs_realpath(name) or name
    if args.event ~= "BufWritePost" and vim.b[args.buf].eitri_read_from ~= record then
      note_what_stood(args.buf)
    end
    vim.b[args.buf].eitri_read_from = record
  end,
})
-- `vim.diagnostic.set` replaces a namespace's entries on a buffer, so the note on it goes with the first
-- entry set after the load. An empty set says nothing: its event names no namespace.
vim.api.nvim_create_autocmd("DiagnosticChanged", {
  group = group,
  callback = function(args)
    local stood = vim.b[args.buf].eitri_diagnostics_stood
    if not stood or not args.data then
      return
    end
    for _, entry in ipairs(args.data.diagnostics or {}) do
      stood[tostring(entry.namespace)] = nil
    end
    vim.b[args.buf].eitri_diagnostics_stood = next(stood) and stood or nil
  end,
})

-- What the editor shows when this is loaded is news too.
changed()

-- Undoes this chunk: the autocommands, the timer and the records, which nothing keeps true from here on.
return function()
  torn = true
  pcall(vim.api.nvim_del_augroup_by_id, group)
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    pcall(vim.api.nvim_buf_del_var, buf, "eitri_read_from")
    pcall(vim.api.nvim_buf_del_var, buf, "eitri_diagnostics_stood")
  end
  if not timer:is_closing() then
    timer:stop()
    timer:close()
  end
end
