English | [简体中文](tmux-6-keys.zh-CN.md)

# Never used tmux? Learn these 6 keys first

Eitri's window keys come from tmux. If you have only used AI plugins inside Neovim, the first launch can be
confusing: there is a chat panel beside the editor, and `Ctrl+b` no longer pages up.

You do not need to read all of [Keys](keys.md) on the first day. These six are enough to open a project, talk to an
agent and see what it changed. Look up the rest when you need it; `Ctrl+b ?` lists every key at any time.

Everything below is the default setup.

## 1. `Ctrl+h` and `Ctrl+l`: move between the editor and the panel

The window has several modules (the editor, the agent panel, a terminal), and your keys go to only one of them at a
time. So the first thing to learn is how to move the keys.

`Ctrl+h` goes left, `Ctrl+l` right, `Ctrl+j` down and `Ctrl+k` up, the same directions as vim's `hjkl`. In the editor
the key first moves between Neovim's own windows and crosses to the next module only at the edge (in Insert mode only
`Ctrl+l` crosses). If you have used vim-tmux-navigator, this will feel familiar.

- If your configuration already maps one of these keys to something else, Eitri leaves it alone. Use `Ctrl+b` and an
  arrow key (`Left`, `Right`, `Up`, `Down`) to move instead.
- In the bottom terminal these keys are Eitri's, so the shell's clear-screen `Ctrl+l` is `Ctrl+b Ctrl+l` there.

## 2. `i` and `Esc`: typing and browsing in the panel

The agent panel has modes, like vim. If you do not know that, it looks as if typing does nothing.

- With the keys in the panel, press `i` to get into the message box (INSERT), write your question and press `Enter`
  to send it. `Shift+Enter` starts a new line.
- `Esc` goes back to BROWSE, where `j` and `k` walk down and up the conversation.
- The left end of the panel's bottom band shows which mode you are in, in the same place and style as the mode in
  your editor's statusline.

If you send a message while the agent is still working, it goes into the running turn and does not stop it. To stop
the turn, press `Ctrl+c`.

To point the agent at some code, select it in the editor in Visual mode, then ask in the panel; the selected lines are
sent with your message.

## 3. `Ctrl+b`, then `?`: the prefix, and where every key is listed

Window-level actions (opening a terminal, splitting, a new chat, switching tabs) sit behind a prefix key. This is
tmux's habit, and it is the one thing you have to get used to.

Press `Ctrl+b`, let go, then press one more key. It is not three keys at once.

- `Ctrl+b ?` lists every key. Press it whenever you forget something.
- If you pressed `Ctrl+b` and changed your mind, `Esc` cancels it.
- In Neovim `Ctrl+b` pages up. Eitri takes it as the prefix, so to page up press it twice: `Ctrl+b Ctrl+b`.
- To have Eitri list the possible next keys when you pause after the prefix, add
  `eitri.config.set("keymap.which_key_delay", 200)` to `~/.config/eitri/init.lua`.

The prefix can be changed; many people like `Ctrl+a`. [Keys](keys.md) says how.

## 4. `Ctrl+b t`: open a terminal

Running tests, looking at git and installing dependencies all need a shell sooner or later.

`Ctrl+b t` shows the bottom terminal and gives it the keys. Press it again while the terminal has the keys and the
terminal is hidden; its shell keeps running.

For a second terminal press `Ctrl+b Alt+Enter`. It opens to the right of the current module, with its shell started in
the project root.

## 5. `Alt-n`, then `1`: answer the agent

When the agent wants to run something that needs your approval, it stops on a card and waits. It waits until you
answer.

- A line above the message box tells you when a card has arrived. Press `Alt-n` to take it (this works from the box
  too).
- Then press a digit to choose a button. `1` approves, and the last digit always denies. On a card that offers
  "Always allow", `2` approves and saves a rule so the same kind of command does not ask again.
- If you do not want to answer yet, `Esc` leaves card mode and the card keeps waiting.

A letter never answers a card, so you cannot answer one by accident while typing.

With several agents open, `Ctrl+b u` goes to the one that has waited for you longest.

## 6. `c`: see what the turn changed

After the agent finishes a turn you will usually want to know which files it touched.

In the panel, press `Esc` to browse, then `c`. It lists the files that changed on disk during that turn, and you can
open them hunk by hunk. When you see a change you do not want:

- `x` puts that hunk back, and `u` undoes that.
- `i` writes a note, and `s` sends your notes to the agent as the next message, after showing you the message.

Nothing opens unless you press `c`. You do not need a git repository: Eitri compares its own snapshots of the
project. [Turn review](turn-review.md) has the details.

## What next

- Type `:EitriTutor` in the editor to practice these keys step by step. It teaches in English or Simplified Chinese,
  whichever your locale asks for.
- To open more chats or split the window, see "Arranging the window" in [Getting started](getting-started.md#arranging-the-window).
- On Hyprland, sway and niri, Eitri leaves splitting to your window manager by default. In a chat window, `Ctrl+h` and
  `Ctrl+l` from key 1 are your window manager's focus moves; see [Compositor windows](compositor-windows.md#keys).
