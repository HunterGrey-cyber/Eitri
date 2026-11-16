-- Eitri for someone who lives in tmux. Your prefix and your prefix-table keys come from your own tmux
-- configuration (keymap.from_tmux is on unless you turn it off), so this file adds only what tmux has and
-- Eitri does not do by default: splits that start in the pane's directory, new panes below, and a workspace
-- of shells. Copy it to ~/.config/eitri/init.lua, or paste the parts you want into yours.

local k = eitri.keymap

-- Splits like a common tmux setup: \ for a pane on the right, - for one below. Each waits for a module key,
-- as % and " do (<prefix> \ t opens a new terminal on the right); split from a terminal, the new shell starts
-- in that terminal's directory. Your tmux bindings with -c "#{pane_current_path}" do the same.
k.set("prefix", "\\", "split.right", { cwd = "pane" })
k.set("prefix", "-", "split.below", { cwd = "pane" })

-- A new terminal from <prefix> Alt+Enter opens below the module you are in, taking 35% of its space.
-- (A split key says where itself, so this rule does not move it.)
eitri.layout.rule("terminal", { at = "below", size = 0.35 })

-- The first window: the editor and the chat side by side, the terminal along the bottom.
eitri.layout.default({
  "column",
  { "row", { "editor", 0.6 }, { "agent" }, share = 0.7 },
  { "terminal" },
})

-- tmux sessions map to workspaces; tmux windows map to chat tabs. <prefix> ( and ) step through workspaces,
-- and <prefix> L goes back to the last one;
-- <prefix> s chooses one. "shells" puts two terminals one above the other in the middle; a workspace always
-- holds the editor and the chat, so they stay at the sides.
eitri.layout.workspace("shells", {
  "row",
  { "editor", 0.35 },
  { "column", { "terminal" }, { "terminal#2" }, share = 0.45 },
  { "agent" },
})

-- The panel that lists what may follow the prefix, after a pause, like tmux's own key list.
eitri.config.set("keymap.which_key_delay", 800)
