-- Reproduces ~/.config/tmux/base.conf's bindings in Eitri. Paste into
-- $XDG_CONFIG_HOME/eitri/init.lua (default ~/.config/eitri/init.lua).
-- Since 0.2.1 Eitri reads your tmux config itself (keymap.from_tmux, on unless init.lua sets it
-- "off"), which gives these same keys; this snippet is only needed with the import off. Left in
-- place with the import on, it still applies cleanly on top of it and changes nothing.
-- `m`, `q`, `H`, `J`, `K` and `L` are Eitri defaults from this version on (mark, select by number,
-- the far-edge moves and the last workspace), so each is deleted before it is set below.
-- A `del` of a key that is not bound stops startup, so those lines need this version or later,
-- or the tmux import on with these keys bound in tmux.
-- The split cwd option needs this version or later.
local k = eitri.keymap
k.prefix("C-a")                                             -- base.conf:11
k.del("prefix", "C-b")                                      -- base.conf:12
k.set("prefix", "C-a", "send-prefix")                       -- base.conf:14
k.del("prefix", "%")                                        -- base.conf:49
k.del("prefix", '"')                                        -- base.conf:50
k.set("prefix", "\\", "split.right", { cwd = "pane" })      -- base.conf:47
k.set("prefix", "-", "split.below", { cwd = "pane" })     -- base.conf:48
k.set("prefix", "|", "layout.even-horizontal")              -- base.conf:53
k.set("prefix", "_", "layout.even-vertical")                -- base.conf:54
k.del("prefix", "l")                                        -- stock last-window; here l resizes
for key, dir in pairs({ h = "left", j = "down", k = "up", l = "right" }) do
  k.set("prefix", key, "resize." .. dir, { cells = 5, repeatable = true })  -- base.conf:57-60
end
k.del("prefix", "m")                                        -- Eitri's default: mark.toggle (tmux's m)
k.set("prefix", "m", "zoom", { repeatable = true })         -- base.conf:61
k.del("prefix", "H")                                        -- Eitri's defaults: move.far-left/-down/-up
k.del("prefix", "J")
k.del("prefix", "K")
k.del("prefix", "L")
for key, dir in pairs({ H = "left", J = "down", K = "up", L = "right" }) do
  k.set("prefix", key, "swap." .. dir)                      -- base.conf:64-67
end
k.del("prefix", "q")                                        -- Eitri's default: select.number (tmux's q)
k.set("prefix", "q", "tab.close")                           -- base.conf:70 (Eitri always asks y/n)
-- base.conf:69 `x` and :73 `C-l` are Eitri's defaults already.
