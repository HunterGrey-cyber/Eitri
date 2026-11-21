-- Runs a command on the server for a buffer inside a recorded remote project: the argv Neovim would run on the
-- laptop is wrapped in `eitri host exec`, and left alone anywhere else so one config serves local projects too.
local M = {}

local function buffer_dir()
  local name = vim.api.nvim_buf_get_name(0)
  if name == "" then
    return vim.fn.getcwd()
  end
  return vim.fn.fnamemodify(name, ":p:h")
end

local function absolute(path)
  local full = vim.fn.fnamemodify(path, ":p")
  return full == "/" and full or (full:gsub("/+$", ""))
end

-- host: { recorded = function(path) -> root|nil, program = function() -> string }, from the remote module.
function M.cmd(host, argv, opts)
  opts = opts or {}
  local cwd = absolute(opts.cwd or buffer_dir())
  if not host.recorded(cwd) then
    return argv
  end
  local wrapped = { host.program(), "host", "exec", "--cwd", cwd }
  -- A command that reads stdin (a formatter or linter handed the buffer) and answers on stdout needs no saved file
  -- on either side, so it skips both flushes unless asked for them with `flush = true`.
  if opts.flush == false or (opts.stdin and opts.flush ~= true) then
    table.insert(wrapped, "--no-flush")
  end
  if not opts.stdin then
    table.insert(wrapped, "--no-stdin")
  end
  table.insert(wrapped, "--")
  vim.list_extend(wrapped, argv)
  return wrapped
end

-- `:EitriRemote makeprg [<cmd>]`: this buffer's makeprg runs the command on the server (words split on blanks;
-- `make` when none). `errorformat` is untouched: paths in the output are the server's, which equal the mirror's.
function M.makeprg(host, args)
  local words = #args > 0 and args or { "make" }
  local argv = M.cmd(host, words, { stdin = false })
  if argv == words then
    vim.notify("This buffer is outside a recorded remote project; makeprg is unchanged")
    return
  end
  local quoted = vim.tbl_map(function(word)
    return word:match("^[%w_%-%./=:,+@]+$") and word or vim.fn.shellescape(word)
  end, argv)
  vim.bo.makeprg = table.concat(quoted, " ")
end

-- `:EitriRemote term`: a shell on the server, in a terminal buffer of a new tab, started in this buffer's directory
-- (the same path there). `eitri host term` is the relay; it ends with the shell, and wiping the buffer hangs it up,
-- which asks the server to close the shell's terminal. `termopen` before nvim 0.11, which eitri.nvim still serves.
function M.term(host)
  local cwd = absolute(buffer_dir())
  if not host.recorded(cwd) then
    vim.notify("This buffer is outside a recorded remote project; no server shell was opened")
    return
  end
  local argv = { host.program(), "host", "term", "--cwd", cwd }
  vim.cmd("tabnew")
  if vim.fn.has("nvim-0.11") == 1 then
    vim.fn.jobstart(argv, { term = true })
  else
    vim.fn.termopen(argv)
  end
  vim.cmd("startinsert")
end

return M
