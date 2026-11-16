English | [简体中文](takeover.zh-CN.md)

# Take over a terminal nvim

If you have a Neovim running in a terminal, with files open, unsaved changes, a job running or a `:terminal` in it,
Eitri can move that very Neovim into a window of its own, and give it back to the terminal when the window ends.
Nothing is restarted. The Neovim you took over is the editor in the window, with the agent panel beside it and the
bottom terminal under it, and the terminal it came from waits at a small screen until it is returned. If you work in tmux today,
[Coming from tmux](migrate-from-tmux.md) covers the whole move, this way of working included.

## The panel beside nvim, or nvim moved in

Both put an agent next to the Neovim you already use, and they suit different days.

- **[The panel beside nvim](companion.md)** leaves your Neovim in your terminal, on screen, exactly where it is. The
  panel is a second window that attaches to it. Use it when you like your terminal, your tmux or your window manager
  to arrange things.
- **Moving nvim in** puts your Neovim inside Eitri's window, as its editor, drawn by Eitri's renderer (smooth
  scrolling, float effects, the window's grids), next to the panel and the terminal in one tiled window. Use it when
  you want the whole window, and you want the Neovim you are already in rather than a new one.

You can switch back and forth: ending the window returns your Neovim to the terminal, and you can run `:EitriOpen`
again.

## Starting it

Type `:EitriOpen` in the Neovim you want to move. It needs **Neovim 0.12 or later** on that Neovim; below that it prints
one line and nothing of the takeover loads. Nothing else in Eitri is affected, and the editor Eitri starts for itself
keeps its own, lower floor.

```vim
:EitriOpen
:EitriOpen ~/some/project
```

`:EitriOpen` takes an optional project directory. Without one, the project is the Neovim's own working directory
(`getcwd(-1, -1)`, as `:pwd` shows it for the whole session); a later `:cd` does not move the window. The same thing is
`eitri open --nvim <address> [DIR]` from a shell, where `--nvim` is the Neovim's server address (`:echo v:servername`).
`eitri open` always opens a window of its own, and never reads `$NVIM`.

Install [eitri.nvim](companion.md#the-eitrinvim-plugin) for the command. With lazy.nvim, add `"EitriOpen"` to the
spec's `cmd` list beside `"EitriPanel"` and `"EitriPage"`.

The takeover is refused, with one line shown by `:EitriOpen` and the terminal left exactly as it was, when:

- the Neovim is older than 0.12;
- it shows more than one UI, or a UI that is not a terminal (a Neovide window, nvim-qt, another `--remote-ui`
  client);
- it is waiting for a key, or too busy to answer in half a second (press `Esc` and try again);
- it is already open in an Eitri window;
- `editor.takeover` is off in your `init.lua` (see [Turning it off](#turning-it-off));
- there is no display to open a window on, or the server address is not a socket of yours (remote servers over SSH or
  TCP are not supported).

In a script, `eitri open` exits 0 after the window has closed, 64 for a refusal like these before anything moved, and
1 when something failed after the terminal's UI had moved (the terminal then gets its Neovim back).

## What you see

The terminal shows:

```
This nvim is open in Eitri. It comes back here when Eitri lets go of it. <Enter> takes it back now.
If the screen is blank when it comes back, nvim is waiting for a key: press Esc.
```

The window's title and top bar read `<project> (from terminal)`, so you can tell this window from an Eitri you started
yourself. It has everything a window has: the agent panel with its tabs, turn review and permissions, the bottom
terminal, floats, workspaces. `eitri FILE` typed in that terminal opens the file in this Neovim. `<leader>ae` in Visual
mode sends the selection to the panel, as it does in the [panel beside nvim](companion.md). The editor does not reopen
files from the last time or write a session record: it is your Neovim, with the session you gave it.

## How nvim comes back

However the window ends, the same Neovim is back in the terminal, with every buffer, job and `:terminal` it had:

- you close the window, or `prefix x` on the editor (it asks first: `give nvim back to the terminal and close this
  window? (y/n)`), or Eitri quits, or the last module is closed;
- you press `<Enter>` in the terminal's waiting screen;
- the session ends with a logout or a `kill` (Eitri gives the Neovim back, then exits), or Eitri crashes or is killed
  (the terminal notices within about a second and takes its Neovim back);
- another UI attaches to the Neovim (a manual `nvim --remote-ui`): Eitri gives the Neovim back and closes, and the other
  UI stays;
- the attach fails after the terminal's UI moved: Eitri exits with one line and the terminal gets its Neovim back.

Closing the window asks only what it has to:

- if the editor holds a draft with unsaved changes (the panel's `Ctrl+g`) or agent proposals that are open, it says so
  (`the draft in the editor has unsaved changes`, `<N> agent proposals are open`) and asks `close anyway? (y/n)`,
  because those belong to the window and end with it;
- if the Neovim is waiting for a key, or busy, it asks `nvim is waiting for a key (or busy): press Esc in the editor, or
  close anyway? (y/n)`;
- questions about agent turns that are still running come after the Neovim has been given back, so the terminal never
  waits on them.

**Eitri never signals your Neovim and never quits it on its own.** Giving it back is a detach: Eitri takes out
everything it added and the terminal's UI returns. Typing `:qa` or `ZZ` in the window is your own quit: Neovim exits,
the window closes and the terminal is at its shell prompt, as if you had quit in the terminal.

### The blank screen

If the Neovim was waiting for a key when it was given back (you had pressed `f`, or a prompt was open), the terminal
shows a blank screen with Neovim's intro text until the first key. That key goes to Neovim, so press `Esc`. The close
question above exists to warn you of this; Eitri does not send a key to clear it, because that key would answer
whatever Neovim was asking.

## If the terminal was closed

If you closed the terminal while its Neovim was in Eitri, Neovim keeps running in Eitri with every buffer. The window
shows a band, `the terminal this nvim came from is closed`, and at close it asks:

```
closing leaves nvim running with no window. To see it again: nvim --remote-ui --server <address>. Close? (y/n)
```

After `y` the Neovim keeps running without a window, with its unsaved buffers and swap files, and the command shown
brings it back in a new terminal. The same holds if the waiting room Eitri keeps for your terminal was killed
from outside (`the terminal's way back is gone`). If you would rather end that Neovim, do it from a terminal with the
command shown and `:qa`.

## What Eitri puts into your Neovim, and takes out

While the window has your Neovim, Eitri adds a few things to it, and removes all of them again when the Neovim goes
back to the terminal: the commands and keys of the window (`:Eitri`, `<leader>ae`, and the keys under
[`keymap.nvim_prefix`](configuration.md#settings) when you set it), the agent's editor tools and review marks, a timer that reloads buffers changed on disk, and the editor's own settings (the cursor outline, for one).
Files you open, edits you make and files the agent edits at your request stay, as they would in the terminal. Buffers
that only an agent's read tool loaded, and no window shows, are unloaded again. Nothing is written to your Neovim
configuration, and `:Eitri` is never defined over a command of yours. `:Eitri` there completes the words of your
`init.lua` keymap, but not the keys a plugin's page sets when it loads, and `:EitriTutor` does not run in a Neovim you
took over.

If you have your own `:Eitri`, `<leader>ae` or `WatchGlobal`, they survive the takeover and its teardown unchanged.

## Your configuration

Your Neovim started in a terminal, so what your `init.lua` does for a graphical front end never ran. A branch such as
`if vim.g.neovide then ... end` is false at start-up, and there is no `ginit.vim`: Eitri does not source one in a Neovim
it attaches to, because its effects cannot be told from yours and cannot be undone, and they would follow the Neovim
back to the terminal (a `termguicolors` set there changes the terminal's colours).

To adjust the Neovim while it is in Eitri, use a `UIEnter` autocommand that checks which UI arrived, and put things
back in `UILeave`:

```lua
vim.api.nvim_create_autocmd("UIEnter", {
  callback = function()
    local info = vim.api.nvim_get_chan_info(vim.v.event.chan)
    if info.client and info.client.name == "neovide" then
      -- settings for the Eitri window only
    end
  end,
})
```

Anything you set this way is yours to undo; Eitri puts back only what it set.

## The clipboard, tmux plugins and smart-splits

- **Clipboard.** Your Neovim keeps its own clipboard provider. A `tmux` provider reaches Eitri's clipboard while the
  window has the Neovim, and real tmux again afterwards. `wl-copy` and `xclip` keep working. OSC 52 writes to a
  terminal, and the window's UI has none.
- **Inside tmux**, the terminal's tmux pane is hidden behind Eitri, so a move that goes to tmux would change nothing
  you can see. While the Neovim is in the window, Eitri points `TMUX`, `TMUX_PANE` and the front of `PATH` at its own
  stand-in, so vim-tmux-navigator's `Ctrl+h/j/k/l` at the Neovim's edge moves Eitri's focus between modules. Put back
  afterwards. Any other plugin that runs `tmux` (vimux, vim-slime) reaches that stand-in and not your tmux while the
  window has the Neovim, and a `:terminal` started in that time keeps the changed variables, so its `tmux` calls fail
  until a new shell starts there.
- **smart-splits.nvim** picks its multiplexer once, when it starts, and keeps real tmux. Use its `at_edge` hook with
  `require("eitri").edge`, as in [Navigator plugins and tmux](companion.md#navigator-plugins-and-tmux).

`:EitriPanel` in a Neovim that is open in an Eitri window says `this nvim is open in an Eitri window`, and `eitri page`
says its pages open there. A page window already beside the Neovim closes when the takeover starts.

## `:restart` and `:connect`

Typed in a window you took over, `:connect <address>` gives your Neovim back to the terminal and the window closes (the
editor's UI went to that other server, which is not Eitri's to follow). `:restart` ends like `:qa` does for now: the
window closes, the terminal is at its shell prompt, and the new server runs with no window. Its address is in the
takeover's log, the newest `takeover-<16 hex>.log` in `~/.local/state/eitri/companion/` (under `$XDG_STATE_HOME` if you
set it), on the line `nvim restarted at <address>: nvim --remote-ui --server <address>`; run that command in a
terminal to see the Neovim again, before a new `nvim` on the same files meets its swap files.

In the editor Eitri starts itself, both are followed: `:restart` moves the editor to the new Neovim, and `:connect`
moves it to the server you named, leaving the editor's own Neovim running. From then on the window treats that server
as one it did not start: `prefix x` on the editor asks `let go of the nvim :connect reached and close this window?
(y/n)`, since the editor cannot go back to its own Neovim. Closing the window, or the server you reached ending
(`:qa` typed there), asks the `closing leaves nvim running with no window` question above with the address of the
editor's own Neovim; a close that cannot ask (a logout, a `kill`) writes that line to the window's log.

## Turning it off

`eitri.config.set("editor.takeover", false)` in your `init.lua` refuses every takeover: `:EitriOpen` then says
`takeover is off (editor.takeover in init.lua)` and the terminal never leaves. The default is on, but nothing happens
unless you run `:EitriOpen`. See [Configuration](configuration.md#settings).

## What is not covered

- one Neovim shown in the terminal and in Eitri at the same time;
- a Neovim on another machine (SSH or TCP), and one whose UI is a graphical front end;
- Neovim older than 0.12;
- the Mac.

The first rough edges are listed in [known issues](../known-issues.md#companion-mode).
