-- Eitri beside the Neovim you already run in a terminal: the agent panel as a window of its own
-- (eitri panel, or :EitriPanel from Neovim), and the Markdown preview as a window that follows your Neovim.
-- Nothing here changes your Neovim configuration; the README shows the eitri.nvim lines to add there.
-- Copy it to ~/.config/eitri/init.lua, or paste the parts you want into yours.

-- The preview beside your Neovim: :EitriPage markdown opens it; <prefix> b in the panel does the same.
-- With auto it follows each Markdown buffer you enter without coming forward.
eitri.plugin.load("markdown", { auto = true, key = "b" })

-- The panel asks your window manager to move focus between it and your editor (Ctrl+h/j/k/l at the edge).
eitri.config.set("companion.wm", "auto")
