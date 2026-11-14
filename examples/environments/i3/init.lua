-- Eitri for someone used to i3 or sway, with Alt as $mod. Eitri's container keys are i3's $mod chords
-- pressed after the prefix; this file moves them to Ctrl+Alt so your window manager keeps Alt, keeps
-- every module inside Eitri's window, and starts with i3's tabbed containers and a drop-down terminal.
-- Copy it to ~/.config/eitri/init.lua, or paste the parts you want into yours.

-- The Alt chords of the prefix table move to Ctrl+Alt: <prefix> Ctrl+Alt+w makes a container tabbed,
-- <prefix> Ctrl+Alt+Enter opens a terminal. Your own keymap calls after this line win over it.
eitri.keymap.use("i3-alt")

-- On sway, Hyprland and niri Eitri makes chats and pages windows of your window manager by default.
-- This keeps them tiled inside Eitri's window, as i3's containers. Remove the line to let sway tile them.
eitri.config.set("window.tiling", "eitri")

-- The first window: the editor on the left, the chat and the terminal as tabs on the right
-- (Ctrl+h and Ctrl+l step between the tabs, as between modules).
eitri.layout.default({
  "row",
  { "editor", 0.6 },
  { "tabbed", { "agent" }, { "terminal" }, active = 1 },
})

-- New terminals and chat views open as tabs of the container you are in.
eitri.layout.rule("terminal", { at = "tab" })
eitri.layout.rule("agent", { at = "tab" })

-- i3's scratchpad as a drop-down terminal: <prefix> F2 shows terminal 2 over everything, again puts it away.
eitri.keymap.set("prefix", "F2", "scratchpad.show", { module = "terminal#2" })
eitri.layout.rule("terminal#2", { float = true, size = { 0.8, 0.5 } })
