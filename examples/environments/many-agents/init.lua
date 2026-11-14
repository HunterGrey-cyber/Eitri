-- Eitri for someone running several agents at once: the Strips sidebar to see who needs you, The pass to
-- walk the ones that wait, the table to manage them all, and new chat views to the right of the module with the keys.
-- Copy it to ~/.config/eitri/init.lua, or paste the parts you want into yours.

-- Strips, a narrow rail at the window's left edge: <prefix> U shows it with the keys and hides it again.
eitri.plugin.load("agents-strips", { narrow = true })
eitri.layout.rule("lua:agents-strips", { at = "left-edge", size = "62px" })

-- The pass: one line above the editor, there only while an agent waits or a round runs.
-- <prefix> u walks the agents that wait, oldest first; <prefix> B goes back to where you started.
eitri.plugin.load("agents-pass")

-- The table, like top: <prefix> g shows it with the keys and hides it again.
eitri.plugin.load("agents-top", { key = "g" })

-- New chat views (<prefix> ! on a tab breaks it out into one) open to the right of the module with the keys.
eitri.layout.rule("agent", { at = "right" })

-- The first window: the editor, and the chat beside it at a bit under half the width.
eitri.layout.default({ "row", { "editor", 0.55 }, { "agent" } })

-- A permission card in a hidden chat brings the chat back where it was, without moving the keys.
eitri.config.set("modules.chat.on_permission", "reveal")
