-- Eitri's turn review, drawn in the user's own nvim: the hunks of one turn over the buffer of a
-- file the user has open, with keys to move between them, to revert one in the buffer and to close
-- the overlay.
--
-- Installed by one RPC call with the arguments `owner, version`; every later call reaches the
-- module through `_G.__eitri_review`. Paths, hunks and line text arrive only as arguments: they
-- are compared with buffer lines or set as buffer lines, never run, and a file is opened only
-- through `nvim_cmd` with the path as a structured argument and file-name expansion off.
--
-- Nothing here reads or writes a file. A revert changes the buffer only (undoable with `u`, and
-- left unsaved) and is queued as an event, which the host takes by polling. The module then keeps
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
local EOLS = { lf = true, crlf = true, missing = true }
local PRIORITY = 90
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
  torn = false,
}

local function warn(msg)
  vim.notify(msg, vim.log.levels.WARN)
end

local function active_count()
  local n = 0
  for _ in pairs(state.overlays) do
    n = n + 1
  end
  return n
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

-- The buffer that shows `path`: a loaded one first -- of exactly that name, else one naming the
-- same (device, inode) through a link -- and only then an unloaded one, so a listed but unloaded
-- buffer of that name does not hide the loaded one the user is looking at. Never the `bufnr()`
-- function, which reads its argument as a file pattern (`%`, `#`, `*`, `[...]`) and can answer
-- with another file's buffer.
local function find_buf(path)
  local bufs = api.nvim_list_bufs()
  local exact = nil
  for _, b in ipairs(bufs) do
    if api.nvim_buf_get_name(b) == path then
      if api.nvim_buf_is_loaded(b) then
        return b
      end
      exact = exact or b
    end
  end
  local want = vim.uv.fs_stat(path)
  local twin = nil
  if want then
    for _, b in ipairs(bufs) do
      local name = api.nvim_buf_get_name(b)
      if b ~= exact and name ~= '' and name ~= path then
        local st = vim.uv.fs_stat(name)
        if st and st.dev == want.dev and st.ino == want.ino then
          if api.nvim_buf_is_loaded(b) then
            return b
          end
          twin = twin or b
        end
      end
    end
  end
  return exact or twin
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
-- ending) as this buffer would hold the file's lines, final newline included: a line without one must be the buffer's last with
-- 'eol' off, and a last line with one needs 'eol' on. A 'binary' buffer is never compared.
local function holds_side(buf, row, n, lines, eols)
  if vim.bo[buf].binary then
    return false
  end
  local count = api.nvim_buf_line_count(buf)
  if row < 0 or row + n > count then
    return false
  end
  local ff = vim.bo[buf].fileformat
  local ends = row + n == count
  local eol = vim.bo[buf].eol
  local got = api.nvim_buf_get_lines(buf, row, row + n, true)
  for i = 1, n do
    local e = eols[i]
    local want = expected(lines[i], e, ff)
    if want == nil or got[i] ~= want then
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

local function holds_new_side(buf, row, h)
  return holds_side(buf, row, h.new_len, h.new_lines, h.new_eols)
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

local function keyed(lines, eols)
  local parts = {}
  for i = 1, #lines do
    parts[i] = eols[i] .. '\1' .. lines[i] .. '\n'
  end
  return table.concat(parts)
end

-- What a removed line looks like in a virtual line: control characters made printable, so a raw
-- carriage return never reaches the grid.
local function display(text)
  local ok, shown = pcall(vim.fn.strtrans, text)
  if ok and type(shown) == 'string' then
    return shown
  end
  return text
end

