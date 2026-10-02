-- Navigation fallback, loaded with --cmd before the user's config. At load it installs only
-- autocommands. At scheduled VimEnter, User VeryLazy and User LazyLoad, it gives
-- <C-h>/<C-j>/<C-k>/<C-l> in Normal and Visual mode a way out of the editor where the global
-- mapping is empty, nvim's default, or a plain window move. Other mappings and all buffer-local
-- mappings are left alone. Insert mode gets <C-l> only where its global mapping is empty;
-- <C-h>/<C-j>/<C-k> keep their Insert meanings: backspace, newline and digraph.
--
-- Under `--cmd` (`dofile`, no arguments), use the socket from the spawn environment.
-- When injected into a running nvim, take the socket from the argument table instead: inherited
-- environment variables may name a different window and send keys to the wrong panel.
-- An injected chunk returns a function that undoes it.
local opts = ...
local socket
if type(opts) == "table" then
  socket = opts.socket
else
  socket = vim.env.EITRI_PANE_SWITCH_SOCKET
end
if not socket or socket == "" then
  return
end

-- The direction letters are vim-tmux-navigator's own `tr(a:direction, 'phjkl', 'lLDUR')`
-- (plugin/tmux_navigator.vim, s:TmuxAwareNavigate), what the tmux shim writes for `select-pane -L`.
local KEYS = {
  { lhs = "<C-h>", dir = "h", letter = "L", name = "left" },
  { lhs = "<C-j>", dir = "j", letter = "D", name = "down" },
  { lhs = "<C-k>", dir = "k", letter = "U", name = "up" },
  { lhs = "<C-l>", dir = "l", letter = "R", name = "right" },
}
local DESC = "eitri: window or pane "

-- Send synchronously so a busy callback or an editor exit cannot lose a queued letter.
-- `vim.fn.sockconnect`, `chansend` and `chanclose` finish before the mapping returns.
-- An asynchronous `pipe:connect`/`pipe:write` would defer the write to a later event-loop turn.
-- Each `pcall` absorbs connection or write errors, so a missing listener costs one keypress
-- and never raises an error in the editor.
local function send(letter)
  local ok, chan = pcall(vim.fn.sockconnect, "pipe", socket, { rpc = false })
  if not ok or chan == 0 then
    return
  end
  pcall(vim.fn.chansend, chan, letter .. "\n")
  pcall(vim.fn.chanclose, chan)
end

-- vim-tmux-navigator's s:VimNavigate: `wincmd`, an error (E11 in the command-line window) swallowed.
local function wincmd(dir)
  pcall(vim.cmd.wincmd, dir)
end

-- Normal: s:TmuxAwareNavigate -- remember winnr(), wincmd; an unchanged winnr() is the edge of
-- nvim's own layout, and the key goes on to the pane beyond it.
local function normal(key)
  local nr = vim.fn.winnr()
  wincmd(key.dir)
  if vim.fn.winnr() == nr then
    send(key.letter)
  end
end

-- Visual mode: at nvim's edge (`winnr('<dir>') == winnr()`), send the direction letter and
-- keep the selection as the panel's editor context. Otherwise leave Visual mode with <Esc>
-- and move to the neighboring window.
local function visual(key)
  if vim.fn.winnr(key.dir) == vim.fn.winnr() then
    send(key.letter)
    return
  end
  local nr = vim.fn.winnr()
  vim.cmd("normal! \27")
  wincmd(key.dir)
  if vim.fn.winnr() == nr then
    send(key.letter)
  end
end

-- Insert mode: leave Insert first, then navigate as in Normal mode. Move to a neighboring
-- window, or send the direction letter to the next pane at nvim's edge.
--
-- `:stopinsert` takes effect after this callback returns. Moving inside the callback would
-- end Insert in the destination window, step its cursor left and fire InsertLeave there,
-- while leaving the source cursor unchanged. Schedule navigation so Insert ends in the
-- source window before either moving focus or sending the letter.
local function insert(key)
  vim.cmd.stopinsert()
  vim.schedule(function()
    normal(key)
  end)
end

-- `<...>` key notation compared case-insensitively (`<c-w>` and `<C-W>` are one key), everything
-- else exactly: `<C-W>H` moves the window itself (:h CTRL-W_H), it is not `<C-W>h`.
local function notation_lower(rhs)
  return (rhs:gsub("<[^>]*>", string.lower))
end

-- The global mappings of `mode` by lowercased lhs, never a buffer-local one: maparg() returns the
-- current buffer's own mapping first (:h maparg()), and a buffer-local <C-l> (netrw's, for one)
-- must neither be read as the global slot nor touched. nvim_get_keymap() lists global mappings
-- only, lhs in key notation (a raw ^L is reported as <C-L> too).
local function global_maps(mode)
  local by_lhs = {}
  for _, m in ipairs(vim.api.nvim_get_keymap(mode)) do
    by_lhs[m.lhs:lower()] = m
  end
  return by_lhs
end

-- nvim's own default <C-L> (:h CTRL-L-default, :h default-mappings) counts as an empty slot: it is
-- a default, not the user's choice, and with it left alone Ctrl+l could never leave a plain nvim.
local NVIM_DEFAULT_CTRL_L = "<cmd>nohlsearch<bar>diffupdate<bar>normal! <c-l><cr>"

local function is_nvim_default(m, key)
  if key.dir ~= "l" then
    return false
  end
  if m.desc == ":help CTRL-L-default" then
    return true
  end
  return m.callback == nil and m.rhs ~= nil and notation_lower(m.rhs) == NVIM_DEFAULT_CTRL_L
