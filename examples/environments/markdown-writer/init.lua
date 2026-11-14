-- Eitri for writing Markdown: the preview opens beside the editor whenever a Markdown file is current,
-- scrolls with it both ways, and the chat sits below for an agent to read or check what you write.
-- Copy it to ~/.config/eitri/init.lua, or paste the parts you want into yours.

-- The preview, shown without taking the keys when a Markdown buffer becomes current.
-- <prefix> b shows it with the keys (j, k, / and f read it like a web page) and hides it again.
eitri.plugin.load("markdown", { auto = true, key = "b" })

-- It opens to the right of the module that has the keys (the editor, while you edit), a little under half its width.
eitri.layout.rule("lua:markdown", { at = "right", size = 0.45, focus = false })

-- The first window: the editor on top, the chat in a strip below it.
eitri.layout.default({ "column", { "editor", 0.72 }, { "agent" } })

-- After a turn that changed files, a short hint: c opens the review of what changed.
eitri.config.set("review.hint", "key")
