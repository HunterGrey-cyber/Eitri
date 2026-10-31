local __stamp, args = ...
local api = vim.api
local __state = rawget(_G, '__eitri_agent')
if not __state then __state = { seq = {}, scopes = 0 }; _G.__eitri_agent = __state end
do
  local sec, usec = vim.uv.gettimeofday()
  local now = sec * 1000 + usec / 1000
  if now > __stamp.deadline_ms then return { expired = true } end
  local last = __state.seq[__stamp.scope]
  if last and last.seq >= __stamp.seq then return { expired = true } end
  if not last then
    __state.scopes = __state.scopes + 1
    if __state.scopes > 64 then
      local oldest, at = nil, math.huge
      for scope, entry in pairs(__state.seq) do
        if entry.at < at then oldest, at = scope, entry.at end
      end
      __state.seq[oldest] = nil
      __state.scopes = __state.scopes - 1
    end
  end
  __state.seq[__stamp.scope] = { seq = __stamp.seq, at = now }
end
