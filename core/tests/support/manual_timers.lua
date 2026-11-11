-- Real libuv handles retain lifecycle/footprint checks; only their deadlines are driven here.
if _G.test_timers then return end
local uv = vim.uv
local new_timer = uv.new_timer
local schedule = vim.schedule
local timers, now = {}, 0
local far = 1000000000
local methods = {}
function methods:start(delay, repeat_ms, callback)
  assert(not self.handle:is_closing())
  self.due, self.repeat_ms, self.callback = now + delay, repeat_ms, callback
  self.handle:start(far, repeat_ms, function() error('manual timer reached real time') end)
end
function methods:stop()
  self.due = nil
  return self.handle:stop()
end
function methods:close()
  self.due = nil
  return self.handle:close()
end
uv.new_timer = function()
  local timer = { handle = new_timer() }
  setmetatable(timer, { __index = function(self, key)
    if methods[key] then return methods[key] end
    local method = self.handle[key]
    if type(method) == 'function' then
      return function(_, ...) return method(self.handle, ...) end
    end
    return method
  end })
  timers[#timers + 1] = timer
  return timer
end
_G.test_timers = {}
function test_timers.now() return now end
function test_timers.flush()
  local done = false
  schedule(function() done = true end)
  assert(vim.wait(60000, function() return done end, 1), 'scheduled work did not finish')
end
function test_timers.advance(ms)
  local finish = now + ms
  for iteration = 1, 10000 do
    local next_timer
    for _, timer in ipairs(timers) do
      if timer.due and timer.due <= finish and (not next_timer or timer.due < next_timer.due) then
        next_timer = timer
      end
    end
    if not next_timer then now = finish; test_timers.flush(); return end
    now = next_timer.due
    if next_timer.repeat_ms == 0 then next_timer:stop()
    else next_timer.due = now + next_timer.repeat_ms end
    local done, failure = false, nil
    local fire = new_timer()
    fire:start(0, 0, function()
      fire:stop(); fire:close()
      local ok, err = pcall(next_timer.callback)
      if not ok then failure = err end
      -- Runs after anything the production callback scheduled.
      schedule(function() done = true end)
    end)
    assert(vim.wait(60000, function() return done end, 1), 'timer callback did not finish')
    assert(not failure, failure)
  end
  error('manual timers exceeded 10000 firings; a callback did not advance')
end

-- Process-channel closure is asynchronous even when its polling timer is manual.
function test_timers.until_ready(predicate)
  for iteration = 1, 10000 do
    if predicate() then return true end
    test_timers.advance(250)
    vim.wait(1)
  end
  error('condition did not arrive in 10000 advancing timer turns')
end
