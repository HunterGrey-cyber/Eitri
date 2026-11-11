if _G.test_wall_ms then return end
local sec, usec = vim.uv.gettimeofday()
_G.test_wall_ms = sec * 1000 + math.floor(usec / 1000)
vim.uv.gettimeofday = function()
  return math.floor(test_wall_ms / 1000), (test_wall_ms % 1000) * 1000
end
