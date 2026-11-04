-- Reloads for an agent slot. The returned calls and teardown keep the timer alive
-- without sharing a global with the embedded editor's unconditional reload timer.
local opts = ...
opts = opts or {}
local always = opts.always == true
local torn = false
local timer
local group = vim.api.nvim_create_augroup("EitriBufferReload", { clear = true })

-- What this part knows about each buffer it has met: the name it had, and whether its file has been seen
-- since the buffer last looked like one opened on a missing file. A buffer number is never reused.
local files = {}

-- Reads the buffer's file again with :edit, the one command that clears nvim's mark of a buffer opened on a
-- missing file without asking, and returns whether it ran. :edit does more than a reload, so the call is
-- narrowed and every option it sets is put back, error or not:
--   * 'shortmess' gains A: :edit looks for swap files and would open the ATTENTION dialog for one left by
--     another nvim; checktime never does;
--   * 'eventignore' gains BufEnter, BufLeave, WinEnter and WinLeave, and 'autochdir' is off, so the
--     person's working directory is not moved to a background buffer's folder (putting 'autochdir' back
--     sets it to the current buffer's folder again); BufRead*, FileType and the rest still run, as for any
--     read. 'eventignore' is set without :noautocmd (which would overwrite it), so OptionSet fires for it
--     twice, once going in and once going out;
--   * keepalt and keepjumps leave the alternate file and the jumplist alone;
--   * 'readonly' and 'buflisted' are put back if :edit changed them.
-- Left as it is: BufUnload and BufWinEnter fire, nvim_buf_attach listeners are detached (a client
-- re-attaches), and the file type, 'fileformat' and 'fileencoding' are detected again.
local function reread(b)
  local function put(name, value)
    vim.api.nvim_cmd({ cmd = "set", args = { name .. "=" .. vim.fn.escape(value, " \\|\"") }, mods = { noautocmd = true } }, {})
  end
  local function flag(name, on)
    vim.api.nvim_cmd({ cmd = "set", args = { (on and "" or "no") .. name }, mods = { noautocmd = true } }, {})
  end
  local short, ignore, chdir = vim.go.shortmess, vim.go.eventignore, vim.go.autochdir
  local readonly, listed = vim.bo[b].readonly, vim.bo[b].buflisted
  local ok = pcall(function()
    put("shortmess", short .. "A")
    vim.go.eventignore = (ignore == "" and "" or ignore .. ",") .. "BufEnter,BufLeave,WinEnter,WinLeave"
    flag("autochdir", false)
    vim.api.nvim_buf_call(b, function()
      vim.cmd("silent keepalt keepjumps edit")
    end)
  end)
  pcall(put, "shortmess", short)
  pcall(function()
    vim.go.eventignore = ignore
  end)
  pcall(flag, "autochdir", chdir)
  pcall(function()
    vim.api.nvim_buf_call(b, function()
      if readonly and not vim.bo[b].readonly then
        vim.api.nvim_cmd({ cmd = "setlocal", args = { "readonly" }, mods = { noautocmd = true } }, {})
      end
      if not listed and vim.bo[b].buflisted then
        vim.api.nvim_cmd({ cmd = "setlocal", args = { "nobuflisted" }, mods = { noautocmd = true } }, {})
      end
    end)
  end)
  return ok
end

-- How checktime is to meet this shown, unmodified, autoread buffer: "check" (hand it to checktime), "reread"
-- (read the file again with :edit) or nil (leave it). nvim answers two cases with a message instead of a
-- reload, and in the person's own nvim a UI is attached:
--   * the file is gone (E211): in Normal mode a one second pause, or a prompt for a key when the name is
--     too long for the command line;
--   * the file was missing when the buffer was opened and exists now (W13): a dialog that takes the next
--     key typed, whatever 'autoread' says, once per buffer.
-- A file that does not exist now is left alone. The part cannot see nvim's mark of a buffer opened on a
-- missing file, but such a buffer holds one empty line (nothing was read into it, or what was typed was
-- undone), so a buffer that holds one empty line is read again with :edit the first time its file is there
-- after the file was missing, and at first sight: that clears the mark, loads what the file holds, and
-- costs one read of a file that was empty anyway. The buffer counts as seen only once that read has run: a
-- read that failed (the command-line window is open) is tried again, never handed to checktime. Only a
-- regular file is read again (opening a FIFO blocks); every other buffer goes to checktime, which reloads
-- it when the file changed; one whose file existed when it was read is checked again after the file comes
-- back, as before. What can still raise a message: a file deleted between the stat and the check (E211),
-- and a buffer read again while its file is missing and then created within one tick of that (W13).
local function checkable(b)
  if vim.bo[b].buftype ~= "" then
    return nil
  end
  local name = vim.api.nvim_buf_get_name(b)
  if name == "" or name:find("^%a[%w+.-]*://") then
    return nil
  end
  local s = files[b]
  if not s or s.name ~= name then
    s = { name = name }
    files[b] = s
  end
  local blank = vim.api.nvim_buf_line_count(b) == 1 and vim.api.nvim_buf_get_lines(b, 0, 1, false)[1] == ""
  local stat = vim.uv.fs_stat(name)
  if not stat then
    s.seen = false
    return nil
  end
  if blank and not s.seen and stat.type == "file" then
    return "reread"
  end
  s.seen = true
  return "check"
end

-- Checks only buffers shown in a window, as a bare checktime does, and leaves alone a modified buffer
-- and one with autoread off: in the person's own nvim a UI is attached, and checktime on those
-- raises W12 or W11 as a dialog that takes the next keys typed. `checkable` keeps out the two cases
-- that are not reloads (E211, W13).
local function check_shown()
  for b in pairs(files) do
    if not vim.api.nvim_buf_is_valid(b) then
      files[b] = nil
    end
  end
  local seen = {}
  for _, w in ipairs(vim.api.nvim_list_wins()) do
    pcall(function()
      if not vim.api.nvim_win_is_valid(w) then
        return
      end
      local b = vim.api.nvim_win_get_buf(w)
      if seen[b] or not vim.api.nvim_buf_is_valid(b) then
        return
      end
      seen[b] = true
      if vim.bo[b].modified then
        return
      end
      local ar = vim.api.nvim_get_option_value("autoread", { buf = b })
      if ar == nil or ar == -1 then
        ar = vim.go.autoread
      end
      if ar ~= true and ar ~= 1 then
        return
      end
      local how = checkable(b)
      if how == "reread" then
        if reread(b) then
          files[b].seen = true
        end
      elseif how then
        vim.cmd("checktime " .. b)
      end
    end)
  end
end

local function now()
  if torn then
    return false
  end
  check_shown()
  return true
end

local function stop_timer()
  local old = timer
  timer = nil
  if old and not old:is_closing() then
    old:stop()
    old:close()
  end
end

local function start_timer()
  if torn then
    return
  end
  if timer then
    return
  end
  local current = vim.uv.new_timer()
  timer = current
  current:start(1000, 1000, vim.schedule_wrap(function()
    if torn then
      return
    end
    -- A tick queued before busy ended must not run after a stop or a restart.
    if timer ~= current then
      return
    end
    now()
  end))
end

local function busy(on)
  if torn then
    return false
  end
  if always then
    return true
  end
  if on then
    start_timer()
  else
    now()
    stop_timer()
  end
  return true
end

-- These hooks need no startup-only work: they also install after VimEnter.
vim.api.nvim_create_autocmd("FocusGained", { group = group, callback = function()
  if torn then
    return
  end
  now()
end })
if always then
  start_timer()
end

local function teardown()
  if torn then
    return
  end
  torn = true
  stop_timer()
  pcall(vim.api.nvim_del_augroup_by_id, group)
  -- This final check deliberately runs after teardown, including a lost channel.
  vim.schedule(check_shown)
end

return { teardown = teardown, calls = { busy = busy, now = now } }