end

-- A plain window move for the same direction -- stock LazyVim's `map("n", "<C-h>", "<C-w>h",
-- { remap = true })` is one. The fallback is a strict superset: it still moves between windows first.
local function is_plain_move(m, key)
  if m.callback ~= nil or m.expr == 1 or m.rhs == nil then
    return false
  end
  local rhs = notation_lower(m.rhs)
  return rhs == "<c-w>" .. key.dir
    or rhs == "<cmd>wincmd " .. key.dir .. "<cr>"
    or rhs == ":wincmd " .. key.dir .. "<cr>"
end

-- vim-tmux-navigator outside tmux is a plain window move: the plugin reads $TMUX once, when it
-- loads, and with it empty defines its commands as bare `wincmd`. Only an nvim the panel attached
-- to after startup can be in that state with Eitri around, and only outside tmux; inside tmux the
-- plugin is tmux's, and the slot is left alone.
local NAVIGATOR = { h = "Left", j = "Down", k = "Up", l = "Right" }
local navigator_is_plain = type(opts) == "table" and opts.companion == true and (vim.env.TMUX or "") == ""

local function is_plain_navigator(m, key)
  if not navigator_is_plain or m.callback ~= nil or m.expr == 1 or m.rhs == nil then
    return false
  end
  local cmd = "TmuxNavigate" .. NAVIGATOR[key.dir]
  local rhs = notation_lower(m.rhs)
  return rhs == ":<c-u>" .. cmd .. "<cr>" or rhs == "<cmd><c-u>" .. cmd .. "<cr>" or rhs == "<cmd>" .. cmd .. "<cr>"
end

local RUNNERS = { n = normal, x = visual, i = insert }

-- What each install displaced, by "<mode> <lhs>": the global mapping that held the slot (a nvim
-- default or a plain window move), or false for an empty slot. Teardown puts it back.
local installed = {}

local function install(mode, key, displaced)
  local runner = RUNNERS[mode]
  local slot = mode .. " " .. key.lhs
  installed[slot] = displaced or false
  vim.keymap.set(mode, key.lhs, function()
    runner(key)
  end, { desc = DESC .. key.name, silent = true })
end

local function check()
  for _, mode in ipairs({ "n", "x" }) do
    local maps = global_maps(mode)
    for _, key in ipairs(KEYS) do
      local m = maps[key.lhs:lower()]
      if
        m == nil
        or (
          m.desc ~= DESC .. key.name
          and (is_nvim_default(m, key) or is_plain_move(m, key) or is_plain_navigator(m, key))
        )
      then
        install(mode, key, m)
      end
    end
  end
  -- Insert: <C-l> only, and only into an empty global slot -- nothing of nvim's own to displace,
  -- and a user's Insert <C-l> (a completion key, a cursor move) is theirs. The fallback's own
  -- mapping is kept as it is.
  local ctrl_l = KEYS[4]
  if global_maps("i")[ctrl_l.lhs:lower()] == nil then
    install("i", ctrl_l)
  end
end

-- Scheduled, so the check runs after every other handler of the same event: LazyVim sets its
-- <C-h> -> <C-w>h in keymaps.lua on User VeryLazy, from an autocommand defined after this one.
local pending = false
-- Set first thing by the teardown. A check already queued (by the late install below, or by a
-- VeryLazy or LazyLoad that fired just before the teardown) would otherwise run afterwards and put
-- the mappings back.
local torn = false
local function schedule_check()
  if pending then
    return
  end
  pending = true
  vim.schedule(function()
    pending = false
    if torn then
      return
    end
    pcall(check)
  end)
end

local group = vim.api.nvim_create_augroup("eitri_nav", { clear = true })
vim.api.nvim_create_autocmd("VimEnter", { group = group, callback = schedule_check })
vim.api.nvim_create_autocmd("User", { group = group, pattern = { "VeryLazy", "LazyLoad" }, callback = schedule_check })

-- Injected after startup (companion mode): VimEnter, VeryLazy and LazyLoad have all fired already
-- and none will fire again, so the check that those events would have scheduled runs now.
if vim.v.vim_did_enter == 1 then
  schedule_check()
end

-- Undoes this chunk: its autocommands, and each mapping it put in a slot -- restoring what the slot
-- held, but only while the slot still holds Eitri's own mapping (a user who has remapped it since
-- keeps theirs).
return function()
  torn = true
  pcall(vim.api.nvim_del_augroup_by_id, group)
  for slot, displaced in pairs(installed) do
    local mode, lhs = slot:match("^(%S+) (.+)$")
    local current
    for _, m in ipairs(vim.api.nvim_get_keymap(mode)) do
      if m.lhs:lower() == lhs:lower() then
        current = m
      end
    end
    if current ~= nil and current.desc ~= nil and current.desc:sub(1, #DESC) == DESC then
      pcall(vim.keymap.del, mode, lhs)
      if displaced then
        local rhs = displaced.callback or displaced.rhs
        if rhs ~= nil then
          local restore = {
            remap = displaced.noremap == 0,
            silent = displaced.silent == 1,
            expr = displaced.expr == 1,
            nowait = displaced.nowait == 1,
            script = displaced.script == 1,
            desc = displaced.desc,
          }
          -- Only an expr mapping has keycodes to replace; vim.keymap.set refuses the option otherwise.
          if displaced.expr == 1 then
            restore.replace_keycodes = displaced.replace_keycodes == 1
          end
          pcall(vim.keymap.set, mode, displaced.lhs, rhs, restore)
        end
      end
    end
  end
  installed = {}
end
