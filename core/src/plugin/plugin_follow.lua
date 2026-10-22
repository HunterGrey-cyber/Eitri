-- Lets a plugin's page follow the buffer the user is reading: defines the plugin's declared nvim commands
-- (never replacing the user's own), reports the followed buffer's view over a socket while asked to, answers
-- text pulls within size caps, scrolls the followed window, and moves the cursor to a line. Called with
-- (op, opts); every value is an argument and nothing here is built from text it receives. One state per
-- owner (a plugin id). Runs on LuaJIT and PUC 5.1.
local op, opts = ...

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
        st.text_queued = false
        if not st.torn and st.following then
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
    }
    st.last_view = msg
    send(st, msg)
    return
  end
  local top, fraction = top_of(w)
  local c = vim.api.nvim_win_get_cursor(w)
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

local function teardown(o)
  local all = rawget(_G, '__eitri_plugin_follow')
  local st = all and all[o.owner]
  if st == nil then
    return { ok = true }
  end
  st.torn = true
  all[o.owner] = nil
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
    following = false,
    seq = 0,
    defined = {},
    torn = false,
    armed = false,
    dirty = false,
    last_sent = -VIEW_MS,
  }
  all[owner] = st
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

local function scrolloff_of(w)
  local so = vim.wo[w].scrolloff
  if so == nil or so < 0 then
    so = vim.go.scrolloff
  end
  local half = math.floor((vim.api.nvim_win_get_height(w) - 1) / 2)
  return math.max(0, math.min(so, half))
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

local function reveal(o)
  local st = state_of(o.owner)
  local buf = o.buf
  local w = followed_window(st, buf)
  if w == nil then
    return { applied = false }
  end
  scroll_to(w, buf, clamp_line(buf, o.line))
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

local function jump(o)
  local st = state_of(o.owner)
  local buf = o.buf
  local w = followed_window(st, buf)
  if w == nil then
    return { applied = false }
  end
  vim.api.nvim_set_current_win(w)
  local line = clamp_line(buf, o.line)
  local content = vim.api.nvim_buf_get_lines(buf, line - 1, line, false)[1] or ''
  local col = math.floor(tonumber(o.col) or 0)
  local byte
  if col <= 0 then
    byte = (content:find('%S') or 1) - 1
  else
    byte = byte_col(content, col - 1)
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
