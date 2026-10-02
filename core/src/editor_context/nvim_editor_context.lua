-- Eitri editor-context feed (wire 1). Loaded by `shell` with `--cmd`, before the user's own
-- config, so it only installs autocommands and a timer; it changes no setting and no keymap.
--
-- It writes one JSON line per update to EITRI_EDITOR_SOCKET: which file is focused, where the
-- cursor is, and -- only while a visual selection is actually live -- the selected lines and their
-- text.

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
-- because ColorScheme is rare. A dirty flag plus one repeating timer bounds the rate no matter how
-- fast the user types, and 150ms is far below the time it takes to move a hand to the panel.
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

local dirty = true
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

-- Where a blockwise selection over the line cap ends instead (P5-M1, round 2). A block is a
-- DISPLAY-column range applied to every line, taken from its two corners. Moving the lower corner
-- up to the cap line and keeping its byte column is not that range once the lines differ: a byte
-- column 2 that was `b` on the true end line is the `X` of `\tXYZ` at display column 9 on a capped
-- line, and the block silently widened to columns the user never selected (the Codex review ran
-- this very function and got `abcdefghi` for a selection of `ab`).
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
      -- right (M1: the block grew to full-line width) nor the real corner's byte column on another
      -- line (M1 round 2: it grew to wherever that byte sits there) will do: see capped_block_end.
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
  local lines = {}
  if fetch_last then
    local ok, got = pcall(vim.fn.getregion, first, fetch_last, { type = mode })
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
  return { start_line = first[2], end_line = last[2], text = text }
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

local group = vim.api.nvim_create_augroup("EitriEditorContext", { clear = true })
vim.api.nvim_create_autocmd(
  { "CursorMoved", "CursorMovedI", "ModeChanged", "BufEnter", "BufFilePost", "VimEnter" },
  {
    group = group,
    callback = function()
      dirty = true
    end,
  }
)

local timer = vim.uv.new_timer()
timer:start(INTERVAL_MS, INTERVAL_MS, function()
  if not dirty then
    return
  end
  dirty = false
  -- `vim.fn.*` and `nvim_api` are not allowed in a libuv callback (a "fast event"); hop to the
  -- main loop first. Getting this wrong fails with E5560 at runtime, never at load.
  vim.schedule(send)
end)

-- Undoes this chunk: the autocommands and the timer.
return function()
  torn = true
  pcall(vim.api.nvim_del_augroup_by_id, group)
  if not timer:is_closing() then
    timer:stop()
    timer:close()
  end
end
