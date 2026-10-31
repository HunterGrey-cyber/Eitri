-- Gives the window's embedded nvim `:Eitri`, `_G.__eitri_window.run` and `.edge`, `vim.g.eitri` and `User EitriReady`, and with
-- `group` the leader keys that run the same verbs. Called with one table,
-- `{ socket = <path>, words = { <first words it completes> }, names = { <first words the host reads> },
-- modules = { <module ids it completes> }, group = { lhs = <map lhs>, keys = { { suffix, words, desc } } } | nil }`.
-- Nothing here reads the environment: an nvim started inside another Eitri window inherits that window's
-- variables, and reading them would send the words to the wrong one. Runs on LuaJIT and PUC 5.1.
--
-- What is sent is one `W <word> <word>...` line on the pane-switch socket. A word is checked here against the
-- same grammar the host reads (1 to 64 bytes of A-Z a-z 0-9 . _ : = < > # -, at most 6 words), so a word the host
-- would drop is refused here with a message instead of vanishing, and a first word the host has no reading for is
-- refused the same way. What the host refuses among the rest it toasts in the window: nothing comes back on the socket.
local o = ...

local NAME = "Eitri"
-- nvim reports a Lua command's description as its `definition`, which is how a later call tells this command from
-- one the user defined under the same name.
local DESC = "eitri: run a window or layout action"
local MAX_WORDS = 6
local MAX_WORD_BYTES = 64
local GROUP_DESC_PREFIX = "Eitri: "
-- `module.<id>` names a Lua panel too, and only the host has their ids, so these first words are left to it.
local MODULE_ACTION = "module."

if
  type(o) ~= "table"
  or type(o.socket) ~= "string"
  or type(o.words) ~= "table"
  or type(o.names) ~= "table"
  or type(o.modules) ~= "table"
then
  error("eitri: :Eitri needs a socket and its words")
end

-- Before VimEnter the user's config may not have defined its own commands yet, and one defined here first would
-- make theirs fail or silently replace this one. Nothing is changed; the host asks again.
if vim.v.vim_did_enter == 0 then
  return { ok = false, early = true }
end

local socket = o.socket
local first_words = o.words
local module_words = o.modules
local known = {}
for _, name in ipairs(o.names) do
  known[name] = true
end

local function notify(message, level)
  vim.notify(message, level)
end

-- The first word that cannot be sent, as text; nil when `words` (n of them) can all travel.
local function unsendable(words, n)
  if n == 0 then
    return ""
  end
  if n > MAX_WORDS then
    return tostring(words[MAX_WORDS + 1])
  end
  for i = 1, n do
    local word = words[i]
    if type(word) ~= "string" or #word < 1 or #word > MAX_WORD_BYTES or word:find("[^A-Za-z0-9._:=<>#%-]") then
      return tostring(word)
    end
  end
  return nil
end

-- Synchronous, like `nav_fallback.lua`'s `send`: connect, write and close are done before this returns, so the line
-- is on the wire by the time the caller is. Every failure of the socket (nobody listening, a window that is gone)
-- costs the keypress and never an error in the user's editor.
local function write_line(line)
  local ok, chan = pcall(vim.fn.sockconnect, "pipe", socket, { rpc = false })
  if not ok or chan == 0 then
    return false
  end
  local wrote = pcall(vim.fn.chansend, chan, line)
  pcall(vim.fn.chanclose, chan)
  return wrote
end

local function send(words, n)
  local bad = unsendable(words, n)
  if bad ~= nil then
    notify(
      ':Eitri: "' .. bad .. '" cannot be sent (letters, digits and . _ : = < > # - only, at most 64 each, at most 6 words)',
      vim.log.levels.ERROR
    )
    return false
  end
  local first = words[1]
  if not known[first] and first:sub(1, #MODULE_ACTION) ~= MODULE_ACTION then
    notify(':Eitri: "' .. first .. '" is neither a verb nor an action (Tab lists them)', vim.log.levels.ERROR)
    return false
  end
  return write_line("W " .. table.concat(words, " ", 1, n) .. "\n")
end

local function run_words(...)
  return send({ ... }, select("#", ...))
end

-- For a navigator plugin's own edge hook (smart-splits' `at_edge`): the letter the nav fallback writes when a move
-- leaves nvim's last window. Only the four direction words are taken; anything else sends nothing.
local EDGE_LETTERS = { left = "L", down = "D", up = "U", right = "R" }
local function edge(direction)
  local letter = type(direction) == "string" and EDGE_LETTERS[direction] or nil
  if letter == nil then
    return false
  end
  return write_line(letter .. "\n")
end

-- What the next word can be after the finished words `done` of a command.
local SIDES = { "right", "below" }
local KINDS = { "terminal", "agent" }
local TAKES_A_MODULE = { focus = true, show = true, hide = true }
local function candidates(done)
  local verb = done[1]
  if verb == nil then
    return first_words
  end
  if verb == "split" then
    if #done == 1 then
      return SIDES
    end
    if #done == 2 then
      return module_words
    end
  elseif TAKES_A_MODULE[verb] and #done == 1 then
    return module_words
  elseif verb == "open" and #done == 1 then
    return KINDS
  end
  return {}
end

-- The command, only where neither a global nor a buffer-local `:Eitri` is the user's.
local skipped = {}
do
  local global = vim.api.nvim_get_commands({ builtin = false })
  local in_buffer = vim.api.nvim_buf_get_commands(0, { builtin = false })
  local mine = global[NAME] ~= nil and global[NAME].definition == DESC
  local taken = (global[NAME] ~= nil and not mine) or in_buffer[NAME] ~= nil
  local made = false
  if not taken then
    made = pcall(vim.api.nvim_create_user_command, NAME, function(args)
      send(args.fargs, #args.fargs)
    end, {
      nargs = "+",
      desc = DESC,
      complete = function(lead, line, pos)
        -- Everything after the command's name up to the cursor; a modifier like `:vertical` may come before the name.
        local typed = line:sub(1, pos):gsub("^.-Eit%a*%s*", "")
        local done = {}
        for word in typed:gmatch("%S+") do
          done[#done + 1] = word
        end
        if lead ~= "" then
          done[#done] = nil
        end
        local out = {}
        for _, word in ipairs(candidates(done)) do
          if word:sub(1, #lead) == lead then
            out[#out + 1] = word
          end
        end
        return out
      end,
    })
  end
  if not made then
    skipped[#skipped + 1] = NAME
    notify("[nvim] :" .. NAME .. " is already defined; not replacing it", vim.log.levels.WARN)
  end
end

-- The leader keys. A `<leader>` or `<localleader>` in the group's lhs is expanded when it is compared, not when it
-- is written: the user's config may set `mapleader` after this runs, and `<Space>` and " " are the same key.
local function expand(lhs)
  local leader = vim.g.mapleader
  if leader == nil or leader == "" then
    leader = "\\"
  end
  local local_leader = vim.g.maplocalleader
  if local_leader == nil or local_leader == "" then
    local_leader = "\\"
  end
  lhs = (lhs:gsub("<[Ll][Ee][Aa][Dd][Ee][Rr]>", function()
    return leader
  end))
  lhs = (lhs:gsub("<[Ll][Oo][Cc][Aa][Ll][Ll][Ee][Aa][Dd][Ee][Rr]>", function()
    return local_leader
  end))
  return lhs
end

-- `lhs` as the bytes nvim stores, the one form two spellings of a key (`<Space>`, " ") compare equal in.
local function raw(lhs)
  return vim.api.nvim_replace_termcodes(expand(lhs), true, true, true)
end

local function install_group(group)
  local mapped = {} -- raw lhs -> the lhs as mapped, for each map this snippet made
  local installed = false
  local refused = false
  local which_key_told = false

  local function ours(map)
    return type(map) == "table" and type(map.desc) == "string" and map.desc:sub(1, #GROUP_DESC_PREFIX) == GROUP_DESC_PREFIX
  end

  -- The first map of someone else at or under the group's lhs, global or buffer-local. A key the group installed
  -- is left out whoever holds it now: a plugin that maps one later takes that one key, and the rest stand.
  local function collision(group_raw)
    for _, scope in ipairs({ "global", "buffer" }) do
      local maps
      if scope == "global" then
        maps = vim.api.nvim_get_keymap("n")
      else
        maps = vim.api.nvim_buf_get_keymap(0, "n")
      end
      for _, map in ipairs(maps) do
        local lhs = map.lhsraw or raw(map.lhs)
        if lhs:sub(1, #group_raw) == group_raw and mapped[lhs] == nil then
          return map
        end
      end
    end
    return nil
  end

  -- Reads the global table itself: `maparg` answers with a buffer-local map that hides one of ours, and that global
  -- map would stay behind.
  local function remove_ours()
    for _, map in ipairs(vim.api.nvim_get_keymap("n")) do
      local lhs = mapped[map.lhsraw or raw(map.lhs)]
      if lhs ~= nil and ours(map) then
        pcall(vim.api.nvim_del_keymap, "n", lhs)
      end
    end
  end

  local function check()
    if refused then
      return
    end
    local group_raw = raw(group.lhs)
    local found = collision(group_raw)
    if found ~= nil then
      refused = true
      if installed then
        remove_ours()
      end
      local what = found.desc
      if what == nil or what == "" then
        what = found.rhs
      end
      if what == nil or what == "" then
        what = "a Lua function"
      end
      notify(
        'keymap.nvim_prefix "' .. group.lhs .. '": ' .. vim.fn.keytrans(found.lhsraw or raw(found.lhs))
          .. " is already mapped (" .. what:sub(1, 60) .. "); choose another",
        vim.log.levels.WARN
      )
      return
    end
    if not installed then
      installed = true
      for _, key in ipairs(group.keys) do
        local lhs = expand(group.lhs .. key.suffix)
        local taken = vim.fn.maparg(lhs, "n", false, true)
        if type(taken) ~= "table" or next(taken) == nil then
          local words = key.words
          local made = pcall(vim.keymap.set, "n", lhs, function()
            send(words, #words)
          end, { desc = GROUP_DESC_PREFIX .. key.desc, silent = true, noremap = true })
          if made then
            mapped[raw(lhs)] = lhs
          end
        end
      end
    end
    if not which_key_told and package.loaded["which-key"] then
      which_key_told = true
      pcall(function()
        require("which-key").add({ { group.lhs, group = "Eitri window" } })
      end)
    end
  end

  check()
  -- LazyVim maps its own `<leader>` keys at `User VeryLazy`, and plugins load their keys at `User LazyLoad`: either
  -- can come after the install, so the group is checked again there, scheduled so that every handler of the event
  -- (this one is registered first) has run when the maps are read.
  local augroup = vim.api.nvim_create_augroup("EitriVerbs", { clear = true })
  for _, pattern in ipairs({ "VeryLazy", "LazyLoad" }) do
    vim.api.nvim_create_autocmd("User", { group = augroup, pattern = pattern, callback = vim.schedule_wrap(check) })
  end
end

if type(o.group) == "table" and type(o.group.lhs) == "string" and type(o.group.keys) == "table" then
  install_group(o.group)
end

_G.__eitri_window = { run = run_words, edge = edge }
local first = vim.g.eitri ~= 1
vim.g.eitri = 1
if first then
  pcall(vim.api.nvim_exec_autocmds, "User", { pattern = "EitriReady" })
end
return { ok = true, skipped = skipped }
