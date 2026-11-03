-- Reloads for an agent slot. The returned calls and teardown keep the timer alive
-- without sharing a global with the embedded editor's unconditional reload timer.
local opts = ...
opts = opts or {}
local always = opts.always == true
local torn = false
local timer
local group = vim.api.nvim_create_augroup("EitriBufferReload", { clear = true })

local function now()
  if torn then
    return false
  end
  pcall(vim.cmd, "checktime")
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
  vim.schedule(function()
    pcall(vim.cmd, "checktime")
  end)
end

return { teardown = teardown, calls = { busy = busy, now = now } }
