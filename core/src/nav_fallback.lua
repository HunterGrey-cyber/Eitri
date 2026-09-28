-- neovibe nav fallback (spec 2026-09-27-v1-ui-design.md §5, P1 and P14). Loaded with --cmd before
-- the user's config, like the theme and nvim-keys feeds. It sets no option and no mapping at load:
-- it only installs autocommands, and at VimEnter (scheduled), User VeryLazy and User LazyLoad it
-- gives each of <C-h>/<C-j>/<C-k>/<C-l>, in Normal and Visual mode, a way out of the editor --
-- but only where the global slot is empty, nvim's own default, or a plain window move. Anything
-- else (vim-tmux-navigator, a lazy.nvim key stub, smart-splits, a user's own mapping) is left
-- alone, and buffer-local mappings are never touched.
local socket = vim.env.NEOVIBE_PANE_SWITCH_SOCKET
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
local DESC = "neovibe: window or pane "

-- Synchronous, like `core/src/layout/kill.rs`'s `editor_quit_lua` and
-- `core/src/theme/nvim_theme.lua`'s own `send()`. Before this (P5-A1, the pane half --
-- `the private review notes`, "P5-A1"), the asynchronous
-- `pipe:connect`/`pipe:write` pair queued the actual write for a *later* turn of nvim's event
-- loop, so nvim staying busy in the same callback that pressed the key -- or exiting before that
-- later turn ever comes -- could lose the letter outright. `vim.fn.sockconnect`/`chansend`/
-- `chanclose` complete before this function returns, so the write is already on the wire by the
-- time the caller (the mapping) does. Every `pcall` swallows exactly what the async version's
-- `err`/missing-listener branches did: a missing listener, same as before, costs one keypress and
-- never an error in the user's editor.
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

-- Visual (P14; neovibe-only -- the plugin maps Normal mode only). The mapping runs like <Cmd>, so
-- Visual mode is not ended: at nvim's edge (`winnr('<dir>') == winnr()`, :h winnr()) the letter is
-- sent and the selection stays, as the editor context the panel shows. Otherwise Visual is left
-- (<Esc>) and the window changes, as the plugin's `:<C-U>` form does in Normal mode.
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

local function install(mode, key)
  local run = mode == "n" and function()
    normal(key)
  end or function()
    visual(key)
  end
  vim.keymap.set(mode, key.lhs, run, { desc = DESC .. key.name, silent = true })
end

local function check()
  for _, mode in ipairs({ "n", "x" }) do
    local maps = global_maps(mode)
    for _, key in ipairs(KEYS) do
      local m = maps[key.lhs:lower()]
      if m == nil or (m.desc ~= DESC .. key.name and (is_nvim_default(m, key) or is_plain_move(m, key))) then
        install(mode, key)
      end
    end
  end
end

-- Scheduled, so the check runs after every other handler of the same event: LazyVim sets its
-- <C-h> -> <C-w>h in keymaps.lua on User VeryLazy, from an autocommand defined after this one.
local pending = false
local function schedule_check()
  if pending then
    return
  end
  pending = true
  vim.schedule(function()
    pending = false
    pcall(check)
  end)
end

local group = vim.api.nvim_create_augroup("neovibe_nav", { clear = true })
vim.api.nvim_create_autocmd("VimEnter", { group = group, callback = schedule_check })
vim.api.nvim_create_autocmd("User", { group = group, pattern = { "VeryLazy", "LazyLoad" }, callback = schedule_check })
