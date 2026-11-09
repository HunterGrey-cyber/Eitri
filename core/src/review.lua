-- Eitri's turn review, drawn in the user's own nvim: the hunks of one turn over the buffer of a
-- file the user has open, with keys to move between them, to revert one in the buffer and to close
-- the overlay.
--
-- Installed by one RPC call with the arguments `owner, version`; every later call reaches the
-- module through `_G.__eitri_review`. Paths, hunks and line text arrive only as arguments: they
-- are compared with buffer lines or set as buffer lines, never run, and a file is opened only
-- through `nvim_cmd` with the path as a structured argument and file-name expansion off.
--
-- A live draw lets nvim reload an unmodified buffer through checktime. A revert changes the
-- buffer only (undoable with `u`, and left unsaved) and is queued as an event, which the host
-- takes by polling. The module then keeps
-- following the reverted hunk: its text coming back (`u`, or typing it) is an `unrevert` event, and
-- the base text coming back over a drawn hunk (`Ctrl-r`, or typing it) is a `revert` event.
--
-- A buffer line holds a file line without its terminator. `expected` below is the one place that
-- says what text a buffer of a given 'fileformat' holds for a file line; nothing else in this file
-- adds or strips a carriage return or a newline.
local owner, version = ...
local api = vim.api

if type(owner) ~= 'string' or type(version) ~= 'number' then
  error('eitri review: the install takes an owner and a version')
end

local previous = rawget(_G, '__eitri_review')
if previous and previous.owner == owner and previous.version == version then
  return { installed = false, replaced = false }
end

-- A companion panel's module goes when that panel's glue goes (detach, retarget, another panel's
-- install, the channel closing), so it is installed only into the glue of that very channel: a
-- module that nobody would tear down must not be installed at all.
local comp = nil
if owner ~= 'embedded' then
  local chan = owner:match('^companion:(%d+)$')
  if not chan then
    error('eitri review: unknown owner ' .. owner)
  end
  comp = rawget(_G, '__eitri_companion')
  if type(comp) ~= 'table' or comp.chan ~= tonumber(chan) or type(comp.teardowns) ~= 'table' then
    error('eitri review: the companion glue of channel ' .. chan .. ' is not installed')
  end
end

local replaced = false
if previous then
  if type(previous.teardown) == 'function' then
    pcall(previous.teardown)
  end
  if rawget(_G, '__eitri_review') == previous then
    _G.__eitri_review = nil
  end
  replaced = true
end

local KEYS = { ']h', '[h', '<localleader>r', '<localleader>q' }
local DESC = {
  [']h'] = 'eitri review: next hunk',
  ['[h'] = 'eitri review: previous hunk',
  ['<localleader>r'] = 'eitri review: revert the hunk under the cursor',
  ['<localleader>q'] = 'eitri review: close the review overlay',
}
local LIVE_KEYS = { ']h', '[h', '<localleader>r', '<localleader>q', '<localleader>a', '<localleader>d' }
DESC['<localleader>a'] = 'eitri review: keep all changes in this buffer'
DESC['<localleader>d'] = 'eitri review: undo all changes in this buffer'
local COMMANDS = {
  EitriReviewKeep = DESC['<localleader>a'],
  EitriReviewUndo = DESC['<localleader>d'],
}
local CLEAR_DESC = 'eitri review: clear all live drawings'
local EOLS = { lf = true, crlf = true, missing = true }
local PRIORITY = 90
-- Bound the bytes tokenised for one line.
local WORD_MAX_BYTES = 400
-- Bound the line pairs examined for words in one hunk.
local WORD_MAX_PAIRS = 100
-- Bound the token diff on either side of a pair.
local WORD_MAX_TOKENS = 200
-- The host reads every answer as one msgpack frame and drops the connection over a frame of 16 MiB,
-- after the buffer already changed. So a revert whose event would be larger than this is refused
-- (its two sides are echoed whole), and one answer of `take_events` carries events up to the second
-- figure, at least one, leaving the rest for the next.
local MAX_REVERT_EVENT_BYTES = 4194304
local TAKE_EVENTS_BYTES = 8388608
local textdiff = (vim.text and vim.text.diff) or vim.diff

local state = {
  owner = owner,
  version = version,
  ns = api.nvim_create_namespace('eitri_review'),
  -- The module's autocommands exist only while some buffer shows an overlay (`watch`/`unwatch`).
  augroup = nil,
  events = {},
  overlays = {},
  kept = {},
  kept_order = {},
  kept_turns = {},
  kept_count = 0,
  reloading = {},
  -- How many `off` events were ever queued. Each carries its number and every answer the latest, so
  -- the host can tell an `off` queued before an answer that drew the file again from one after it.
  off_seq = 0,
  -- Hunks reverted in a buffer whose overlay went away before their text came back, so nothing
  -- followed them: `{ path, meta, hunk }`, oldest first. A later show that draws one over its new
  -- text learns the revert was undone (`take_forgotten`).
  forgotten = {},
  torn = false,
}
local MAX_FORGOTTEN = 256

-- Whether `vim.notify` is still nvim's own, which echoes on the command line. Anything else -- a
-- notification plugin's function, or a callable table -- shows a long line its own way.
local function own_notify()
  local notify = vim.notify
  if type(notify) ~= 'function' then
    return false
  end
  local ok, info = pcall(debug.getinfo, notify, 'S')
  local source = ok and info and info.source or ''
  return source:find('vim/_editor', 1, true) ~= nil or source:find('vim/_core/editor', 1, true) ~= nil
end

-- `msg` cut in the middle to at most `room` cells, as nvim's own `shortmess+=T` cuts a long
-- message: its start and its end stay, the end with two thirds of the room, so an instruction at
-- the end ("undo it in the panel (x)") is still read.
local function cut_to(msg, room)
  local mark = '...'
  local keep = room - #mark
  local head_room = math.floor(keep / 3)
  local tail_room = keep - head_room
  local n = vim.fn.strchars(msg)
  local head, tail = '', ''
  local i = 0
  while i < n do
    local ch = vim.fn.strcharpart(msg, i, 1)
    if vim.fn.strdisplaywidth(head .. ch) > head_room then
      break
    end
    head = head .. ch
    i = i + 1
  end
  local j = n - 1
  while j >= i do
    local ch = vim.fn.strcharpart(msg, j, 1)
    if vim.fn.strdisplaywidth(ch .. tail) > tail_room then
      break
    end
    tail = ch .. tail
    j = j - 1
  end
  return head .. mark .. tail
end

-- A line wider than `v:echospace` stops nvim at a hit-enter prompt in the middle of the key the
-- person pressed, and the editor waits there until they answer it. nvim keeps in `:messages` only
-- what it shows (a `:silent` message never reaches it), so through nvim's own notify such a line
-- is echoed cut in the middle to fit, and that is what `:messages` holds; anything shorter, or any
-- line under a replaced `vim.notify`, goes through `vim.notify` whole.
local function say(msg, level)
  local room = tonumber(vim.v.echospace) or 0
  if room <= 3 or vim.fn.strdisplaywidth(msg) <= room or not own_notify() then
    vim.notify(msg, level)
    return
  end
  local hl = level == vim.log.levels.WARN and 'WarningMsg' or (level == vim.log.levels.ERROR and 'ErrorMsg' or nil)
  vim.api.nvim_echo({ { cut_to(msg, room), hl } }, true, {})
