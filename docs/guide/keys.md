English | [简体中文](keys.zh-CN.md)

# Keys

Eitri's keys come from three places, and they do not collide: a **prefix** for the window itself (tabs,
modules, layout), a few keys that work **everywhere** in the window, and the **agent panel's own keys**,
which are vim's. Everything below is the default setup. `?` in the panel lists the keys of your own setup,
including anything you changed in [your configuration](configuration.md) or took over from tmux.

## The prefix

The prefix is stock tmux's: press `Ctrl+b`, let go, then press one key. It is read before the editor, the
panel and the bottom terminal see anything, so it works wherever the keys are. `Esc` cancels it, and a key the
table does not bind ends the wait without doing anything. To send `Ctrl+b` itself to whatever has the keys,
press it twice. `<prefix> ?` lists every key of the table.

The keys marked "repeats" can be pressed again within half a second without the prefix, as in tmux.

**Tabs.** Each conversation in the agent panel is a tab.

| keys | what they do |
|---|---|
| `<prefix> c` | a new tab |
| `<prefix> n` / `<prefix> p` | the next / the previous tab |
| `<prefix> 1` ... `9` | that tab by number |
| `<prefix> l` | the tab you were on before |
| `<prefix> ,` | rename this tab |
| `<prefix> &` | close this tab, after a `y/n` question |
| `<prefix> w` | choose a session: the open tabs first, then saved ones to resume |
| `<prefix> i` | this session's details |

**Modules.** The window is made of modules: the editor, the agent panel and the bottom terminal.

| keys | what they do |
|---|---|
| `<prefix> e` / `a` / `t` | show and focus the editor / the agent / the terminal, or hide it when it already has the keys |
| `<prefix> Left`, `Right`, `Up`, `Down` | move the keys to the module on that side (repeats) |
| `<prefix> ;` | move the keys back to the module that had them before |
| `<prefix> o` | move the keys to the next module on screen |
| `<prefix> z` | zoom this module, or restore it |
| `<prefix> %` or `<prefix> "`, then `e`, `a` or `t` | open that module to the right of / below this one, or move it there |
| `<prefix> {` / `<prefix> }` | swap this module with the previous / the next one on screen |
| `<prefix> Alt+1` / `Alt+2` | every module in one row / in one column, at equal sizes |
| `<prefix> Ctrl+Up`, `Ctrl+Down`, `Ctrl+Left`, `Ctrl+Right` | move the nearest divider one cell that way (repeats) |
| `<prefix> Alt+Up`, `Alt+Down`, `Alt+Left`, `Alt+Right` | the same, five cells (repeats) |
| `<prefix> x` | close this module and end what runs in it, after a `y/n` question; closing the last one on screen closes Eitri |
| `<prefix> [` or `<prefix> PageUp` | scroll the bottom terminal back (copy mode) |
| `<prefix> F11` | immersive: fullscreen without the top bar |

**The window and the panel.**

