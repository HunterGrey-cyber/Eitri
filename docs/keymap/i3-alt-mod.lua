-- In an init.lua, `eitri.keymap.use("i3-alt")` records exactly the calls below; this file is what that name is built from.
-- For i3 or sway with Alt as `$mod`. The stock prefix table has Alt chords after the prefix
-- (`Alt+w`, `Alt+Enter`, `Alt+1` ...); a stock i3 (/etc/i3/config) or sway (/etc/sway/config)
-- grabs those chords for itself before Eitri sees them, so those rows never run. This moves
-- exactly the rows that the stock config grabs; the rest of the table is untouched. Paste it into
-- $XDG_CONFIG_HOME/eitri/init.lua (default ~/.config/eitri/init.lua); if your own i3 or sway
-- config frees one of these chords, leave its pair out.
--
-- The rule: a moved row keeps its key with Ctrl added (Ctrl+Alt+<key> is bound by neither stock
-- config nor stock tmux). Two things Eitri cannot read as Ctrl chords, Ctrl+Shift+<character>, change
-- the rule: the four moves (Alt+Shift+h/j/k/l) become Ctrl+Alt+h/j/k/l, and the scratchpad move
-- (Alt+Shift+minus) becomes `_` itself.
-- A `del` of a key that is not bound stops startup, so each `del` below names a key the stock table
-- binds; with the tmux import on, a tmux config that unbinds one of them needs that pair left out.
local k = eitri.keymap
local move = { left = "h", down = "j", up = "k", right = "l" }
for dir, key in pairs(move) do
  k.del("prefix", "M-" .. key:upper())                         -- $mod+Shift+<dir>: move the focused window
  k.set("prefix", "C-M-" .. key, "move." .. dir, { repeatable = true })  -- Shift traded for Ctrl: Eitri reads no Ctrl+Shift+letter
end
k.del("prefix", "M-w")                                         -- $mod+w: tabbed layout
k.set("prefix", "C-M-w", "container.tabbed")
k.del("prefix", "M-s")                                         -- $mod+s: stacking layout
k.set("prefix", "C-M-s", "container.stacked")
k.del("prefix", "M-a")                                         -- $mod+a: focus parent
k.set("prefix", "C-M-a", "focus.parent")
k.del("prefix", "M-d")                                         -- $mod+d: the launcher
k.set("prefix", "C-M-d", "focus.child")
k.del("prefix", "M-Enter")                                     -- $mod+Return: a terminal
k.set("prefix", "C-M-Enter", "instance.new", { kind = "terminal" })
k.del("prefix", "M-S-Space")                                   -- $mod+Shift+space: float toggle
k.set("prefix", "C-M-Space", "float.toggle")                   -- Shift dropped: Ctrl+Shift+Space is an input-method switch on many setups
k.del("prefix", "M--")                                         -- $mod+minus: show the scratchpad
k.set("prefix", "C-M--", "scratchpad.show")
k.del("prefix", "M-_")                                         -- $mod+Shift+minus: move to the scratchpad
k.set("prefix", "_", "scratchpad.move")                        -- Shift+minus is `_`; free in stock tmux and in the stock table
for dir, key in pairs({ left = "Left", down = "Down", up = "Up", right = "Right" }) do
  k.del("prefix", "M-" .. key)                                 -- $mod+<arrow>: focus
  k.set("prefix", "C-M-" .. key, "resize." .. dir, { cells = 5, repeatable = true })
end
k.del("prefix", "M-1")                                         -- $mod+1 ... $mod+5: workspaces 1 to 5
k.set("prefix", "C-M-1", "layout.even-horizontal")
k.del("prefix", "M-2")
k.set("prefix", "C-M-2", "layout.even-vertical")
k.del("prefix", "M-3")
k.set("prefix", "C-M-3", "layout.main-horizontal")
k.del("prefix", "M-4")
k.set("prefix", "C-M-4", "layout.main-vertical")
k.del("prefix", "M-5")
k.set("prefix", "C-M-5", "layout.tiled")
k.del("prefix", "M-e")                                         -- $mod+e: toggle split
k.set("prefix", "C-M-e", "container.toggle-split")
-- Alt+o (rotate down) is not grabbed by either stock config and stays where it is.
