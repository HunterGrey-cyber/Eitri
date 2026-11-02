-- Eitri editor watch. Loaded with --cmd before the user's config, like the nav fallback, and only
-- for the nvim a window embeds. It tells the window two things over the pane-switch socket and
-- changes nothing in the editor: that this nvim will say when it leaves, and which of the project's
-- files have unsaved changes left in swap files. Nothing is recovered, opened or deleted here.
--
-- Everything waits for VimEnter. A config that runs `autocmd!` removes every autocommand made before
-- it, so the VimLeavePre made earlier could be gone with nothing saying so: made at VimEnter, the
-- line that says it exists is sent only once it does. 'directory' is the user's by then too.
local socket = vim.env.EITRI_PANE_SWITCH_SOCKET
if not socket or socket == "" then
  return
end

-- nvim starts in the project; a config may `cd` elsewhere afterwards.
local root = vim.fn.getcwd()

-- Swap files read at one start. What is left over is not counted, and is still found by nvim itself
-- when the file is opened.
local MAX_SWAP_FILES = 256

-- Synchronous, like `core/src/layout/kill.rs`'s `editor_quit_lua`: at VimLeavePre a later turn of the
-- event loop never comes, so the line has to be on the wire when this returns. Every `pcall` keeps a
-- missing listener out of the user's editor.
local function send(line)
  local ok, chan = pcall(vim.fn.sockconnect, "pipe", socket, { rpc = false })
  if not ok or chan == 0 then
    return
  end
  pcall(vim.fn.chansend, chan, line .. "\n")
  pcall(vim.fn.chanclose, chan)
end

-- An entry of 'directory' as an absolute path with no trailing slash. A relative entry means the
-- edited file's directory, of which only the project root is looked in (`nvim -r` looks there too).
local function absolute(entry)
  local dir = vim.fn.expand(entry)
  if dir:sub(1, 1) ~= "/" then
    dir = root .. "/" .. dir
  end
  return (dir:gsub("/+$", ""))
end

-- The swap files in `dir` that may be this project's, at most `budget`. A name holding `%` is a full
-- path spelled out (a 'directory' entry ending in `//`), so another project's is skipped unread.
local function swap_files(dir, budget)
  local found = {}
  local handle = vim.uv.fs_scandir(dir)
  if not handle then
    return found
  end
  local spelled = root:gsub("/", "%%") .. "%"
  while #found < budget do
    local name = vim.uv.fs_scandir_next(handle)
    if not name then
      break
    end
    local swap = name:match("%.sw[a-z]$") ~= nil
    if swap and (not name:find("%", 1, true) or name:sub(1, #spelled) == spelled) then
      found[#found + 1] = dir .. "/" .. name
    end
  end
  return found
end

-- The project file a swap file holds unsaved changes of, with when they were written; nil otherwise.
-- `swapinfo` gives a pid only for an nvim still running: that one is editing the file now, and there
-- is nothing to recover.
local function unsaved(path)
  local info = vim.fn.swapinfo(path)
  if info.error or info.dirty == 0 or info.pid ~= 0 or info.fname == nil or info.fname == "" then
    return nil
  end
  local file = vim.fn.fnamemodify(info.fname, ":p")
  if file:sub(1, #root + 1) ~= root .. "/" then
    return nil
  end
  return file:sub(#root + 2), info.mtime
end

-- `S <count> <newest>`, the file's name on one line and short enough for a toast.
local function report()
  local count, newest, newest_at = 0, nil, nil
  local budget = MAX_SWAP_FILES
  for _, entry in ipairs(vim.opt.directory:get()) do
    for _, path in ipairs(swap_files(absolute(entry), budget)) do
      budget = budget - 1
      local name, at = unsaved(path)
      if name then
        count = count + 1
        if newest_at == nil or at > newest_at then
          newest, newest_at = name, at
        end
      end
    end
  end
  if newest then
    send(("S %d %s"):format(count, vim.fn.strcharpart(newest:gsub("%c", "?"), 0, 120)))
  end
end

local group = vim.api.nvim_create_augroup("eitri_editor_watch", { clear = true })
vim.api.nvim_create_autocmd("VimEnter", {
  group = group,
  once = true,
  callback = function()
    vim.api.nvim_create_autocmd("VimLeavePre", {
      group = group,
      callback = function()
        -- `v:dying` is set on the way out of a deadly signal, which is no quit anyone asked for.
        if vim.v.dying == 0 then
          send("nvim-leaving")
        end
      end,
    })
    send("nvim-watched")
    vim.schedule(function()
      pcall(report)
    end)
  end,
})