| keys | what they do |
|---|---|
| `<prefix> f` | HINT: jump anywhere in the window (see [HINT](#hint)) |
| `<prefix> r` | reload the agent panel; the session keeps running |
| `<prefix> ?` | the full list of keys, in the panel |
| `<prefix> :` | the panel's `:` command line (see [The command line](#the-command-line)) |
| `<prefix> Ctrl+h`, `Ctrl+j`, `Ctrl+k`, `Ctrl+l` | send that chord itself to whatever has the keys; the bare chords below are Eitri's |

**Without the prefix**, these work anywhere in the window:

| keys | what they do |
|---|---|
| `F11` | fullscreen |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | text size larger / smaller / reset, for the editor and the panel together (the keypad's `+`, `-` and `0` work too) |
| `Ctrl+wheel` | text size of the pane under the pointer only |

A companion window (see [Companion mode](companion.md)) has no layout of its own, so there the prefix keeps
the tab keys, `r`, `?`, `:`, `f`, sending the prefix itself and the arrow keys, which move toward the
neighbouring window; every layout key answers `not in a companion window`.

## Moving between panes

`Ctrl+h`, `Ctrl+j`, `Ctrl+k` and `Ctrl+l` move the keys to the module on the left, below, above and right,
the way vim-tmux-navigator does. They have no prefix, and they are Eitri's in every module, the bottom
terminal included (the terminal never receives them; `<prefix> Ctrl+l` sends the chord to it if a program
needs it).

**From the editor.** Neovim goes first: the key moves between Neovim's own windows, and only when there is no
window left that way does it cross into the neighbouring module. This works with vim-tmux-navigator
installed. Without it Eitri gives these four keys the same behaviour itself, in Normal and Visual mode (in
Insert mode only `Ctrl+l`, since `Ctrl+h`, `Ctrl+j` and `Ctrl+k` keep their Insert meanings), but only on a key
your configuration has not mapped to something else: a mapping of your own is left alone.

**From the agent panel.** `Ctrl+h` and `Ctrl+l` leave the panel to the module on that side. `Ctrl+k` and
`Ctrl+j` depend on the panel's mode (below): `Ctrl+j` in BROWSE starts typing, in the box (INPUT) it leaves
the panel to the module below; `Ctrl+k` in INPUT goes back to BROWSE, and in BROWSE it leaves the panel to
the module above. From the topmost module, `Ctrl+k` goes on to the top bar: `h` and `l` move along it,
`Enter` activates an item, and `Ctrl+j` or `Esc` return the keys to where they were.

In BROWSE, vim's `Ctrl+w h`, `Ctrl+w j`, `Ctrl+w k` and `Ctrl+w l` do the same moves; `Ctrl+w j` is always the
module below, never the box.

## The panel's modes

The panel has two modes, shown at the left of its bottom band.

- **INPUT** is the message box. Typing goes into it.
- **BROWSE** is the conversation. The keys are vim's: you move over rows, fold and copy them, and answer
  cards.

`Esc` leaves INPUT for BROWSE. `i` or `o` (the caret where you left it), `A` (at the end of the draft) or
`Ctrl+j` go from BROWSE to INPUT. `i`, `o`, `A` and `f` act only on a key that stands alone: if another key
is pressed within about a quarter of a second of it, it does nothing, so the first letters of a sentence typed
right after a reply arrives do not start HINT or open the box in the middle of a word.

### In INPUT

| keys | what they do |
|---|---|
| `Enter` | send; while a turn runs, queue the message for when it ends |
| `Ctrl+Enter` | send now: interrupts a running turn, then sends the queue and this message |
| `Shift+Enter`, `Alt+Enter`, or a backslash then `Enter` | a new line (the backslash is replaced by it) |
| `Up` / `Down` | from the first / the last line: take the queue back, then step through earlier prompts |
| `Ctrl+r` | search earlier prompts; `Enter` puts one in the box |
| `Ctrl+w` / `Ctrl+u` | delete a word / to the start of the line |
| `Ctrl+c` | interrupt a running turn; when idle, clear the box into the history |
| `Ctrl+y` | approve the oldest card waiting in this tab (see [Answering a card](#answering-a-card)) |
| `Ctrl+g` | edit the draft in Neovim; `:wq` brings it back, `:q!` changes nothing |
| `Ctrl+o` | the detailed view (also in BROWSE) |
| `Shift+Tab` | switch between Auto and Bypass; entering Bypass asks first |
| `Esc` | stop typing and go back to BROWSE |
| `?` | the list of keys, from an empty box |

A message that starts with `/` is a Claude Code slash command. Some work (`/clear`, `/compact`, `/cost`,
`/context`, `/usage`, `/model`, `/mcp`, `/agents`, `/doctor` and `/output-style` are the ones that show an
effect), a bare `/model` or `/effort` opens a picker, and a few that need a full terminal (`/login`,
`/config` and `/resume`) are refused with a message. The rest are sent as ordinary text, which Claude answers
as words rather than as a command. `?` lists the ones that work.

### In BROWSE

| keys | what they do |
|---|---|
| `j` / `k`, or `Down` / `Up` | the next / the previous row; a long row scrolls first |
| `h` / `l` | the previous / the next button in the row |
| `gg` / `G` | the first / the last row |
| `[[` / `]]` | the previous / the next prompt of yours |
| `]p` / `[p` | the next / the previous card waiting for an answer (wraps; moves the cursor, answers nothing) |
| `Ctrl+d` / `Ctrl+u` | half a page down / up |
| `Ctrl+e` / `Ctrl+y` | one line down / up |
| `Ctrl+f` or `PageDown` / `PageUp` | a view down / up (`Ctrl+b` is the prefix, so `PageUp` is the way back) |
| `zt` / `zz` / `zb` | this row to the top / the middle / the bottom of the view |
| `Enter`, or `za` | show or hide a tool's result, or a collapsed run of calls |
| `zo` / `zc` | open / close that fold only |
| `zh` / `zl` | scroll this row's table left / right |
| `Ctrl+o` | the detailed view: every result, longer excerpts, no collapsed runs |
| `y` | copy the row (the message, command or path), or the code block HINT landed on |
| `Y` | copy a tool row's whole output |
| `Ctrl+g` | open this row's whole text in a scratch buffer in Neovim |
| `gf` | open the file path on this row in the editor (several: pick by letter) |
| `gx` | open this row's web link in your browser (several: pick by letter) |
| `/` | search the conversation; `Enter` keeps the match, `Esc` goes back |
| `n` / `N` | the next / the previous match, wrapping |
| `c` | review what changed on disk during this tab's latest turn (see [Turn review](turn-review.md)) |
| `Ctrl+c` | interrupt the running turn (`Esc` never does) |
| `r` | start a new session in this tab, once its session has ended |
| `f` | HINT |
| `:` | the command line |
| `?` | the list of keys; `?`, `Esc` or `q` closes it |

A count goes before many of these: `3j` is three rows, `3G` or `3gg` is row 3, `2]]` is two prompts, `2gt`
is tab 2, `5 Ctrl+e` is five lines, and `]p` and `[p` go that many cards on. `0` continues a count but does
not start one.

A key after `g`, `z`, `[`, `]` or `Ctrl+w` that completes none of the pairs above ends the sequence without
doing anything, so `g` followed by `d` cannot deny a card.

### Tabs and the leader

`<leader>` is `Space`, unless your Neovim configuration sets `mapleader` to a single key the panel can use.
After it, a short list of what can follow appears. These keys work in BROWSE.

| keys | what they do |
|---|---|
| `H` / `L`, `[b` / `]b`, `gT` / `gt` | the previous / the next tab (`2gt` is tab 2) |
| `<leader>b b` | the tab you were on before |
| `<leader>b d` | close this tab, after a `y/n` question |
| `<leader>b o` | close every other tab, after a `y/n` question |
| `<leader>f n` | a new tab |
| `<leader>,` | switch tab |
| `<leader>/` | search the conversation |
| `<leader>i` | this tab's details |
| `<leader>t` | continue this conversation in a terminal |
| `<leader>m` | switch between Auto and Bypass (as `Shift+Tab`) |
| `<leader>?` | the list of keys |

You can change them in [your configuration](configuration.md).

### Selecting text

`v` in BROWSE starts a caret on the row under the cursor, `V` starts a line selection directly.

- In the caret: `h` `l` move by character, `j` `k` by screen line, `w` `b` `e` by word, `0` and `$` to the start
  and end of the line, `gg` and `G` to the first and last character of the conversation; a count goes in front of
  a motion. `v` or `V` starts a selection from there, and `Esc` goes back to BROWSE.
- In VISUAL (`v`, by character) and V-LINE (`V`, by line): the same motions extend the selection, `o` swaps which
  end moves, `v` and `V` switch between the two modes (a mode's own key goes back to the caret), `y` copies what
  is highlighted and returns to BROWSE, `>` quotes it into your message and starts typing, and `Esc` goes back
  to the caret.
- `Ctrl+e` and `Ctrl+y` scroll one line without moving the caret or the selection, and `Ctrl+c` still
  interrupts a running turn. Any other plain key ends the selection with a note naming it; other chords with
  `Ctrl` or `Alt` are ignored.

## Answering a card

A permission card waits for you in the conversation, next to the tool call it gates. In BROWSE:

| keys | what they do |
|---|---|
| `a` | approve the card under the cursor, or the card that gates the tool call under it |
| `d` | deny it |
| `D` | deny with a reason: the keys go to the card's reason box, and `Enter` there sends the denial |
| `h` / `l`, then `Enter` | walk to one of the card's buttons and press it |
| `]p` / `[p` | move the cursor to the next / the previous waiting card |

From INPUT, `Ctrl+y` approves the oldest card waiting in this tab, and the band names the card it would
approve. There is no key that denies from the box, since a denial can carry a reason you want to type.

These keys answer only when they stand alone. `a`, `d`, `D` and `Enter` on a button do nothing if another key
was pressed within about a quarter of a second before or after; `Ctrl+y` is refused in the same way, right
after a readline kill key (`Ctrl+u`, `Ctrl+w`, `Ctrl+k`), and for a quarter of a second after the card it names
changed. The band says why when a key was refused. None of them takes a count, a held key's repeat never
answers, and a key with `Ctrl`, `Alt`, `Super` or `AltGr` held does not answer either (`D` is typed with
`Shift`). `Enter` on a button answers only right after `h`, `l` or `Tab` walked the focus onto it, or a HINT
landed on it. A card of a session that has ended cannot be answered.

## HINT

`f` in BROWSE, or `<prefix> f` from anywhere, labels everything you can jump to: the top bar's items, the
editor, the terminal when it is shown, and in the panel every visible row, code block, link, button and the
message box. Type a label's letters (they come from `asdjklghweruio`) to go there.
`Backspace` deletes a letter and `Esc` cancels. Landing on a row moves the cursor to it; landing on a link
focuses it and `Enter` opens it, and landing on a code block lets `y` copy it. Pressing `f` again does
nothing, since `f` is never a label.

## The command line

`:` in BROWSE (or `<prefix> :` from anywhere) opens a command line at the bottom of the panel. It is not
vim's: it takes two commands, so that `:ls` or `:d` typed from habit land here and never on a card. `Enter`
runs it and `Esc` closes it; anything else shows a note saying there are no other commands.

| command | what it does |
|---|---|
| `:trust` | check this project's own Claude Code configuration (its `.claude/`, `.mcp.json` and `CLAUDE.md`) and ask whether to trust it; sessions started afterwards load it. It says so when there is nothing to trust or it is already trusted |
| `:untrust` | forget that trust; sessions started afterwards leave the project's configuration out. Running sessions are unchanged |

What trusting means is in [Permissions](permissions.md).

## Your tmux keys

If you use tmux, Eitri picks up your prefix and your prefix-table bindings from the same files tmux
reads (`/etc/tmux.conf`, `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`,
`~/.config/tmux/tmux.conf`, and what they `source-file`), wherever an Eitri action matches the tmux
command. It only reads them; it never starts or asks tmux. A notice says what was taken (it appears
once for each change to what is imported), and `<prefix> ?` lists each line that was not, with the reason.
Things Eitri has no equivalent of, and anything that would need tmux to run something, are skipped.

An `unbind -a` in your tmux configuration removes Eitri's tmux-like defaults too, but Eitri's own keys (HINT,
the module keys, `?`, the panel reload, immersive and the tab keys tmux has no command for) stay.

To turn the import off, put `eitri.config.set("keymap.from_tmux", "off")` in `~/.config/eitri/init.lua`
(`"on"` is the default; any other value stops the launch, naming the key). Your own `eitri.keymap` calls in
that file always win over what came from tmux; for instance, with the import off, a `Ctrl+a` prefix is
`eitri.keymap.prefix("C-a")`. The prefix must be a `Ctrl` or `Alt` chord or a function key. The calls
themselves are in [Configuration](configuration.md).
