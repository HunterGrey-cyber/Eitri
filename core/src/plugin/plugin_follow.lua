-- Lets a plugin's page follow the buffer the user is reading: defines the plugin's declared nvim commands
-- (never replacing the user's own), reports the followed buffer's view over a socket while asked to, answers
-- text pulls within size caps, scrolls the followed window, and moves the cursor to a line. Called with
-- (op, opts); every value is an argument and nothing here is built from text it receives. One state per
-- owner (a plugin id). Runs on LuaJIT and PUC 5.1.
local op, opts = ...

-- A stale connection cannot change the current holder's glue, including during editor startup.
-- With no id the reference names the agent slot; an id names a page slot.
local slot
if opts.slot ~= nil then
  local ref = opts.slot
  if type(ref) ~= 'table' or type(ref.chan) ~= 'number' then
    return { ok = false, slot = false }
  end
  if ref.id == nil then
    slot = rawget(_G, '__eitri_companion')
  elseif type(ref.id) == 'string' then
    local pages = rawget(_G, '__eitri_pages')
    slot = pages and pages[ref.id]
  end
  if type(slot) ~= 'table' or slot.torn or slot.chan ~= ref.chan or type(slot.adopt) ~= 'function' then
    return { ok = false, slot = false }
  end
  if op ~= 'install' then
    local all = rawget(_G, '__eitri_plugin_follow')
    local st = all and all[opts.owner]
    if st == nil or st.torn or st.slot_chan ~= ref.chan or st.slot_id ~= ref.id then
      return { ok = false, slot = false }
    end
  end
end

local uv = vim.uv or vim.loop

-- View lines are throttled to one per VIEW_MS per owner, leading and trailing: fast enough for scroll sync,
-- few enough that holding a motion key does not flood the host.
local VIEW_MS = 40
-- A view whose top is the one a reveal set, within this long of it, is that reveal's echo.
local ECHO_MS = 150

local DOES = { show = true, toggle = true, hide = true }

local function state_of(owner)
  local all = rawget(_G, '__eitri_plugin_follow')
  if all == nil or type(owner) ~= 'string' then
    return nil
  end
  local st = all[owner]
  if st == nil or st.torn then
    return nil
  end
  return st
end

-- The loop's cached time lags while a callback runs; the throttle and the echo window need the real one.
local function now()
  return uv.hrtime() / 1e6
end

-- One JSON line per connection. A connect error drops the line: the host may be gone, and nvim must not wait
-- for it.
local function send(st, msg)
  local ok, payload = pcall(vim.json.encode, msg)
  if not ok or type(payload) ~= 'string' then
    return
  end
  local pipe = uv.new_pipe(false)
  if not pipe then
    return
  end
  local started = pcall(pipe.connect, pipe, st.socket, function(err)
    if err then
      pipe:close()
      return
    end
    pipe:write(payload .. '\n', function()
      pipe:close()
    end)
  end)
  if not started and not pipe:is_closing() then
    pipe:close()
  end
end

-- A buffer this owner may follow: a normal buffer, of a listed filetype, that is not one of Eitri's scratch
-- buffers.
local function followable(st, buf)
  if type(buf) ~= 'number' then
    return false
  end
  if not vim.api.nvim_buf_is_valid(buf) or not vim.api.nvim_buf_is_loaded(buf) then
    return false
  end
  if vim.bo[buf].buftype ~= '' or not st.filetypes[vim.bo[buf].filetype] then
    return false
  end
  local name = vim.api.nvim_buf_get_name(buf)
  local prefix = st.scratch_prefix
  if prefix and name:sub(1, #prefix) == prefix then
    return false
  end
  local dir = st.scratch_dir
  if dir and name ~= '' and (name == dir or name:sub(1, #dir + 1) == dir .. '/') then
    return false
  end
  return true
end

local function tiled(w)
  return vim.api.nvim_win_get_config(w).relative == ''
end

-- The window that shows `buf`: the current one, else a tiled window of the current tab page, else any tiled
-- window.
local function window_for(buf)
  local cur = vim.api.nvim_get_current_win()
  if vim.api.nvim_win_get_buf(cur) == buf then
    return cur
  end
  for _, w in ipairs(vim.api.nvim_tabpage_list_wins(0)) do
    if vim.api.nvim_win_get_buf(w) == buf and tiled(w) then
      return w
    end
  end
  for _, w in ipairs(vim.fn.win_findbuf(buf)) do
    if tiled(w) then
      return w
    end
  end
  return nil
end

-- The window's top as a buffer line (a closed fold's first line) and the hidden share of a wrapped top line
-- that 'smoothscroll' has partly scrolled away.
local function top_of(w)
  local view = vim.api.nvim_win_call(w, vim.fn.winsaveview)
  local top = view.topline
  local fold = vim.api.nvim_win_call(w, function()
    return vim.fn.foldclosed(top)
  end)
  if fold ~= -1 then
    return fold, 0
  end
  local skip = view.skipcol or 0
  -- Without 'smoothscroll' nvim may still skip the start of a line too long for the window; the reader sees
  -- that line from its start, so no fraction is reported.
  if skip <= 0 or not vim.wo[w].smoothscroll or type(vim.api.nvim_win_text_height) ~= 'function' then
    return top, 0
  end
  local rows = vim.api.nvim_win_text_height(w, { start_row = top - 1, end_row = top - 1 }).all
  local rest = vim.api.nvim_win_text_height(w, { start_row = top - 1, end_row = top - 1, start_vcol = skip }).all
  if rows <= 0 or rest >= rows then
    return top, 0
  end
  return top, (rows - rest) / rows
end

local changed

-- Edits to the followed buffer reach the view watch even while no window shows it: a plugin's
-- nvim_buf_set_lines or a reload of a hidden buffer fires none of the view events, and the host pulls text only
-- when a view's tick moves. The callback detaches itself once the buffer is no longer the followed one.
local function watch_text(st, buf)
  st.attached = st.attached or {}
  if st.attached[buf] then
    return
  end
  local function on_change()
    if st.torn or not st.following or st.buf ~= buf then
      st.attached[buf] = nil
      return true
    end
    if not st.text_queued then
      st.text_queued = true
      -- Buffer callbacks run under textlock; the view is built on the main loop.
      vim.schedule(function()
        if st.torn then
          return
        end
        st.text_queued = false
        if st.following then
          changed(st)
        end
      end)
    end
  end
  local ok, attached = pcall(vim.api.nvim_buf_attach, buf, false, {
    on_lines = on_change,
    on_reload = on_change,
    on_detach = function()
      st.attached[buf] = nil
    end,
  })
  if ok and attached then
    st.attached[buf] = true
  end
end

local function clamp(n, lo, hi)
  return math.max(lo, math.min(n, hi))
end

-- The window's effective 'scrolloff' in screen lines: the window's own value, else the global one, at most what
-- the window's height lets nvim keep (a large value keeps the cursor in the middle).
local function scrolloff_of(w)
  local so = vim.wo[w].scrolloff
  if so == nil or so < 0 then
    so = vim.go.scrolloff
  end
  local half = math.floor((vim.api.nvim_win_get_height(w) - 1) / 2)
  return math.max(0, math.min(so, half))
end

local function send_view(st)
  st.last_sent = now()
  st.dirty = false
  local buf = st.buf
  if buf == nil then
    return
  end
  if vim.api.nvim_buf_is_valid(buf) and not vim.api.nvim_buf_is_loaded(buf) then
    -- An unloaded buffer ('nohidden', 'bufhidden', :bunload) keeps its handle and comes back loaded when it is
    -- edited again; the page keeps its last text meanwhile. No window shows it, so there is no view to send.
    return
  end
  if not followable(st, buf) then
    -- Wiped, renamed into the scratch directory, or its filetype changed: it is no longer followed, and its name
    -- is not sent again.
    st.buf = nil
    st.reveal = nil
    send(st, { v = 1, k = 'gone', owner = st.owner, buf = buf })
    return
  end
  watch_text(st, buf)
  local w = window_for(buf)
  if w == nil then
    -- No window shows it, so there is no position to sync, but the page still keeps it live: an edit moves the
    -- tick, and it is no longer current. The position last reported stands, kept inside the buffer's lines; a
    -- view saying nothing new is not sent.
    local last = st.last_view
    local tick = vim.api.nvim_buf_get_changedtick(buf)
    local count = vim.api.nvim_buf_line_count(buf)
    if last == nil or last.buf ~= buf or (last.tick == tick and not last.current and last.line_count == count) then
      return
    end
    st.seq = st.seq + 1
    local msg = {
      v = 1,
      k = 'view',
      owner = st.owner,
      seq = st.seq,
      buf = buf,
      name = vim.api.nvim_buf_get_name(buf),
      filetype = vim.bo[buf].filetype,
      tick = tick,
      current = false,
      line_count = count,
      top = clamp(last.top, 1, count),
      top_fraction = last.top <= count and last.top_fraction or 0,
      bottom = clamp(last.bottom, 1, count),
      cursor = { line = clamp(last.cursor.line, 1, count), col = last.cursor.col },
      cause = 'user',
      scrolloff = last.scrolloff,
      rows = last.rows,
      cursor_row = last.cursor_row,
    }
    st.last_view = msg
    send(st, msg)
    return
  end
  local top, fraction = top_of(w)
  local c = vim.api.nvim_win_get_cursor(w)
  -- The screen row the cursor's line starts on. Under 'wrap' that is its first column's row, or the window's first
  -- row when the line starts above the window ('smoothscroll'); without 'wrap' each line is one row, and its first
  -- column may be scrolled out sideways, so the cursor's own row is the line's.
  local cursor_row
  if vim.wo[w].wrap then
    cursor_row = vim.fn.screenpos(w, c[1], 1).row
    if cursor_row == 0 then
      cursor_row = vim.fn.win_screenpos(w)[1]
    end
  else
    cursor_row = vim.fn.win_screenpos(w)[1] + vim.api.nvim_win_call(w, vim.fn.winline) - 1
  end
  st.seq = st.seq + 1
  local msg = {
    v = 1,
    k = 'view',
    owner = st.owner,
    seq = st.seq,
    buf = buf,
    name = vim.api.nvim_buf_get_name(buf),
    filetype = vim.bo[buf].filetype,
    tick = vim.api.nvim_buf_get_changedtick(buf),
    current = w == vim.api.nvim_get_current_win(),
    line_count = vim.api.nvim_buf_line_count(buf),
    top = top,
    top_fraction = fraction,
    bottom = vim.fn.line('w$', w),
    cursor = { line = c[1], col = c[2] },
    cause = 'user',
    scrolloff = scrolloff_of(w),
    rows = vim.o.lines,
    cursor_row = cursor_row,
  }
  local r = st.reveal
  if r ~= nil and r.buf == buf and r.top == top and now() - r.at <= ECHO_MS then
    msg.cause = 'reveal'
    msg.token = r.token
  end
  st.last_view = msg
  send(st, msg)
end

local function arm(st, ms)
  st.armed = true
  local timer = st.timer
  timer:start(math.max(1, math.ceil(ms)), 0, function()
    -- A libuv callback may not call the API; the work hops to the main loop first.
    vim.schedule(function()
      if st.torn or not st.following or st.timer ~= timer then
        return
      end
      st.armed = false
      if st.dirty then
        send_view(st)
      end
    end)
  end)
end

-- Something about the view may have changed: the current buffer becomes the followed one when it may be, and a
-- view line goes out at once or at the end of the throttle window.
function changed(st)
  if st.torn or not st.following then
    return
  end
  local cur = vim.api.nvim_get_current_buf()
  if followable(st, cur) then
    st.buf = cur
  end
  if st.armed then
    st.dirty = true
    return
  end
  local wait = VIEW_MS - (now() - st.last_sent)
  if wait <= 0 then
    send_view(st)
  else
    st.dirty = true
    arm(st, wait)
  end
end

local function auto_check(st)
  if st.torn then
    return
  end
  local buf = vim.api.nvim_get_current_buf()
  if buf ~= st.auto_last and followable(st, buf) then
    st.auto_last = buf
    send(st, { v = 1, k = 'cmd', owner = st.owner, cmd = 'auto', buf = buf })
  end
end

local function stop_following(st)
  for _, id in ipairs(st.view_ids or {}) do
    pcall(vim.api.nvim_del_autocmd, id)
  end
  st.view_ids = nil
  if st.timer ~= nil then
    st.timer:stop()
    if not st.timer:is_closing() then
      st.timer:close()
    end
    st.timer = nil
  end
  st.armed = false
  st.dirty = false
  st.following = false
end

local function teardown_state(st)
  st.torn = true
  local all = rawget(_G, '__eitri_plugin_follow')
  if all and all[st.owner] == st then
    all[st.owner] = nil
    -- A slot leaving the user's editor takes its empty registry away with it.
    if st.slot_chan ~= nil and next(all) == nil then
      _G.__eitri_plugin_follow = nil
    end
  end
  pcall(vim.api.nvim_del_augroup_by_name, 'EitriPluginFollow_' .. st.owner)
  stop_following(st)
  -- A command is deleted only while it is still the one this owner defined; nvim reports a Lua command's
  -- description as its `definition`, so a command the user redefined since has another one and stays.
  local have = vim.api.nvim_get_commands({ builtin = false })
  for name, desc in pairs(st.defined) do
    local cmd = have[name]
    if cmd ~= nil and cmd.definition == desc then
      pcall(vim.api.nvim_del_user_command, name)
    end
  end
  return { ok = true }
end

local function teardown(o)
  local st = state_of(o.owner)
  if st ~= nil then
    teardown_state(st)
  end
  return { ok = true }
end

local function install(o)
  -- Before VimEnter the user's config may not have defined its own commands yet, and one defined here first
  -- would make theirs fail or silently replace this one. Nothing is changed; the host asks again.
  if vim.v.vim_did_enter == 0 then
    return { ok = false, early = true }
  end
  local owner = o.owner
  if type(owner) ~= 'string' or owner == '' or type(o.socket) ~= 'string' then
    error('plugin_follow: install needs an owner and a socket')
  end
  local commands = o.commands or {}
  for _, c in ipairs(commands) do
    if type(c[1]) ~= 'string' or not DOES[c[2]] then
      error('plugin_follow: a command is not a name with show, toggle or hide')
    end
  end
  -- A reinstall must not take this owner's own commands for the user's.
  teardown({ owner = owner })
  local all = rawget(_G, '__eitri_plugin_follow')
  if all == nil then
    all = {}
    rawset(_G, '__eitri_plugin_follow', all)
  end
  local filetypes = {}
  for _, f in ipairs(o.filetypes or {}) do
    if type(f) == 'string' then
      filetypes[f] = true
    end
  end
  local dir = o.scratch_dir
  if type(dir) == 'string' and dir ~= '' then
    while #dir > 1 and dir:sub(-1) == '/' do
      dir = dir:sub(1, -2)
    end
  else
    dir = nil
  end
  local st = {
    owner = owner,
    socket = o.socket,
    filetypes = filetypes,
    scratch_dir = dir,
    scratch_prefix = type(o.scratch_prefix) == 'string' and o.scratch_prefix ~= '' and o.scratch_prefix or nil,
    slot_chan = o.slot and o.slot.chan,
    slot_id = o.slot and o.slot.id,
    following = false,
    seq = 0,
    defined = {},
    torn = false,
    armed = false,
    dirty = false,
    last_sent = -VIEW_MS,
  }
  all[owner] = st
  if slot then
    slot.adopt(function()
      if all[owner] == st then
        teardown_state(st)
      end
    end)
  end
  st.group = vim.api.nvim_create_augroup('EitriPluginFollow_' .. owner, { clear = true })

  local skipped = {}
  local global = vim.api.nvim_get_commands({ builtin = false })
  local in_buffer = vim.api.nvim_buf_get_commands(0, { builtin = false })
  for _, c in ipairs(commands) do
    local name, does = c[1], c[2]
    if global[name] ~= nil or in_buffer[name] ~= nil then
      skipped[#skipped + 1] = name
    else
      local desc = 'eitri plugin ' .. owner .. ' ' .. does
      local made = pcall(vim.api.nvim_create_user_command, name, function()
        if st.torn then
          return
        end
        send(st, { v = 1, k = 'cmd', owner = owner, cmd = does })
      end, { nargs = 0, desc = desc })
      if made then
        st.defined[name] = desc
      else
        -- A name nvim keeps for itself cannot be defined; it is reported like one the user has.
        skipped[#skipped + 1] = name
      end
    end
  end

  if o.auto then
    vim.api.nvim_create_autocmd({ 'BufEnter', 'FileType' }, {
      group = st.group,
      callback = function()
        if st.torn then
          return
        end
        auto_check(st)
      end,
    })
    vim.schedule(function()
      if not st.torn then
        auto_check(st)
      end
    end)
  end
  return { ok = true, skipped = skipped }
end

local VIEW_EVENTS = {
  'BufEnter',
  'WinEnter',
  'BufFilePost',
  'FileType',
  'CursorMoved',
  'CursorMovedI',
  'WinScrolled',
  'WinResized',
  'TextChanged',
  'TextChangedI',
  'VimResized',
}

local function follow(o)
  local st = state_of(o.owner)
  if st == nil then
    return { ok = false }
  end
  stop_following(st)
  if not o.on then
    return { ok = true }
  end
  st.following = true
  st.timer = uv.new_timer()
  -- Kept by id so that turning following off removes these and leaves the auto-show watch.
  st.view_ids = {
    vim.api.nvim_create_autocmd(VIEW_EVENTS, {
      group = st.group,
      callback = function()
        if st.torn then
          return
        end
        changed(st)
      end,
    }),
    vim.api.nvim_create_autocmd('BufWipeout', {
      group = st.group,
      callback = function(ev)
        if st.torn or not st.following then
          return
        end
        if ev.buf == st.buf then
          st.buf = nil
          st.reveal = nil
          send(st, { v = 1, k = 'gone', owner = st.owner, buf = ev.buf })
        end
      end,
    }),
  }
  local cur = vim.api.nvim_get_current_buf()
  if followable(st, cur) then
    st.buf = cur
  end
  send_view(st)
  return { ok = true }
end

local function text(o)
  local st = state_of(o.owner)
  local buf = o.buf
  if st == nil or not followable(st, buf) then
    return { gone = true, buf = buf }
  end
  local lines = vim.api.nvim_buf_line_count(buf)
  local bytes = vim.api.nvim_buf_get_offset(buf, lines)
  local tick = vim.api.nvim_buf_get_changedtick(buf)
  -- Measured before reading, so a buffer past the caps is never copied.
  if bytes > (tonumber(o.max_bytes) or 0) or lines > (tonumber(o.max_lines) or 0) then
    return { buf = buf, tick = tick, too_large = true, bytes = bytes, lines = lines }
  end
  return { buf = buf, tick = tick, lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false) }
end

-- The followed window of `buf`, or nil when `buf` is not the buffer this owner follows or no window shows it.
local function followed_window(st, buf)
  if st == nil or buf ~= st.buf or not followable(st, buf) then
    return nil
  end
  return window_for(buf)
end

local function clamp_line(buf, line)
  local n = vim.api.nvim_buf_line_count(buf)
  line = math.floor(tonumber(line) or 1)
  if line < 1 then
    return 1
  end
  if line > n then
    return n
  end
  return line
end

-- Puts `line` at the top of `w`. The cursor moves only as far as it must to stay in view, as a scroll command
-- moves it: nvim would otherwise scroll the window back to the cursor.
local function scroll_to(w, buf, line)
  local n = vim.api.nvim_buf_line_count(buf)
  local so = scrolloff_of(w)
  vim.api.nvim_win_call(w, function()
    local view = vim.fn.winsaveview()
    -- 'scrolloff' keeps no margin above the buffer's first line.
    local first = line
    if line > 1 then
      first = math.min(line + so, n)
    end
    vim.fn.winrestview({ topline = line, lnum = first, skipcol = 0 })
    local bottom = vim.fn.line('w$')
    local last = bottom
    if bottom < n then
      last = math.max(first, bottom - so)
    end
    local lnum = math.min(math.max(view.lnum, first), last)
    local keep = { topline = line, lnum = lnum, skipcol = 0 }
    if lnum == view.lnum then
      keep.col = view.col
      keep.coladd = view.coladd
      keep.curswant = view.curswant
    end
    vim.fn.winrestview(keep)
  end)
end

-- Puts the cursor of `w` on `want`, or on the line nearest it inside the window that keeps the window where it is.
-- nvim scrolls whenever the cursor is within 'scrolloff' screen lines of an edge, and with 'wrap' a line may take
-- several, so each candidate is set and checked against what nvim makes of it (`winline` validates the window),
-- moving toward the window's middle until the top stays. With none found the cursor is left where it was.
local function place_cursor(w, want)
  vim.api.nvim_win_call(w, function()
    local view = vim.fn.winsaveview()
    local top, skip = view.topline, view.skipcol or 0
    local first, last = top, vim.fn.line('w$')
    if first > last then
      return
    end
    local function stays(l)
      local keep = { topline = top, skipcol = skip, lnum = l }
      if l == view.lnum then
        keep.col = view.col
        keep.coladd = view.coladd
        keep.curswant = view.curswant
      else
        keep.col = 0
        keep.curswant = 0
      end
      vim.fn.winrestview(keep)
      vim.fn.winline()
      local now = vim.fn.winsaveview()
      return now.topline == top and (now.skipcol or 0) == skip
    end
    local l = clamp(want, first, last)
    local step = (l - first) <= (last - l) and 1 or -1
    while l >= first and l <= last do
      if stays(l) then
        return
      end
      l = l + step
    end
    vim.fn.winrestview(view)
  end)
end

-- The top line that starts `cursor` on screen row `row` of nvim's grid, as near as the window allows. The rows
-- above it respect 'scrolloff' and count lines as nvim draws them: wrapped lines take their rows, closed folds one
-- (a cursor inside a closed fold starts on the fold's row). A line above too tall for the rows wanted is still taken
-- when without it the cursor would sit within 'scrolloff' of the top, as long as the cursor stays inside the window:
-- nvim would otherwise move the cursor off its line, and a whole line is all a top can start with.
local function top_for_row(w, cursor, row)
  local first = vim.fn.win_screenpos(w)[1]
  local height = vim.api.nvim_win_get_height(w)
  local so = scrolloff_of(w)
  local most = math.max(so, height - 1 - so)
  local want = clamp(row - first, so, most)
  local top, above = cursor, 0
  vim.api.nvim_win_call(w, function()
    local own = vim.fn.foldclosed(cursor)
    if own ~= -1 then
      top = own
    end
    while top > 1 do
      local l = top - 1
      local fold = vim.fn.foldclosed(l)
      local h, start = 1, l
      if fold ~= -1 then
        start = fold
      else
        h = vim.api.nvim_win_text_height(w, { start_row = l - 1, end_row = l - 1 }).all
      end
      if above + h > want then
        if above < so and above + h <= most then
          top = start
        end
        break
      end
      above = above + h
      top = start
    end
  end)
  return top
end

local function reveal(o)
  local st = state_of(o.owner)
  local buf = o.buf
  local w = followed_window(st, buf)
  if w == nil then
    return { applied = false }
  end
  local top_line = clamp_line(buf, o.line)
  if o.row ~= nil and o.cursor ~= nil then
    top_line = top_for_row(w, clamp_line(buf, o.cursor), o.row)
  end
  scroll_to(w, buf, top_line)
  if o.cursor ~= nil then
    place_cursor(w, clamp_line(buf, o.cursor))
  end
  local top = top_of(w)
  -- Bound to its buffer: another buffer whose window happens to start on the same line is not this echo.
  st.reveal = { buf = buf, token = o.token, top = top, at = now() }
  -- A window that is not current may scroll without a WinScrolled before the next redraw, so the echo is
  -- asked for here.
  changed(st)
  return { applied = true }
end

-- The length in bytes of the character at byte `i` of `s`, counted as the page counts it: the page gets the text
-- with each maximal ill-formed UTF-8 subsequence replaced by one U+FFFD, so such a subsequence is one character.
local function char_len(s, i)
  local b = s:byte(i)
  local need, lo, hi
  if b < 0x80 then
    return 1
  elseif b >= 0xC2 and b <= 0xDF then
    need, lo, hi = 1, 0x80, 0xBF
  elseif b == 0xE0 then
    need, lo, hi = 2, 0xA0, 0xBF
  elseif b == 0xED then
    need, lo, hi = 2, 0x80, 0x9F
  elseif b >= 0xE1 and b <= 0xEF then
    need, lo, hi = 2, 0x80, 0xBF
  elseif b == 0xF0 then
    need, lo, hi = 3, 0x90, 0xBF
  elseif b >= 0xF1 and b <= 0xF3 then
    need, lo, hi = 3, 0x80, 0xBF
  elseif b == 0xF4 then
    need, lo, hi = 3, 0x80, 0x8F
  else
    return 1
  end
  for j = 1, need do
    local c = s:byte(i + j)
    if c == nil or c < lo or c > hi then
      return j
    end
    lo, hi = 0x80, 0xBF
  end
  return need + 1
end

-- The byte offset of the first `chars` characters of `s`, stopping at its end.
local function byte_col(s, chars)
  local i, n = 1, 0
  while n < chars and i <= #s do
    i = i + char_len(s, i)
    n = n + 1
  end
  return math.min(i - 1, #s)
end

-- The byte offset (from 0) of character `col` (from 1) of line `line`, and the line; `col` 0 or below is the line's
-- first non-blank character.
local function byte_at(buf, line, col)
  local content = vim.api.nvim_buf_get_lines(buf, line - 1, line, false)[1] or ''
  col = math.floor(tonumber(col) or 0)
  if col <= 0 then
    return (content:find('%S') or 1) - 1, content
  end
  return byte_col(content, col - 1), content
end

-- The first byte of the line's last character (0 on an empty line): where a mark past the line's end is held.
local function last_char(content)
  local i, last = 1, 0
  while i <= #content do
    last = i - 1
    i = i + char_len(content, i)
  end
  return last
end

-- How a selection is made from nvim's mode: `now` from Normal, Visual and Select mode, which run in Normal mode's
-- loop; `keys` from Insert and Replace mode (and Normal mode entered from them with CTRL-O), whose own loop a
-- command run from here cannot end; nil from a command line, a prompt, an operator waiting for its motion or a
-- terminal, which are the user's and are left alone.
local function selecting(mode)
  if mode.blocking then
    return nil
  end
  local m = mode.mode
  local first = m:sub(1, 1)
  if m == 'n' or first == 'v' or first == 'V' or first == '\22' or first == 's' or first == 'S' or first == '\19' then
    return 'now'
  end
  if first == 'i' or first == 'R' or m:sub(1, 2) == 'ni' then
    return 'keys'
  end
  return nil
end

-- Selects from (`line`, `byte`) to the jump's `select_*` place in nvim's own visual mode, the cursor ending at the
-- second place, by setting the '< and '> marks and running gv: the cursor never visits the first place, so the
-- window scrolls only as far as the second needs. Lines are clamped to the buffer and columns to their line.
local function select_to(w, buf, line, byte, content, o, how)
  local to = clamp_line(buf, o.select_line)
  local to_byte, to_content = byte_at(buf, to, o.select_col)
  local linewise = o.select_mode == 'line'
  byte = math.min(byte, last_char(content))
  to_byte = math.min(to_byte, last_char(to_content))
  if not linewise and vim.o.selection == 'exclusive' then
    -- An exclusive selection leaves out the character at its later end: that end goes one character on.
    if to > line or (to == line and to_byte >= byte) then
      if to_byte < #to_content then
        to_byte = to_byte + char_len(to_content, to_byte + 1)
      end
    elseif byte < #content then
      byte = byte + char_len(content, byte + 1)
    end
  end
  local want = linewise and 'V' or 'v'
  local function marks()
    -- gv on a closed fold selects the whole fold: the folds holding either end are opened, as moving the cursor
    -- into them would show them, so the selection is the one asked for.
    vim.api.nvim_win_call(w, function()
      for _, l in ipairs({ line, to }) do
        if vim.fn.foldclosed(l) ~= -1 then
          vim.api.nvim_cmd({ cmd = 'foldopen', range = { l, l }, bang = true }, {})
        end
      end
    end)
    vim.api.nvim_buf_set_mark(buf, '<', line, byte, {})
    vim.api.nvim_buf_set_mark(buf, '>', to, to_byte, {})
  end
  if how == 'keys' then
    -- nvim leaves Insert mode and selects as its next keys; gv takes the buffer's last visual mode, and setting the
    -- marks of a buffer that never had one makes it charwise.
    marks()
    local last = vim.fn.visualmode()
    if last == '' then
      last = 'v'
    end
    local keys = '<C-\\><C-n>gv' .. (last ~= want and want or '')
    vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(keys, true, false, true), 'n', false)
    return { applied = true }
  end
  if vim.api.nvim_get_mode().mode ~= 'n' then
    vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes('<C-\\><C-n>', true, false, true), 'nx', false)
  end
  vim.api.nvim_set_current_win(w)
  marks()
  vim.api.nvim_cmd({ cmd = 'normal', args = { 'gv' }, bang = true }, {})
  if vim.api.nvim_get_mode().mode ~= want then
    vim.api.nvim_cmd({ cmd = 'normal', args = { want }, bang = true }, {})
  end
  return { applied = vim.api.nvim_get_mode().mode == want }
end

local function jump(o)
  local st = state_of(o.owner)
  local buf = o.buf
  local w = followed_window(st, buf)
  if w == nil then
    return { applied = false }
  end
  local how = o.select_line ~= nil and selecting(vim.api.nvim_get_mode()) or nil
  if o.select_line ~= nil and how == nil then
    return { applied = false }
  end
  vim.api.nvim_set_current_win(w)
  local line = clamp_line(buf, o.line)
  local byte, content = byte_at(buf, line, o.col)
  if how ~= nil then
    return select_to(w, buf, line, byte, content, o, how)
  end
  vim.api.nvim_win_set_cursor(w, { line, byte })
  return { applied = true }
end

if op == 'install' then
  return install(opts)
elseif op == 'follow' then
  return follow(opts)
elseif op == 'text' then
  return text(opts)
elseif op == 'reveal' then
  return reveal(opts)
elseif op == 'jump' then
  return jump(opts)
elseif op == 'teardown' then
  return teardown(opts)
end
error('plugin_follow: unknown op ' .. tostring(op))