end

local function warn(msg)
  say(msg, vim.log.levels.WARN)
end

local function active_count()
  local n = 0
  for _ in pairs(state.overlays) do
    n = n + 1
  end
  return n
end

-- Every answer reports both drawings and events still waiting for the host.
local function status(answer)
  answer.active = active_count()
  answer.pending = #state.events
  answer.off_seq = state.off_seq
  answer.live_only = true
  for _, ov in pairs(state.overlays) do
    if not ov.live then
      answer.live_only = false
      break
    end
  end
  return answer
end

local function is_shown(buf)
  if not buf or not api.nvim_buf_is_loaded(buf) then
    return false
  end
  for _, win in ipairs(api.nvim_list_wins()) do
    local config = api.nvim_win_get_config(win)
    if api.nvim_win_get_buf(win) == buf and config.relative == '' and not config.external then
      return true
    end
  end
  return false
end

-- The text a buffer of 'fileformat' `ff` holds for a file line of `text` ended by `eol`, or nil
-- when such a buffer cannot hold that line unchanged. A unix buffer of a file with mixed endings
-- keeps a CR-LF line's carriage return in its text; a dos buffer cannot hold an LF line; a mac
-- buffer is never compared.
local function expected(text, eol, ff)
  if ff == 'unix' then
    if eol == 'lf' or eol == 'missing' then
      return text
    end
    if eol == 'crlf' then
      return text .. '\r'
    end
  elseif ff == 'dos' then
    if eol == 'crlf' or eol == 'missing' then
      return text
    end
  end
  return nil
end

local function check_path(path)
  -- An absolute path also rules out anything an Ex command line would read as `+cmd` or `++opt`.
  if type(path) ~= 'string' or path:sub(1, 1) ~= '/' then
    error('eitri review: a path must be absolute')
  end
end

local function check_meta(meta)
  if type(meta) ~= 'table' or type(meta.tab) ~= 'number' or type(meta.session) ~= 'string'
    or type(meta.turn) ~= 'number' or (meta.scope ~= 'turn' and meta.scope ~= 'session') then
    error('eitri review: malformed meta')
  end
end

local function side_ok(start, len, lines, eols)
  if type(start) ~= 'number' or type(len) ~= 'number' or len < 0 or start < 0 or (len > 0 and start < 1) then
    return false
  end
  if type(lines) ~= 'table' or type(eols) ~= 'table' or #lines ~= len or #eols ~= len then
    return false
  end
  for i = 1, len do
    if type(lines[i]) ~= 'string' or not EOLS[eols[i]] then
      return false
    end
  end
  return true
end

-- A malformed hunk is the host's bug, so it is an error, not a hunk that "no longer matches".
local function check_hunks(hunks)
  if type(hunks) ~= 'table' then
    error('eitri review: hunks must be a list')
  end
  for _, h in ipairs(hunks) do
    local ok = type(h) == 'table' and type(h.id) == 'number'
      and side_ok(h.old_start, h.old_len, h.old_lines, h.old_eols)
      and side_ok(h.new_start, h.new_len, h.new_lines, h.new_eols)
    if not ok then
      error('eitri review: malformed hunk ' .. tostring(type(h) == 'table' and h.id or '?'))
    end
  end
end

-- Where a hunk's new side begins, from 0: a side of no lines is "after line `start`", any other
-- begins at line `start`.
local function start_row(h)
  if h.new_len == 0 then
    return h.new_start
  end
  return h.new_start - 1
end

-- Whether rows [row, row + n) of `buf` hold `lines` (one side of a hunk, `eols` naming each line's
-- ending) as this buffer would hold the file's lines, final newline included:
-- a line without one must be the buffer's last with
-- 'eol' off, and a last line with one needs 'eol' on. A 'binary' buffer is never compared.
local function holds_side(buf, row, n, lines, eols, snapshot)
  if (snapshot and snapshot.binary) or (not snapshot and vim.bo[buf].binary) then
    return false
  end
  local count = snapshot and #snapshot or api.nvim_buf_line_count(buf)
  if row < 0 or row + n > count then
    return false
  end
  local ff = snapshot and snapshot.ff or vim.bo[buf].fileformat
  local ends = row + n == count
  local eol = snapshot and snapshot.eol
  if not snapshot then eol = vim.bo[buf].eol end
  local got = snapshot or api.nvim_buf_get_lines(buf, row, row + n, true)
  for i = 1, n do
    local e = eols[i]
    local want = expected(lines[i], e, ff)
    if want == nil or got[snapshot and row + i or i] ~= want then
      return false
    end
    if e == 'missing' and not (i == n and ends and not eol) then
      return false
    end
  end
  if n > 0 and ends and eols[n] ~= 'missing' and not eol then
    return false
  end
  return true
end

local function holds_new_side(buf, row, h, snapshot)
  return holds_side(buf, row, h.new_len, h.new_lines, h.new_eols, snapshot)
end

-- The tracker's current start row and last row, and whether it still spans whole lines.
local function tracked(buf, d)
  local ok, m = pcall(api.nvim_buf_get_extmark_by_id, buf, state.ns, d.tracker, { details = true })
  if not ok or type(m) ~= 'table' or m[1] == nil then
    return nil
  end
  local det = m[3] or {}
  local end_row, end_col = det.end_row or m[1], det.end_col or 0
  local last = end_col > 0 and end_row or end_row - 1
  return m[1], last, end_col == 0 and end_row - m[1] or -1
end

-- About how many bytes `event` takes in an answer: its line texts, a little for each line's framing
-- and the name of its ending, and its fixed fields. Never less than the real encoding.
local function event_size(event)
  local n = 512 + #(event.path or '') + #(event.session or '')
  for _, side in ipairs({ event.old_lines or {}, event.new_lines or {} }) do
    for i = 1, #side do
      n = n + #side[i] + 16
    end
  end
  return n
end

local function drop_marks(buf, d)
  for _, id in ipairs(d.marks) do
    pcall(api.nvim_buf_del_extmark, buf, state.ns, id)
  end
  d.marks = {}
  d.alive = false
end

-- NUL and the escape byte spelled out with the escape byte, so the result never holds a NUL:
-- vim.fn.sha256 refuses a string with one on nvim 0.10 and 0.11 (it arrives as a Blob).
local FIELD_ESCAPES = { ['\0'] = '\1\2', ['\1'] = '\1\1' }

-- A text as its byte length and its escaped bytes: no text can run into the next field.
local function field(text)
  local escaped = text:gsub('[%z\1]', FIELD_ESCAPES)
  return #text .. ':' .. escaped
