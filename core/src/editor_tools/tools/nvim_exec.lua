-- Runs the one text a person approved on its card, past the deadline and sequence check. This is the only chunk that
-- evaluates an argument. `args.code` runs with the user's rights and nothing here limits it; what stays bounded is the
-- answer, which always comes back as one table, whatever the code did or threw.
local RESULT_LIMIT = 64 * 1024

-- An error may be any Lua value, even one whose __tostring throws. The code has run by the time this is asked, so saying
-- what went wrong must not itself fail.
local function said(value)
  local ok, text = pcall(tostring, value)
  if ok and type(text) == 'string' then
    return text
  end
  return 'an error that cannot be shown'
end

-- The one answer, under `field`. str_utf_start at the first byte left out says how far back its character starts, 0
-- when it starts right there, so a cut never ends inside a character.
local function answer(field, text)
  if #text <= RESULT_LIMIT then
    return { [field] = text }
  end
  local keep = RESULT_LIMIT + vim.str_utf_start(text, RESULT_LIMIT + 1)
  return { [field] = text:sub(1, keep), cut = true }
end

-- Each value on its own, so one that cannot be shown leaves the others and the fact that the code ran.
local function inspected(count, values)
  local lines = {}
  for at = 1, count do
    local ok, text = pcall(vim.inspect, values[at])
    lines[at] = ok and text or ('<could not be shown: ' .. said(text) .. '>')
  end
  return table.concat(lines, '\n')
end

-- `...` keeps the number of values: a trailing nil is one, and no value at all is an empty answer.
local function ended(ok, ...)
  if not ok then
    return answer('failed', said((...)))
  end
  return answer('output', inspected(select('#', ...), { ... }))
end

local function run_approved()
  if args.kind == 'ex' then
    local ok, result = pcall(api.nvim_exec2, args.code, { output = true })
    if not ok then
      return answer('failed', said(result))
    end
    return answer('output', result.output)
  end

  if args.kind == 'lua' then
    -- loadstring: an nvim built with PUC Lua 5.1 has no `load` of a string.
    local run, problem = loadstring(args.code, '=eitri-agent')
    if not run then
      return answer('failed', said(problem))
    end
    return ended(pcall(run))
  end

  return { failed = 'unknown kind' }
end

if not args.observe_saves then return run_approved() end
local paths = {}
local watch = api.nvim_create_autocmd({ 'BufWritePost', 'FileWritePost', 'FileAppendPost' }, {
  callback = function(event)
    local path = vim.uv.fs_realpath(event.match) or vim.fn.fnamemodify(event.match, ':p')
    paths[path] = true
  end,
})
local ok, result = pcall(run_approved)
pcall(api.nvim_del_autocmd, watch)
if not ok then result = answer('failed', said(result)) end
result.saves = vim.tbl_keys(paths)
table.sort(result.saves)
return result
