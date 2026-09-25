-- Reproduces ~/.config/tmux/base.conf's bindings in neovibe. Paste into ~/.config/neovibe/init.lua.
local k = neovibe.keymap
k.prefix("C-a")                                             -- base.conf:11
k.del("prefix", "C-b")                                      -- base.conf:12
k.set("prefix", "C-a", "send-prefix")                       -- base.conf:14
k.del("prefix", "%")                                        -- base.conf:49
k.del("prefix", '"')                                        -- base.conf:50
k.set("prefix", "\\", "split.right")                        -- base.conf:47
k.set("prefix", "-", "split.below")                         -- base.conf:48
k.set("prefix", "|", "layout.even-horizontal")              -- base.conf:53
k.set("prefix", "_", "layout.even-vertical")                -- base.conf:54
k.del("prefix", "l")                                        -- stock last-window; his l resizes
for key, dir in pairs({ h = "left", j = "down", k = "up", l = "right" }) do
  k.set("prefix", key, "resize." .. dir, { cells = 5, repeatable = true })  -- base.conf:57-60
end
k.set("prefix", "m", "zoom", { repeatable = true })         -- base.conf:61
for key, dir in pairs({ H = "left", J = "down", K = "up", L = "right" }) do
  k.set("prefix", key, "swap." .. dir)                      -- base.conf:64-67
end
k.set("prefix", "q", "tab.close")                           -- base.conf:70 (neovibe always asks y/n)
-- base.conf:69 `x` and :73 `C-l` are neovibe's defaults already.