end

local function keyed(lines, eols)
  local parts = {}
  for i = 1, #lines do
    parts[i] = eols[i] .. '\1' .. lines[i] .. '\n'
  end
  return table.concat(parts)
end

-- Both sides, each as its line count and every line's ending and text as fields.
local function fingerprint(h)
  local parts = {}
  for _, side in ipairs({ { h.old_lines, h.old_eols }, { h.new_lines, h.new_eols } }) do
    local lines, eols = side[1], side[2]
    parts[#parts + 1] = #lines .. ';'
    for i = 1, #lines do
      parts[#parts + 1] = field(eols[i]) .. field(lines[i])
    end
  end
  return vim.fn.sha256(table.concat(parts))
end

local function kept_hunks(path, meta)
  local turns = state.kept[path]
  return turns and turns[meta.session .. ':' .. meta.turn_id]
end

-- What a piece of a removed line looks like in a virtual line, `col` display columns in: control
-- characters made printable, so a raw carriage return never reaches the grid, and each Tab spread
-- over blanks to the next tab stop of the buffer (`strtrans` would show it as `^I`). The virtual line
-- starts at the text area's column 0 like the buffer's own line, so the stops line up with the
-- buffer's indentation. Runs with `buf` current: `strdisplaywidth` reads that buffer's 'tabstop' and
-- 'vartabstop'. Returns the text to show and the column after it.
local function display(text, col)
  local shown, at = {}, 1
  while at <= #text + 1 do
    local tab = text:find('\t', at, true)
    local piece = text:sub(at, (tab or #text + 1) - 1)
    if piece ~= '' then
      local ok, printable = pcall(vim.fn.strtrans, piece)
      if not (ok and type(printable) == 'string') then printable = piece end
      local wok, width = pcall(vim.fn.strdisplaywidth, printable, col)
      shown[#shown + 1], col = printable, wok and (type(width) == 'number') and col + width or col + #printable
    end
    if not tab then break end
    local wok, width = pcall(vim.fn.strdisplaywidth, '\t', col)
    if not (wok and type(width) == 'number' and width > 0) then width = 1 end
    shown[#shown + 1], col = string.rep(' ', width), col + width
    at = tab + 1
  end
  return table.concat(shown), col
end

-- The byte ranges that differ between two paired lines, found by diffing their words (a run of word
-- characters, a run of blanks, or one other byte). Nothing when a line is over a bound, or when a side
-- changed from end to end and so has nothing left to set the change against.
local function word_ranges(old, new)
  if #old > WORD_MAX_BYTES or #new > WORD_MAX_BYTES then return end
  local tokens, offsets = {}, {}
  for side, text in ipairs({ old, new }) do
    local ts, os, pos = {}, { 0 }, 1
    while pos <= #text do
      local token = text:match('^[A-Za-z0-9_\128-\255]+', pos) or text:match('^%s+', pos) or text:sub(pos, pos)
      ts[#ts + 1], pos = token, pos + #token
      os[#os + 1] = pos - 1
      if #ts > WORD_MAX_TOKENS then return end
    end
    tokens[side], offsets[side] = ts, os
  end
  local changes = textdiff(table.concat(tokens[1], '\n') .. '\n', table.concat(tokens[2], '\n') .. '\n',
    { result_type = 'indices' })
  local ranges = { {}, {} }
  for side, text in ipairs({ old, new }) do
    local total = 0
    for _, sub in ipairs(changes) do
      local start, len = sub[side * 2 - 1], sub[side * 2]
      if len > 0 then
        local a, b = offsets[side][start], offsets[side][start + len]
        local last = ranges[side][#ranges[side]]
        if last and text:sub(last[2] + 1, a):match('^%s*$') then
          total, last[2] = total + b - last[2], b
        else
          ranges[side][#ranges[side] + 1], total = { a, b }, total + b - a
        end
      end
    end
    if total == #text then return end
  end
  return ranges
end

-- A removed line as virtual-line chunks: the changed byte ranges in `DiffText`, the rest in `DiffDelete`.
-- The display column runs on across chunks, because a tab's width depends on where it starts.
local function old_chunks(buf, old, ranges)
  local chunks, at, col = {}, 0, 0
  local function add(from, to, group)
    local shown
    shown, col = display(old:sub(from, to), col)
    chunks[#chunks + 1] = { shown, group }
  end
  local function build()
    for _, r in ipairs(ranges or {}) do
      if r[1] > at then add(at + 1, r[1], 'DiffDelete') end
      add(r[1] + 1, r[2], 'DiffText')
      at = r[2]
    end
    if at < #old or #chunks == 0 then add(at + 1, #old, 'DiffDelete') end
  end
  if not pcall(api.nvim_buf_call, buf, build) then
    chunks, at, col = {}, 0, 0
    build()
  end
  return chunks
end

-- Draws one hunk whose new side is at `row`. A hunk carries context lines and may hold more than
-- one change, so the added and removed lines are found by diffing its two sides (each line keyed
-- with its terminator, so a change of ending alone shows too).
local function draw_hunk(buf, row, h)
  local count = api.nvim_buf_line_count(buf)
  -- `row` is where the hunk was last seen: an edit that replaces the whole hunk (a redo of its
  -- revert) takes the tracker with it, and this is where the base text is looked for then.
  local d = { hunk = h, marks = {}, alive = true, row = row }
  local function mark(r, opts, col)
    opts.priority = opts.priority or PRIORITY
    local id = api.nvim_buf_set_extmark(buf, state.ns, r, col or 0, opts)
    d.marks[#d.marks + 1] = id
    return id
  end
  -- Follows the hunk's lines as the buffer is edited: a line added just after it does not widen it.
  d.tracker = mark(row, { end_row = row + h.new_len, end_col = 0, end_right_gravity = false })
  local first = nil
  local subs = textdiff(keyed(h.old_lines, h.old_eols), keyed(h.new_lines, h.new_eols), { result_type = 'indices' })
  local pairs_seen = 0
  for _, sub in ipairs(subs or {}) do
    local sa, ca, sb, cb = sub[1], sub[2], sub[3], sub[4]
    -- Where a block holds as many old lines as new ones they pair up; `olds[k]` is the old line's
    -- changed ranges once its new line carries its own word marks.
    local olds = {}
    for k = 0, cb - 1 do
      local r, ranges, before = row + sb - 1 + k, nil, #d.marks
      if ca == cb and pairs_seen < WORD_MAX_PAIRS then
        pairs_seen = pairs_seen + 1
        local found, got = pcall(word_ranges, h.old_lines[sa + k], h.new_lines[sb + k])
        ranges = found and got or nil
      end
      -- A line highlight is painted over any text highlight whatever the priorities, so a line
      -- with word marks is filled by a text highlight that runs on to the window's edge instead.
      local ok = ranges and pcall(function()
        mark(r, { hl_group = 'DiffAdd', end_row = r + 1, end_col = 0, hl_eol = true, strict = false })
        for _, w in ipairs(ranges[2]) do
          mark(r, { hl_group = 'DiffText', end_col = w[2], priority = PRIORITY + 1 }, w[1])
        end
      end)
      if ok then
        olds[k] = ranges[1]
      else
        for j = #d.marks, before + 1, -1 do
          pcall(api.nvim_buf_del_extmark, buf, state.ns, table.remove(d.marks))
        end
        mark(r, { line_hl_group = 'DiffAdd' })
      end
    end
    -- With no new lines, `sb` is the new line the removal follows.
    local anchor = cb > 0 and (row + sb - 1) or (row + sb)
    if ca > 0 then
      local virt = {}
      for i = sa, sa + ca - 1 do
        virt[#virt + 1] = old_chunks(buf, h.old_lines[i], olds[i - sa])
      end
      if anchor >= count then
        mark(count - 1, { virt_lines = virt, virt_lines_above = false })
      else
        mark(anchor, { virt_lines = virt, virt_lines_above = true })
      end
    end
    if first == nil then
      first = math.min(anchor, count - 1)
    end
  end
  d.first = mark(first or row, { sign_text = '▎', sign_hl_group = 'DiffChange' })
  return d
end

local remove_overlay, watch, unwatch

-- Commands belong to the live drawings and leave with them. A command replaced by the person's
-- configuration keeps its new definition, just as a mapping replaced over ours does.
local function remove_commands(buf)
  local ok, commands = pcall(api.nvim_buf_get_commands, buf, {})
  if ok then
    for name, desc in pairs(COMMANDS) do
      if commands[name] and commands[name].definition == desc then
        pcall(api.nvim_buf_del_user_command, buf, name)
      end
    end
  end
  for _, ov in pairs(state.overlays) do
    if ov.live then return end
  end
  local command = api.nvim_get_commands({}).EitriReviewClear
  if command and command.definition == CLEAR_DESC then
    pcall(api.nvim_del_user_command, 'EitriReviewClear')
  end
end

local function clear_live()
  if state.torn then return end
  for buf, ov in pairs(state.overlays) do
    if ov.live then remove_overlay(buf, 'user') end
  end
end

local function set_commands(buf, actions)
  local commands = api.nvim_buf_get_commands(buf, {})
  local callbacks = {
    EitriReviewKeep = actions['<localleader>a'],
    EitriReviewUndo = actions['<localleader>d'],
  }
  for name, desc in pairs(COMMANDS) do
    if not commands[name] then
      api.nvim_buf_create_user_command(buf, name, callbacks[name], { desc = desc })
    end
  end
  if not api.nvim_get_commands({}).EitriReviewClear then
    api.nvim_create_user_command('EitriReviewClear', clear_live, { desc = CLEAR_DESC })
  end
end

-- The buffer-local maps of `lhs` that the overlay's keys shadow are saved and come back when it
-- goes; `maparg` also reports global maps, which are left alone, and `maparg`/`mapset` work on the
-- current buffer, hence `nvim_buf_call`.
local function set_keys(buf, ov, actions)
  api.nvim_buf_call(buf, function()
    for _, lhs in ipairs(ov.live and LIVE_KEYS or KEYS) do
      local was = vim.fn.maparg(lhs, 'n', false, true)
      if not ov.live or type(was) ~= 'table' or next(was) == nil then
        local saved = (type(was) == 'table' and was.buffer == 1) and was or nil
        vim.keymap.set('n', lhs, actions[lhs], { buffer = buf, desc = DESC[lhs], silent = true })
        local mine = vim.fn.maparg(lhs, 'n', false, true)
        ov.keys[#ov.keys + 1] = { ours = mine.lhs, desc = DESC[lhs], saved = saved }
      end
    end
  end)
end

-- Removes the overlay's keys, by the lhs they had when set (a later change of 'maplocalleader'
-- does not matter), and only while they are still the overlay's: another map set over one since
-- is the user's and stays.
local function restore_keys(buf, ov)
  pcall(api.nvim_buf_call, buf, function()
    for i = #ov.keys, 1, -1 do
      local k = ov.keys[i]
      local now = vim.fn.maparg(k.ours, 'n', false, true)
      if type(now) == 'table' and now.buffer == 1 and now.desc == k.desc then
        pcall(api.nvim_buf_del_keymap, buf, 'n', k.ours)
        if k.saved then
          pcall(vim.fn.mapset, 'n', false, k.saved)
        end
      end
    end
  end)
end

local function queue_off(path, why)
  state.off_seq = state.off_seq + 1
  state.events[#state.events + 1] = { kind = 'off', path = path, why = why, seq = state.off_seq }
end

-- Takes the overlay off `buf`. With `why`, an `off` event says so; without, the host asked for it.
remove_overlay = function(buf, why)
  local ov = state.overlays[buf]
  if not ov then
    return
  end
  state.overlays[buf] = nil
  for _, d in ipairs(ov.hunks) do
    if d.ghost then
      state.forgotten[#state.forgotten + 1] = { path = ov.path, meta = ov.meta, hunk = d.hunk }
    end
  end
  while #state.forgotten > MAX_FORGOTTEN do
    table.remove(state.forgotten, 1)
  end
  pcall(api.nvim_buf_clear_namespace, buf, state.ns, 0, -1)
  restore_keys(buf, ov)
  if ov.live then remove_commands(buf) end
  if why then
    queue_off(ov.path, why)
  end
  if active_count() == 0 then
    unwatch()
  end
end

function state.keep_current()
  if state.torn then return end
  local buf = api.nvim_get_current_buf()
  local ov = state.overlays[buf]
  if not ov then return end
  if ov.meta.turn_id then
    local turns = state.kept[ov.path]
    if not turns then
      turns = {}
      state.kept[ov.path] = turns
    end
    local key = ov.meta.session .. ':' .. ov.meta.turn_id
    local record = turns[key]
    if not record then
      record = {}
      turns[key] = record
    end
    local turn = state.kept_turns[key]
    if not turn then
      turn = { paths = {}, count = 0 }
      state.kept_turns[key] = turn
      state.kept_order[#state.kept_order + 1] = key
    end
    turn.paths[ov.path] = true
    for _, d in ipairs(ov.hunks) do
      if d.alive or d.ghost then
        local fp = fingerprint(d.hunk)
        if not record[fp] then
          record[fp] = true
          state.kept_count = state.kept_count + 1
          turn.count = turn.count + 1
        end
      end
    end
    while state.kept_count > 4096 do
      local oldest = table.remove(state.kept_order, 1)
      local old_turn = state.kept_turns[oldest]
      for path in pairs(old_turn.paths) do
        state.kept[path][oldest] = nil
        if next(state.kept[path]) == nil then state.kept[path] = nil end
      end
      state.kept_count = state.kept_count - old_turn.count
      state.kept_turns[oldest] = nil
    end
  end
  remove_overlay(buf, 'user')
end

local function jump(forward)
  if state.torn then
    return
  end
  local buf = api.nvim_get_current_buf()
  local ov = state.overlays[buf]
  if not ov then
    return
  end
  local rows = {}
  for _, d in ipairs(ov.hunks) do
    if d.alive then
      local ok, m = pcall(api.nvim_buf_get_extmark_by_id, buf, state.ns, d.first, {})
      if ok and type(m) == 'table' and m[1] ~= nil then
        rows[#rows + 1] = m[1]
      end
    end
  end
  table.sort(rows)
  local cur = api.nvim_win_get_cursor(0)[1] - 1
  local left, target = vim.v.count1, nil
  if forward then
    for i = 1, #rows do
      if left > 0 and rows[i] > cur then
        target, left = rows[i], left - 1
      end
    end
  else
    for i = #rows, 1, -1 do
      if left > 0 and rows[i] < cur then
        target, left = rows[i], left - 1
      end
    end
  end
  if target then
    api.nvim_win_set_cursor(0, { target + 1, 0 })
  end
end

-- Where the row `row` (from 0) of `buf` is tracked now, or nil when the mark is gone.
local function mark_row(buf, id)
  local ok, m = pcall(api.nvim_buf_get_extmark_by_id, buf, state.ns, id, {})
  if ok and type(m) == 'table' and m[1] ~= nil then
    return m[1]
  end
  return nil
end

-- The lines a buffer holds once hunk `h`, whose new side now sits in `rows` rows from `at`, is
-- reverted -- or, when the buffer cannot take the revert, nil and the reason. A buffer cannot hold
-- zero lines, nor stand for a file that does not exist; and setting lines cannot change 'eol' in a
-- way `u` undoes, nor give a line an ending this buffer cannot hold.
local function revert_plan(buf, at, h, rows)
  if h.old_len == 0 or h.new_len == 0 then
    return nil, 'this hunk creates or empties the file; revert it in the panel (x)'
  end
  local ff = vim.bo[buf].fileformat
  local ends = at + rows == api.nvim_buf_line_count(buf)
  local olds, refuse = {}, false
  for i = 1, h.old_len do
    local e = h.old_eols[i]
    olds[i] = expected(h.old_lines[i], e, ff)
    if olds[i] == nil or (e == 'missing' and not (i == h.old_len and ends)) then
      refuse = true
    end
  end
  if ends and (h.old_eols[h.old_len] == 'missing') ~= (h.new_eols[h.new_len] == 'missing') then
    refuse = true
  end
  if refuse then
    return nil, "this hunk changes the file's line endings; revert it in the panel (x)"
  end
  return olds
end

-- The event that says hunk `h` was reverted in the buffer (`revert`) or has its text back
-- (`unrevert`). The hunk's own header is echoed as it was shown; `at_line` is where its lines are
-- now, after any edit above them moved them.
local function hunk_event(kind, ov, h, at)
  local meta = ov.meta
  return {
    kind = kind,
    tab = meta.tab,
    session = meta.session,
    turn = meta.turn,
    scope = meta.scope,
    path = ov.path,
    hunk_id = h.id,
    old_start = h.old_start,
    old_len = h.old_len,
    new_start = h.new_start,
    new_len = h.new_len,
    at_line = at + 1,
    old_lines = h.old_lines,
    new_lines = h.new_lines,
    old_eols = h.old_eols,
    new_eols = h.new_eols,
  }
end

-- A reverted hunk stays in the overlay as two invisible marks on the row its lines start at, so
-- that the text coming back (`u`, or retyping it) is noticed. No single mark follows the hunk
-- through every edit at its edge: one that stays put when a line is added right above it is left
-- behind by that line, and one that moves with such a line is carried past the hunk's end when the
-- hunk's lines are replaced (an undo, a redo, setting lines). So there is one of each, and the
-- text is looked for at each of the places they point to (`ghost_rows`). `at2` is where the second
-- one was, when a carried hunk had them apart.
local function make_ghost(buf, d, at, at2)
  d.ghost = api.nvim_buf_set_extmark(buf, state.ns, at, 0, { right_gravity = false })
  d.ghost2 = api.nvim_buf_set_extmark(buf, state.ns, at2 or at, 0, {})
end

-- Rows where the text of a reverted hunk may start now, most likely first: where the mark that
-- stays put points, where the one that follows added lines points, and that one's row less the
-- hunk's new side (it moved to the end of lines set over the hunk's).
local function ghost_rows(buf, d)
  local rows = {}
  local a, b = mark_row(buf, d.ghost), mark_row(buf, d.ghost2)
  for _, row in ipairs({ a, b, b and b - d.hunk.new_len }) do
    if row and row >= 0 and not vim.tbl_contains(rows, row) then
      rows[#rows + 1] = row
    end
  end
  return rows
end

local function revert()
  if state.torn then
    return
  end
  local buf = api.nvim_get_current_buf()
  local ov = state.overlays[buf]
  if not ov then
    return
  end
  if ov.running then
    warn('the turn is still running; undo when it ends (Esc in the panel interrupts it)')
    return
  end
  local cur = api.nvim_win_get_cursor(0)[1] - 1
  local found, at, len = nil, nil, nil
  for _, d in ipairs(ov.hunks) do
    if d.alive then
      local s, last, n = tracked(buf, d)
      if s and cur >= s and cur <= last then
        found, at, len = d, s, n
        break
      end
    end
  end
  if not found then
    warn('there is no review hunk under the cursor')
    return
  end
  local h = found.hunk
  if len ~= h.new_len or not holds_new_side(buf, at, h) then
    warn('this hunk no longer matches the buffer; nothing was reverted')
    return
  end
  local olds, why = revert_plan(buf, at, h, h.new_len)
  if not olds then
    warn(why)
    return
  end
  local event = hunk_event('revert', ov, h, at)
  if event_size(event) > MAX_REVERT_EVENT_BYTES then
    warn('this hunk is too large to revert here; revert it in the panel (x)')
    return
  end
  local ok, err = pcall(api.nvim_buf_set_lines, buf, at, at + h.new_len, true, olds)
  if not ok then
    warn('the hunk could not be reverted: ' .. tostring(err))
    return
  end
  drop_marks(buf, found)
  make_ghost(buf, found, at)
  state.events[#state.events + 1] = event
end

local function undo_all()
  if state.torn then return end
  local buf = api.nvim_get_current_buf()
  local ov = state.overlays[buf]
  if not ov or not ov.live then return end
  if ov.running then
    warn('the turn is still running; undo when it ends (Esc in the panel interrupts it)')
    return
  end
  local plans, missing = {}, ov.skipped
  local kept = kept_hunks(ov.path, ov.meta)
  for _, d in ipairs(ov.hunks) do
    local h = d.hunk
    if d.alive then
      local at, _, len = tracked(buf, d)
      if at ~= nil and len == h.new_len and holds_new_side(buf, at, h) then
        plans[#plans + 1] = { drawing = d, at = at }
      else
        missing = missing + 1
      end
    elseif not d.ghost and not (kept and kept[fingerprint(h)]) then
      missing = missing + 1
    end
  end
  if missing > 0 then
    warn(missing .. ' changes of this turn are not drawn here; undo them in the panel (x)')
    return
  end
  table.sort(plans, function(a, b) return a.at < b.at end)
  -- Validate every replacement and event before touching any line. Reverted hunks are followed
  -- by their ghosts and need no second replacement. Each event is read from the top down, as the
  -- reverts of the hunks above it already stand, so its line is its row less what they shifted.
  local shift = 0
  for _, plan in ipairs(plans) do
    local h = plan.drawing.hunk
    if h.old_len == 0 or h.new_len == 0 then
      warn('this change creates or empties the file; undo it in the panel (x)')
      return
    end
    plan.lines = revert_plan(buf, plan.at, h, h.new_len)
    if not plan.lines then
      warn("this change alters the file's line endings; undo it in the panel (x)")
      return
    end
    plan.event = hunk_event('revert', ov, h, plan.at - shift)
    if event_size(plan.event) > MAX_REVERT_EVENT_BYTES then
      warn('this change is too large to undo here; undo it in the panel (x)')
      return
    end
    shift = shift + h.new_len - h.old_len
  end
  if #plans == 0 then return end
  -- All replacements run in this one key press or command, so nvim records one undo step.
  -- Starting at the bottom preserves every earlier replacement's validated line range.
  for i = #plans, 1, -1 do
    local plan = plans[i]
    local d = plan.drawing
    local ok, err = pcall(api.nvim_buf_set_lines, buf, plan.at, plan.at + d.hunk.new_len, true, plan.lines)
    if not ok then
      warn('the changes could not be undone: ' .. tostring(err))
      return
    end
    drop_marks(buf, d)
    make_ghost(buf, d, plan.at)
  end
  -- Events describe the buffer from top to bottom, whatever order the hunks arrived in.
  for _, plan in ipairs(plans) do
    state.events[#state.events + 1] = plan.event
  end
  local noun = #plans == 1 and ' change' or ' changes'
  say(#plans .. noun .. ' undone in this buffer only; :w writes them, u brings them back', vim.log.levels.INFO)
end
state.undo_current = undo_all

-- After an edit of `buf`: a hunk whose lines were changed is no longer drawn, unless the change
-- put the base text back (a redo of a revert, or the user typing it), which reads as the revert;
-- and a hunk reverted here whose own text is back (an undo, or typing it again) is drawn again and
-- reported, so that the revert recorded for it can be dropped.
local MAX_SEARCH_LINES = 20000

-- The one row where `h`'s new side is in the snapshot and its old side is not, or nil when there is none
-- or more than one place (an edit that moved whole text, such as a formatter or `:%!`, left both
-- marks elsewhere; two places cannot be told apart). A large buffer is not searched.
local function find_new_side(lines, ff, h)
  if lines.binary or h.new_len == 0 then
    return nil
  end
  local first = expected(h.new_lines[1], h.new_eols[1], ff)
  if first == nil then
    return nil
  end
  local function holds(row, n, side, eols)
    if row + n - 1 > #lines then
      return false
    end
    local ends = row + n - 1 == #lines
    for i = 1, n do
      local want = expected(side[i], eols[i], ff)
      if want == nil or lines[row + i - 1] ~= want then
        return false
      end
      if eols[i] == 'missing' and not (i == n and ends and not lines.eol) then
        return false
      end
    end
    return not (n > 0 and ends and eols[n] ~= 'missing' and not lines.eol)
  end
  local found = nil
  for _, row in ipairs(lines.rows[first] or {}) do
    if holds(row, h.new_len, h.new_lines, h.new_eols)
      and not holds(row, h.old_len, h.old_lines, h.old_eols) then
      if found then
        return nil
      end
      found = row - 1
    end
  end
  return found
end

-- `search` also looks the whole buffer over for a reverted hunk's text when no mark points to it;
-- typing in insert mode does not, since it would do that on every key.
local function follow_edits(buf, search)
  local ov = state.overlays[buf]
  if not ov then
    return
  end
  -- The buffer's bytes are read whole only once a search needs them (and then once for the call);
  -- until then each comparison reads just its own rows, so typing costs nothing that grows with the file.
  local snapshot
  local lines, ff
  local searched = false
  for i, d in ipairs(ov.hunks) do
    local h = d.hunk
    if d.ghost then
      -- A hunk whose new side is a leading part of its old side (it removed lines at the end of the
      -- file) holds both at once; its text cannot be told from the revert, so it stays reverted.
      local back = nil
      for _, s in ipairs(ghost_rows(buf, d)) do
        if back == nil and holds_new_side(buf, s, h, snapshot)
          and not holds_side(buf, s, h.old_len, h.old_lines, h.old_eols, snapshot) then
          back = s
        end
      end
      if back == nil and search then
        if not searched then
          searched = true
          if api.nvim_buf_line_count(buf) <= MAX_SEARCH_LINES then
            -- All searches in this call share the buffer bytes, options and first-line index.
            lines = api.nvim_buf_get_lines(buf, 0, -1, true)
            ff = vim.bo[buf].fileformat
            lines.ff = ff
            lines.eol = vim.bo[buf].eol
            lines.binary = vim.bo[buf].binary
            snapshot = lines
            lines.rows = {}
            for row, line in ipairs(lines) do
              local rows = lines.rows[line]
              if not rows then
                rows = {}
                lines.rows[line] = rows
              end
              rows[#rows + 1] = row
            end
          end
        end
        if lines then
          back = find_new_side(lines, ff, h)
        end
      end
      if back then
        pcall(api.nvim_buf_del_extmark, buf, state.ns, d.ghost)
        pcall(api.nvim_buf_del_extmark, buf, state.ns, d.ghost2)
        ov.hunks[i] = draw_hunk(buf, back, h)
        state.events[#state.events + 1] = hunk_event('unrevert', ov, h, back)
      end
    elseif d.alive then
      local s, _, n = tracked(buf, d)
      local intact = s ~= nil and n == h.new_len
      if intact then
        d.row = s
      end
      if not intact or not holds_new_side(buf, s, h, snapshot) then
        -- Where the base text may have come back: where the tracker is, and where the hunk was
        -- last seen intact (an edit that replaced the whole hunk left the tracker at its end).
        local back = nil
        for _, at in ipairs({ s or d.row, d.row }) do
          if back == nil and at < api.nvim_buf_line_count(buf) and revert_plan(buf, at, h, h.old_len)
            and holds_side(buf, at, h.old_len, h.old_lines, h.old_eols, snapshot) then
            back = at
          end
        end
        local event = back and hunk_event('revert', ov, h, back)
        drop_marks(buf, d)
        if event and event_size(event) <= MAX_REVERT_EVENT_BYTES then
          make_ghost(buf, d, back)
          state.events[#state.events + 1] = event
        end
      end
    end
  end
  if ov.live then
    for _, d in ipairs(ov.hunks) do
      if d.alive or d.ghost then return end
    end
    remove_overlay(buf, 'edited')
  end
end

local function close()
  if state.torn then
    return
  end
  remove_overlay(api.nvim_get_current_buf(), 'user')
end

local ACTIONS = {
  [']h'] = function()
    jump(true)
  end,
  ['[h'] = function()
    jump(false)
  end,
  ['<localleader>r'] = revert,
  ['<localleader>q'] = close,
  ['<localleader>a'] = state.keep_current,
  ['<localleader>d'] = undo_all,
}

-- Removes and returns whether `path` had `hunk` of review `meta` among the forgotten reverts.
local function take_forgotten(path, meta, hunk)
  for i, f in ipairs(state.forgotten) do
    if f.path == path and f.meta.tab == meta.tab and f.meta.session == meta.session and f.meta.turn == meta.turn
      and f.meta.scope == meta.scope and vim.deep_equal(f.hunk, hunk) then
      table.remove(state.forgotten, i)
      return true
    end
  end
  return false
end

-- Draws into a loaded buffer, replacing its overlay. Only a live draw includes a wholly emptied
-- file; an explicit drawing keeps its existing treatment of hunks with no new lines.
-- A hunk reverted here, whose overlay then went away, and which is drawn again over its new text
-- (and not over its old one) has had its revert undone: an `unrevert` event says so, queued behind
-- the events the host has not taken yet, so it never overtakes the revert it undoes. A hunk that
-- was not reverted here says nothing.
local function show_into(buf, path, meta, hunks, live)
  -- A hunk reverted in this buffer under the same review is carried over to the new overlay (the
  -- panel redraws it when the user opens another hunk of the file), so that its text coming back
  -- is still noticed; it is not drawn, since its new side is not in the buffer.
  local carried = {}
  local before = state.overlays[buf]
  if before and before.path == path and before.meta.tab == meta.tab and before.meta.session == meta.session
    and before.meta.turn == meta.turn and before.meta.scope == meta.scope then
    for _, d in ipairs(before.hunks) do
      local a = d.ghost and mark_row(buf, d.ghost)
      local b = d.ghost and mark_row(buf, d.ghost2)
      if a or b then
        carried[d.hunk.id] = { hunk = d.hunk, row = a or b, row2 = b or a }
      end
    end
  end
  remove_overlay(buf, nil)
  local drawn, skipped, list, back = 0, 0, {}, {}
  local ok, err = pcall(function()
    for _, h in ipairs(hunks) do
      if h.new_len > 0 or live then
        local row = start_row(h)
        if holds_new_side(buf, row, h) then
          list[#list + 1] = draw_hunk(buf, row, h)
          drawn = drawn + 1
          -- Its new side is there. Unless its old side is too (a hunk that removed lines at the end
          -- of the file holds both, and cannot be told from its revert), its revert has been undone.
          if h.old_len == 0 or not holds_side(buf, row, h.old_len, h.old_lines, h.old_eols) then
            back[#back + 1] = { h = h, row = row }
          end
        elseif carried[h.id] and vim.deep_equal(carried[h.id].hunk, h) then
          local d = { hunk = h, marks = {}, alive = false }
          make_ghost(buf, d, carried[h.id].row, carried[h.id].row2)
          take_forgotten(path, meta, h)
          list[#list + 1] = d
        else
          skipped = skipped + 1
        end
      end
    end
  end)
  if not ok then
    pcall(api.nvim_buf_clear_namespace, buf, state.ns, 0, -1)
    error(err)
  end
  local notice = nil
  if skipped == 1 then
    notice = '1 hunk no longer matches this buffer'
  elseif skipped > 1 then
    notice = skipped .. ' hunks no longer match this buffer'
  end
  -- An overlay whose hunks are all reverted stays, so the text of those coming back is still
  -- followed; it goes like any other, and takes its marks and the module's autocommands with it.
  if #list > 0 then
    local ov = { path = path, meta = meta, hunks = list, keys = {},
      live = live == true, running = live and meta.running or false, skipped = skipped }
    state.overlays[buf] = ov
    set_keys(buf, ov, ACTIONS)
    if ov.live then set_commands(buf, ACTIONS) end
    watch()
    for _, b in ipairs(back) do
      if take_forgotten(path, meta, b.h) then
        state.events[#state.events + 1] = hunk_event('unrevert', ov, b.h, b.row)
      end
    end
  else
    -- Nothing is drawn or followed, so nothing is kept.
    pcall(api.nvim_buf_clear_namespace, buf, state.ns, 0, -1)
  end
  return status({
    drawn = drawn,
    kept = #list,
    skipped = skipped,
    notice = notice,
  })
end

-- Only into a buffer that is already loaded: loading one here could stop at a swap-file prompt,
-- and would draw into a buffer the user cannot see.
function state.show(path, meta, hunks)
  check_path(path)
  check_meta(meta)
  check_hunks(hunks)
  local buf = find_buf(path)
  if not buf or not api.nvim_buf_is_loaded(buf) then
    return status({ drawn = 0, skipped = 0, notice = 'the file is not open in the editor' })
  end
  return show_into(buf, path, meta, hunks)
end

function state.open_and_show(path, line, meta, hunks)
  check_path(path)
  if line == vim.NIL then
    line = nil
  end
  if hunks == vim.NIL then
    hunks = nil
  end
  if line ~= nil and type(line) ~= 'number' then
    error('eitri review: the line must be a number')
  end
  check_meta(meta)
  if hunks ~= nil then
    check_hunks(hunks)
  end
  local buf = find_buf(path)
  if buf == nil and not vim.uv.fs_stat(path) then
    return status({ opened = false, error = 'the file does not exist' })
  end
  -- Editing the current file again would read it again and clear its overlay as "reloaded".
  if buf ~= api.nvim_get_current_buf() then
    local cmd = { cmd = 'edit', args = { path }, magic = { file = false } }
    local ok, err = pcall(api.nvim_cmd, cmd, {})
    if not ok and tostring(err):find('E37:', 1, true) then
      cmd.cmd = 'split'
      ok, err = pcall(api.nvim_cmd, cmd, {})
    end
    if not ok then
      return status({ opened = false, error = tostring(err) })
    end
  end
  local cur = api.nvim_get_current_buf()
  if line ~= nil then
    local l = math.max(1, math.min(math.floor(line), api.nvim_buf_line_count(cur)))
    api.nvim_win_set_cursor(0, { l, 0 })
  end
  local answer = { opened = true, active = active_count() }
  if hunks ~= nil then
    local shown = show_into(cur, path, meta, hunks)
    for k, v in pairs(shown) do
      answer[k] = v
    end
  end
  return status(answer)
end

local function must_leave(buf)
  if vim.bo[buf].modified then return true end
  local ov = state.overlays[buf]
  if ov then
    if not ov.live then return true end
    for _, d in ipairs(ov.hunks) do
      if d.ghost then return true end
    end
  end
  return false
end

function state.live_show(path, meta, hunks)
  check_path(path)
  check_meta(meta)
  check_hunks(hunks)
  if type(meta.turn_id) ~= 'string' or type(meta.running) ~= 'boolean'
    or type(meta.read_lines) ~= 'number' or meta.read_lines < 0 or meta.read_lines % 1 ~= 0
    or meta.scope ~= 'turn' then
    error('eitri review: malformed live meta')
  end
  local buf = find_buf(path)
  local answer = { drawn = 0, shown = is_shown(buf), left = false, skipped = 0 }
  if not answer.shown then return status(answer) end
  if must_leave(buf) then
    answer.left = true
    -- The turn ended but its exact hunks cannot go over this buffer: its drawing from the running
    -- turn would otherwise stay marked running, and undo would keep refusing for a turn that is over.
    local ov = state.overlays[buf]
    if not meta.running and ov and ov.live and ov.running and ov.meta.session == meta.session
      and ov.meta.turn_id == meta.turn_id then
      remove_overlay(buf, 'settled')
    end
    return status(answer)
  end
  local autoread = api.nvim_get_option_value('autoread', { buf = buf })
  -- Unset in the buffer: the global value, not the current buffer's (`vim.o` reads that one's local).
  if autoread == nil or autoread == -1 then autoread = vim.go.autoread end
  local before = state.overlays[buf]
  if autoread then
    state.reloading[buf] = true
    pcall(api.nvim_cmd, { cmd = 'checktime', args = { tostring(buf) } }, {})
    state.reloading[buf] = nil
  end
  -- Reload autocommands belong to the person's configuration and can change either buffer state.
  answer.shown = is_shown(buf)
  if not answer.shown or must_leave(buf) then
    answer.left = answer.shown
    -- The reload took the old drawing off without an event, as one replacing it was due; none is.
    if before and not state.overlays[buf] then queue_off(before.path, 'reload') end
    return status(answer)
  end
  remove_overlay(buf, nil)
  local filtered = {}
  local kept = kept_hunks(path, meta)
  for _, h in ipairs(hunks) do
    if not kept or not kept[fingerprint(h)] then filtered[#filtered + 1] = h end
  end
  -- A file the job read as empty is one empty line here; a line with text means it was written since.
  local emptied_over_text = meta.read_lines == 0 and api.nvim_buf_get_lines(buf, 0, 1, true)[1] ~= ''
  if not autoread or api.nvim_buf_line_count(buf) ~= math.max(1, meta.read_lines) or emptied_over_text then
    answer.skipped = #filtered
    return status(answer)
  end
  local drawn = show_into(buf, path, meta, filtered, true)
  answer.drawn = drawn.drawn
  answer.skipped = drawn.skipped
  return status(answer)
end

function state.live_clear(path)
  check_path(path)
  local buf = find_buf(path)
  local ov = buf and state.overlays[buf]
  local cleared = ov ~= nil and ov.live == true
  if cleared then remove_overlay(buf, nil) end
  return status({ cleared = cleared })
end

function state.clear(path)
  check_path(path)
  local buf = find_buf(path)
  if buf then
    remove_overlay(buf, nil)
  end
  return status({})
end

function state.clear_all()
  for buf in pairs(state.overlays) do
    remove_overlay(buf, nil)
  end
  return status({})
end

-- The oldest events, in order, as many as fit in one answer (at least one); `more` says some wait.
function state.take_events()
  local all, out, used = state.events, {}, 0
  local taken = 0
  for i = 1, #all do
    local size = event_size(all[i])
    if i > 1 and used + size > TAKE_EVENTS_BYTES then
      break
    end
    out[i] = all[i]
    used = used + size
    taken = i
  end
  local rest = {}
  for i = taken + 1, #all do
    rest[#rest + 1] = all[i]
  end
  state.events = rest
  return status({ events = out, more = #rest > 0 })
end

-- One set of autocommands for the whole module, never one per buffer; each returns at once for a
-- buffer without an overlay. They exist only while some buffer shows an overlay: the first overlay
-- registers them and the removal of the last one deletes them, so a closed review leaves no
-- autocommand behind. What stays in the editor until the module's own teardown is the module table
-- `_G.__eitri_review`: it holds the events no poll has taken yet (the close of the last overlay is
-- one), and its owner and version are what the next install and every later call are checked
-- against. A callback must not return a true value (that deletes it).
watch = function()
  if state.augroup then
    return
  end
  local group = api.nvim_create_augroup('eitri_review', { clear = true })
  state.augroup = group

  api.nvim_create_autocmd({ 'TextChanged', 'TextChangedI' }, {
    group = group,
    callback = function(args)
      if not state.torn then
        follow_edits(args.buf, args.event == 'TextChanged')
      end
    end,
  })

  -- `:edit!` and a reload after the file changed on disk unload the buffer and read it again.
  api.nvim_create_autocmd('BufReadPost', {
    group = group,
    callback = function(args)
      if state.torn or not state.overlays[args.buf] then
        return
      end
      local live = state.overlays[args.buf].live
      local why = not state.reloading[args.buf] and 'reload' or nil
      remove_overlay(args.buf, why)
      if not live then
        say('the review overlay was cleared: the file was reloaded', vim.log.levels.INFO)
      end
    end,
  })

  api.nvim_create_autocmd({ 'BufDelete', 'BufWipeout' }, {
    group = group,
    callback = function(args)
      if state.torn or not state.overlays[args.buf] then
        return
      end
      remove_overlay(args.buf, 'closed')
    end,
  })

  -- An unload is also the first half of a reload, so whether the buffer was closed is decided once
  -- the reload, if any, has had its turn.
  api.nvim_create_autocmd('BufUnload', {
    group = group,
    callback = function(args)
      local b = args.buf
      if state.torn or not state.overlays[b] then
        return
      end
      vim.schedule(function()
        if not state.torn and state.overlays[b] and not api.nvim_buf_is_loaded(b) then
          remove_overlay(b, 'closed')
        end
      end)
    end,
  })
end

unwatch = function()
  local group = state.augroup
  state.augroup = nil
  if group then
    pcall(api.nvim_del_augroup_by_id, group)
  end
end

-- Takes every overlay off without an event: the panel the events were for is going.
local function teardown()
  if state.torn then
    return
  end
  state.torn = true
  for buf in pairs(state.overlays) do
    pcall(remove_overlay, buf, nil)
  end
  state.overlays = {}
  state.kept = {}
  state.kept_order = {}
  state.kept_turns = {}
  state.kept_count = 0
  unwatch()
  if rawget(_G, '__eitri_review') == state then
    _G.__eitri_review = nil
  end
  if comp then
    for i = #comp.teardowns, 1, -1 do
      if comp.teardowns[i] == teardown then
        table.remove(comp.teardowns, i)
      end
    end
  end
end
state.teardown = teardown

_G.__eitri_review = state
if comp then
  comp.teardowns[#comp.teardowns + 1] = teardown
end

return { installed = true, replaced = replaced }