-- Draws one hunk whose new side is at `row`. A hunk carries context lines and may hold more than
-- one change, so the added and removed lines are found by diffing its two sides (each line keyed
-- with its terminator, so a change of ending alone shows too).
local function draw_hunk(buf, row, h)
  local count = api.nvim_buf_line_count(buf)
  -- `row` is where the hunk was last seen: an edit that replaces the whole hunk (a redo of its
  -- revert) takes the tracker with it, and this is where the base text is looked for then.
  local d = { hunk = h, marks = {}, alive = true, row = row }
  local function mark(r, opts)
    opts.priority = PRIORITY
    local id = api.nvim_buf_set_extmark(buf, state.ns, r, 0, opts)
    d.marks[#d.marks + 1] = id
    return id
  end
  -- Follows the hunk's lines as the buffer is edited: a line added just after it does not widen it.
  d.tracker = mark(row, { end_row = row + h.new_len, end_col = 0, end_right_gravity = false })
  local first = nil
  local subs = textdiff(keyed(h.old_lines, h.old_eols), keyed(h.new_lines, h.new_eols), { result_type = 'indices' })
  for _, sub in ipairs(subs or {}) do
    local sa, ca, sb, cb = sub[1], sub[2], sub[3], sub[4]
    for i = 0, cb - 1 do
      mark(row + sb - 1 + i, { line_hl_group = 'DiffAdd' })
    end
    -- With no new lines, `sb` is the new line the removal follows.
    local anchor = cb > 0 and (row + sb - 1) or (row + sb)
    if ca > 0 then
      local virt = {}
      for i = sa, sa + ca - 1 do
        virt[#virt + 1] = { { display(h.old_lines[i]), 'DiffDelete' } }
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

-- The buffer-local maps of `lhs` that the overlay's keys shadow are saved and come back when it
-- goes; `maparg` also reports global maps, which are left alone, and `maparg`/`mapset` work on the
-- current buffer, hence `nvim_buf_call`.
local function set_keys(buf, ov, actions)
  api.nvim_buf_call(buf, function()
    for _, lhs in ipairs(KEYS) do
      local was = vim.fn.maparg(lhs, 'n', false, true)
      local saved = (type(was) == 'table' and was.buffer == 1) and was or nil
      vim.keymap.set('n', lhs, actions[lhs], { buffer = buf, desc = DESC[lhs], silent = true })
      local mine = vim.fn.maparg(lhs, 'n', false, true)
      ov.keys[#ov.keys + 1] = { ours = mine.lhs, desc = DESC[lhs], saved = saved }
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

-- Takes the overlay off `buf`. With `why`, an `off` event says so; without, the host asked for it.
remove_overlay = function(buf, why)
  local ov = state.overlays[buf]
  if not ov then
    return
  end
  state.overlays[buf] = nil
  pcall(api.nvim_buf_clear_namespace, buf, state.ns, 0, -1)
  restore_keys(buf, ov)
  if why then
    state.events[#state.events + 1] = { kind = 'off', path = ov.path, why = why }
  end
  if active_count() == 0 then
    unwatch()
  end
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

-- After an edit of `buf`: a hunk whose lines were changed is no longer drawn, unless the change
-- put the base text back (a redo of a revert, or the user typing it), which reads as the revert;
-- and a hunk reverted here whose own text is back (an undo, or typing it again) is drawn again and
-- reported, so that the revert recorded for it can be dropped.
local MAX_SEARCH_LINES = 20000

-- The one row where `h`'s new side is in `buf` and its old side is not, or nil when there is none
-- or more than one place (an edit that moved whole text, such as a formatter or `:%!`, left both
-- marks elsewhere; two places cannot be told apart). A large buffer is not searched.
local function find_new_side(buf, h)
  local count = api.nvim_buf_line_count(buf)
  if count > MAX_SEARCH_LINES or h.new_len == 0 then
    return nil
  end
  local first = expected(h.new_lines[1], h.new_eols[1], vim.bo[buf].fileformat)
  if first == nil then
    return nil
  end
  local found = nil
  for row, line in ipairs(api.nvim_buf_get_lines(buf, 0, -1, true)) do
    if line == first and holds_new_side(buf, row - 1, h)
      and not holds_side(buf, row - 1, h.old_len, h.old_lines, h.old_eols) then
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
  for i, d in ipairs(ov.hunks) do
    local h = d.hunk
    if d.ghost then
      -- A hunk whose new side is a leading part of its old side (it removed lines at the end of the
      -- file) holds both at once; its text cannot be told from the revert, so it stays reverted.
      local back = nil
      for _, s in ipairs(ghost_rows(buf, d)) do
        if back == nil and holds_new_side(buf, s, h) and not holds_side(buf, s, h.old_len, h.old_lines, h.old_eols) then
          back = s
        end
      end
      if back == nil and search then
        back = find_new_side(buf, h)
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
      if not intact or not holds_new_side(buf, s, h) then
        -- Where the base text may have come back: where the tracker is, and where the hunk was
        -- last seen intact (an edit that replaced the whole hunk left the tracker at its end).
        local back = nil
        for _, at in ipairs({ s or d.row, d.row }) do
          if back == nil and at < api.nvim_buf_line_count(buf) and revert_plan(buf, at, h, h.old_len)
            and holds_side(buf, at, h.old_len, h.old_lines, h.old_eols) then
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
}

-- Draws `hunks` into `buf`, replacing the overlay it had (no event: the host asked). A hunk whose
-- new side is not in the buffer as given is skipped, never drawn over other text; one with no new
-- lines has nothing to draw over.
local function show_into(buf, path, meta, hunks)
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
  local drawn, skipped, list = 0, 0, {}
  local ok, err = pcall(function()
    for _, h in ipairs(hunks) do
      if h.new_len > 0 then
        local row = start_row(h)
        if holds_new_side(buf, row, h) then
          list[#list + 1] = draw_hunk(buf, row, h)
          drawn = drawn + 1
        elseif carried[h.id] and vim.deep_equal(carried[h.id].hunk, h) then
          local d = { hunk = h, marks = {}, alive = false }
          make_ghost(buf, d, carried[h.id].row, carried[h.id].row2)
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
    local ov = { path = path, meta = meta, hunks = list, keys = {} }
    state.overlays[buf] = ov
    set_keys(buf, ov, ACTIONS)
    watch()
  else
    -- Nothing is drawn or followed, so nothing is kept.
    pcall(api.nvim_buf_clear_namespace, buf, state.ns, 0, -1)
  end
  return { drawn = drawn, kept = #list, skipped = skipped, notice = notice, active = active_count() }
end

-- Only into a buffer that is already loaded: loading one here could stop at a swap-file prompt,
-- and would draw into a buffer the user cannot see.
function state.show(path, meta, hunks)
  check_path(path)
  check_meta(meta)
  check_hunks(hunks)
  local buf = find_buf(path)
  if not buf or not api.nvim_buf_is_loaded(buf) then
    return { drawn = 0, skipped = 0, notice = 'the file is not open in the editor', active = active_count() }
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
    return { opened = false, error = 'the file does not exist', active = active_count() }
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
      return { opened = false, error = tostring(err), active = active_count() }
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
  return answer
end

function state.clear(path)
  check_path(path)
  local buf = find_buf(path)
  if buf then
    remove_overlay(buf, nil)
  end
  return { active = active_count() }
end

function state.clear_all()
  for buf in pairs(state.overlays) do
    remove_overlay(buf, nil)
  end
  return { active = active_count() }
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
  return { events = out, active = active_count(), more = #rest > 0 }
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
      remove_overlay(args.buf, 'reload')
      vim.notify('the review overlay was cleared: the file was reloaded', vim.log.levels.INFO)
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
