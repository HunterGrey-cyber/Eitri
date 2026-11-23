English | [简体中文](environments.zh-CN.md)

# Working environments

Eitri does little on its own until you tell it how you work. These are whole `init.lua` files, each written for one
way of working, to copy and change. Each is a directory under `examples/environments/` in Eitri's source tree, with
the `init.lua` and a short README saying who it is for and what it sets up.

| recipe | for you if | what it sets up |
|---|---|---|
| [`tmux`](../../examples/environments/tmux/README.md) | you live in tmux | splits that start in the pane's directory, new terminals below, a workspace of shells, the key list after a pause; your tmux keys come across by themselves ([Coming from tmux](migrate-from-tmux.md) says which) |
| [`i3`](../../examples/environments/i3/README.md) | you use i3 or sway with Alt as `$mod` | the container keys on `Ctrl+Alt`, everything tiled inside Eitri, tabbed containers, a drop-down terminal |
| [`markdown-writer`](../../examples/environments/markdown-writer/README.md) | you write in Markdown | the preview beside the editor whenever a Markdown file is current, the chat below |
| [`many-agents`](../../examples/environments/many-agents/README.md) | several agents work for you at once | the Strips rail, The pass and the table; new chat views to the right of the module with the keys; a card brings its chat back |
| [`beside-nvim`](../../examples/environments/beside-nvim/README.md) | you keep Neovim in a terminal | the agent panel and the Markdown preview as windows beside your own Neovim |

Every call in them is one that [Configuration](configuration.md) documents, and each file starts cleanly as an
`init.lua`.

## Making one your own

- **Start from one and change it.** A recipe is an ordinary `init.lua`: copy it to `~/.config/eitri/init.lua`, or
  paste the parts you want into the one you have. A call Eitri refuses does nothing and is listed when the window
  opens, with the file and line.
- **Arrange the window, then write it down.** Lay the window out by hand, then call `eitri.layout.export()` from a
  command of your own. It returns, as text, the `eitri.layout.default` and `eitri.layout.workspace` calls that make
  that arrangement at a first launch; [Layout from Lua](configuration.md#layout-from-lua) shows a command, bound to a
  key, that writes them to a file to paste from.
- **A saved layout wins.** Eitri remembers each project's layout, so a changed `eitri.layout.default` shows only in a
  project with no saved layout yet. To see it in a project you have opened before, delete that project's layout file
  ([where the layout is kept](configuration.md#layout-from-lua)).
- **Add a page of your own.** When a recipe needs something Eitri does not show, write it as a plugin:
  [Write a plugin](write-a-plugin.md) is the reference, and the agents pages of the `many-agents` recipe are built the
  same way.
